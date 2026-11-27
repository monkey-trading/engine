//! A carry's size does not grow with the source's stakers, and its entitlements fit the file's bound: a source with
//! tens of thousands of one-unit stakers carries in a file the size of one with none, whose genesis mirrors them
//! again; and a carry of entitlements past what a log's first segment once held is built, read and sequenced over
//! segments a member fetches whole, with the bound fitting some 700,000 of them, in a range an archive reader takes.

use std::path::Path;

use alloy::primitives::B256;
use engine_core::carry::{CarriedBalance, MAX_CARRY_ENTRIES};
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::AccountId;
use tokio::sync::watch;

use super::*;
use crate::log::{CommandEntry, Manifest};
use crate::test_support::scratch_dir;

/// The node's reference log whose last checkpoint is a source that carries.
const CARRIED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");
/// One-unit stakers enough to have put the stake parts past what a log's first segment held for its members.
const DUST_STAKERS: u32 = 50_000;
/// The siblings a proof takes in a state tree of a million leaves or so, in which 700,000 entitlements sit.
const DEEP_PROOF: usize = 20;
/// What a command's seal, its domain, signer and signature, and its `admin_applied` event add to its log line, with
/// room.
const SEAL_AND_EVENTS: u64 = 512;
/// The seqs one range of the archive holds outside tests (`archive::RANGE_SEQS`).
const LAUNCH_RANGE_SEQS: u64 = 10_000;
/// The bytes launch flow fills a range with, about 5.5 MB, with room.
const LAUNCH_RANGE_FLOW: u64 = 8 << 20;

/// The reference source after `stakers` more addresses each staked one base unit of MONKEY, mirrored as the node
/// mirrors `Staked` events, with the mirror's scan completed past them; its root is computed.
fn dusted(stakers: u32) -> Engine {
    let path = Path::new(CARRIED);
    let seq = last_checkpoint(path).unwrap().expect("the reference log holds a checkpoint");
    let mut engine = crate::exit::engine_at_checkpoint(path, seq).unwrap();
    let frontier = engine.staking().and_then(|staking| staking.completed_through).expect("a completed frontier");
    let (ts, block) = (engine.now(), frontier.block + 1);
    let mut next = seq + 1;
    for n in 0..stakers {
        let mut account = [0xd0u8; 20];
        account[16..].copy_from_slice(&n.to_be_bytes());
        let cmd = Command::Stake {
            account: AccountId(account),
            amount: TokenUnits(1),
            block,
            chain_ref: format!("0xdust:{n}"),
        };
        let events = engine.apply(&Sequenced { seq: next, ts, cmd }).expect("a mirrored stake applies");
        assert!(matches!(events.last(), Some(Event::Staked { .. })), "{events:?}");
        next += 1;
    }
    let block_hash = B256::repeat_byte(0xd1);
    let cmd = Command::StakingProgress { block, block_hash, block_time: None, named_turn: None };
    let events = engine.apply(&Sequenced { seq: next, ts, cmd }).expect("the frontier applies");
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
    engine.state_root();
    engine
}

/// The carry file of `engine`'s state, under this build's protocol.
fn carry_of(engine: &Engine) -> CarryFile {
    let (parts, totals) = engine.carry_parts(ProtocolVersion::LATEST).expect("the state carries");
    CarryFile { source: totals.source, totals, parts }
}

#[test]
fn a_source_with_any_number_of_stakers_carries_in_a_file_its_stakers_do_not_grow() {
    let (none, dust) = (dusted(0), dusted(DUST_STAKERS));
    let (bare, dusty) = (carry_of(&none), carry_of(&dust));
    let staked = none.staking().and_then(|staking| staking.staked_total).expect("a total").0;
    assert_eq!(dusty.totals.staked.0, staked + u128::from(DUST_STAKERS));
    assert_eq!(dusty.totals.accounts, bare.totals.accounts, "the entitled accounts alone are carried");
    assert!(!dusty.parts.iter().any(|part| matches!(part, CarryPart::Stakes { .. })), "no part carries a stake");

    // The file grows only as the tree's depth lengthens its other entries' proofs, by under a byte a staker, where
    // a carried stake took over a thousand.
    let (bare_bytes, dusty_bytes) = (encode(&bare).unwrap().len(), encode(&dusty).unwrap().len());
    let growth = dusty_bytes.saturating_sub(bare_bytes);
    assert!(growth < DUST_STAKERS as usize, "{DUST_STAKERS} stakers grow the carry from {bare_bytes} to {dusty_bytes}");
    let dir = scratch_dir("carry-dust");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("carry.json");
    write(&dusty, &path).unwrap();
    assert_eq!(read(&path).unwrap().0, dusty);

    // The genesis leaves every stake to mirror again, to the source's frontier and total.
    let engine = super::tests::pre_run(&dusty.parts);
    let theirs = dust.staking().expect("the source staked");
    let remirror = engine.staking().and_then(|staking| staking.remirror).expect("the stakes are mirrored again");
    assert_eq!((remirror.through, remirror.staked_total), (theirs.completed_through.unwrap(), dusty.totals.staked));
    assert!(engine.staked_balances().is_empty());
}

#[test]
fn entitlements_past_what_a_first_segment_held_carry_over_segments_a_member_fetches() {
    let file = carry_of(&dusted(0));
    let entry = |n: u32| {
        let mut account = [0xe0u8; 20];
        account[16..].copy_from_slice(&n.to_be_bytes());
        CarriedBalance {
            account: AccountId(account),
            amount: TokenUnits(1),
            siblings: vec![B256::repeat_byte(7); DEEP_PROOF],
        }
    };
    // About 74 MB of entitlements, past the 48 MiB the carry was bounded at while its genesis sat in one segment.
    let entitlements: Vec<CarriedBalance> = (0..50_000).map(entry).collect();
    let mut parts = vec![file.parts[0].clone()];
    parts.extend(
        entitlements.chunks(MAX_CARRY_ENTRIES).map(|chunk| CarryPart::Entitlements { entries: chunk.to_vec() }),
    );
    parts.push(CarryPart::End);
    let totals = carried_by(file.source, &parts).expect("the sums fit");
    assert_eq!(totals.accounts, 50_000);
    let large = CarryFile { source: file.source, totals, parts };

    let bytes = encode(&large).unwrap();
    let old_bound = crate::standby::MAX_ARCHIVE_BYTES / 4 * 3;
    assert!(bytes.len() as u64 > old_bound, "{} bytes", bytes.len());
    // The bound fits some 700,000 entitlements with proofs this deep.
    let per_entry = bytes.len() as u64 / 50_000;
    assert!(MAX_FILE_BYTES / per_entry >= 700_000, "{per_entry} bytes an entitlement");
    let dir = scratch_dir("carry-entitlements");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("carry.json");
    write(&large, &path).unwrap();
    assert_eq!(read(&path).unwrap().0.totals, totals);

    // Sequenced as a genesis, at a log's usual commands a segment, the parts fill segments a member fetches whole.
    let log_dir = dir.join("log");
    let manifest = watch::channel(Manifest::default()).0;
    let mut writer = SegmentedLog::open(&log_dir, |_| Ok(())).unwrap().into_writer(10_000, manifest).unwrap();
    for (seq, part) in (0..).zip(&large.parts) {
        let cmd = Command::Admin(AdminCommand::CarryState(part.clone()));
        let entry = CommandEntry { sequenced: Sequenced { seq, ts: 0, cmd }, events: None, seal: None };
        writer.append(&LogEntry::Command(entry)).unwrap();
    }
    writer.flush().unwrap();
    let listed = Manifest::load(&log_dir).unwrap();
    assert!(listed.segments.len() >= 2, "{:?}", listed.segments);
    assert!(listed.segments.iter().all(|segment| segment.bytes <= crate::standby::MAX_ARCHIVE_BYTES), "{listed:?}");

    // The archive cuts the log into ranges of a count of seqs, however many bytes they hold, so one range can hold
    // the whole genesis: the file's parts, each framed as a log line, with its seal and events, in under a kibibyte.
    // A reader takes a range of the largest carry beside a range of launch flow.
    let lines: u64 = listed.segments.iter().map(|segment| segment.bytes).sum();
    let framing = lines.saturating_sub(bytes.len() as u64) / large.parts.len() as u64 + SEAL_AND_EVENTS;
    assert!(framing < 1024, "{framing} bytes of framing a part");
    const _: () = assert!(crate::archive::RANGE_BYTES >= MAX_FILE_BYTES + LAUNCH_RANGE_SEQS * 1024 + LAUNCH_RANGE_FLOW);
}
