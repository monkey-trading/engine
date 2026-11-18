use engine_core::merkle::{DistributionProof, ExitTerms};
use engine_core::staking::UsdcUnits;

use super::*;

/// The node's reference log for the latest protocol: placeholder checkpoint signatures, and
/// signed withdrawals, which `engine replay` refuses without a trusted signer.
const REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v38.jsonl");

#[test]
fn a_staker_with_no_trading_account_is_given_its_claim_call() {
    let staker = AccountId([5; 20]);
    let terms = ExitTermsProof { seq: 9, root: B256::ZERO, terms: ExitTerms::default(), siblings: vec![] };
    let distribution =
        DistributionProof { seq: 9, root: B256::ZERO, account: staker, entitled: UsdcUnits(7), siblings: vec![] };
    let proof = ExitProof {
        seq: 9,
        root: B256::ZERO,
        terms,
        account: None,
        approvals: vec![],
        queued: vec![],
        distribution: Some(distribution),
        pending_deposits: vec![],
        unfinalised_credits: vec![],
    };
    let calls = account_calls(staker, &proof);
    let selectors: Vec<[u8; 4]> = calls.iter().map(|call| call.calldata[..4].try_into().unwrap()).collect();
    assert_eq!(selectors, [startExitCall::SELECTOR, registerExitDistributionCall::SELECTOR, claimExitCall::SELECTOR]);
    let claim = claimExitCall::abi_decode(&calls[2].calldata).unwrap();
    assert_eq!(claim.account, address(staker));
    let none = ExitProof { distribution: None, ..proof };
    assert_eq!(account_calls(staker, &none).len(), 1, "nothing to register, nothing to claim");
}

#[test]
fn an_accounts_unfinalised_credits_follow_its_register_call_in_bounded_batches() {
    let account = AccountId([6; 20]);
    let terms = ExitTermsProof { seq: 9, root: B256::ZERO, terms: ExitTerms::default(), siblings: vec![] };
    let leaf = engine_core::merkle::AccountLeaf {
        balance: Fixed::from_int(400),
        unfinalised_deposits: Fixed::from_int(400),
        withdrawal_nonce: 0,
        toxicity_bps: 0,
        open_positions: 0,
        toxicity_fraction: Fixed::ZERO,
        toxicity_history: Fixed::ZERO,
    };
    let held = AccountProof {
        seq: 9,
        root: B256::ZERO,
        account,
        leaf,
        siblings: vec![B256::repeat_byte(1)],
        positions: vec![],
        markets: vec![],
    };
    let credit = |deposit_id: u64| engine_core::merkle::UnfinalisedCreditProof {
        seq: 9,
        root: B256::ZERO,
        chain_ref: format!("ref:{deposit_id}"),
        deposit: PendingReceipt {
            deposit_id,
            account: Some(account),
            payer: Some(AccountId([7; 20])),
            amount: Fixed::from_int(10),
        },
        siblings: vec![B256::repeat_byte(2)],
    };
    let proof = ExitProof {
        seq: 9,
        root: B256::ZERO,
        terms,
        account: Some(held),
        approvals: vec![],
        queued: vec![],
        distribution: None,
        pending_deposits: vec![],
        unfinalised_credits: (0..40).map(credit).collect(),
    };
    let calls = account_calls(account, &proof);
    let selectors: Vec<[u8; 4]> = calls.iter().map(|call| call.calldata[..4].try_into().unwrap()).collect();
    assert_eq!(
        selectors,
        [
            startExitCall::SELECTOR,
            registerExitCall::SELECTOR,
            registerExitCreditsCall::SELECTOR,
            registerExitCreditsCall::SELECTOR,
            claimExitCall::SELECTOR
        ]
    );
    let register = registerExitCall::abi_decode(&calls[1].calldata).unwrap();
    assert_eq!(register.state.unfinalisedDeposits, Fixed::from_int(400).raw());
    let batches: Vec<registerExitCreditsCall> =
        calls[2..4].iter().map(|call| registerExitCreditsCall::abi_decode(&call.calldata).unwrap()).collect();
    let ids: Vec<u64> = batches.iter().flat_map(|batch| batch.credits.iter().map(|c| c.depositId)).collect();
    assert_eq!(ids, (0..40).collect::<Vec<_>>(), "every credit once, in order");
    assert_eq!(batches.iter().map(|batch| batch.credits.len()).collect::<Vec<_>>(), [CREDITS_PER_CALL, 8]);
    let first = &batches[0].credits[0];
    assert_eq!(
        (first.chainRef.clone(), first.account, first.payer, first.amount, first.siblings.clone()),
        (
            Bytes::from_static(b"ref:0"),
            address(account),
            Address::repeat_byte(7),
            Fixed::from_int(10).raw(),
            vec![B256::repeat_byte(2)]
        )
    );
    assert!(batches.iter().all(|batch| batch.account == address(account)));
}

#[test]
fn the_receipt_at_a_deposit_id_is_given_its_refund_call_and_an_empty_id_is_refused() {
    let mut checkpoints = Vec::new();
    log::read_file(Path::new(REFERENCE), |entry| {
        if let LogEntry::Checkpoint(cp) = entry {
            checkpoints.push(cp.seq);
        }
        Ok(())
    })
    .unwrap();
    let seq = *checkpoints.last().expect("the reference log checkpoints");
    let house = exit_bundle(Path::new(REFERENCE), seq, ExitSubject::House).unwrap();
    let pending = house.house_pending_deposits.first().expect("the V38 session leaves a house deposit pending");
    let id = pending.deposit.deposit_id;
    let bundle = exit_bundle(Path::new(REFERENCE), seq, ExitSubject::Deposit(id)).unwrap();
    let receipt = bundle.receipt.as_ref().expect("the pending receipt");
    assert_eq!(
        (receipt.kind, &receipt.chain_ref, receipt.deposit, &receipt.siblings),
        (ReceiptKind::Pending, &pending.chain_ref, pending.deposit, &pending.siblings)
    );
    assert!(receipt.verify(bundle.root));
    let selectors: Vec<[u8; 4]> = bundle.calls.iter().map(|call| call.calldata[..4].try_into().unwrap()).collect();
    assert_eq!(selectors, [startExitCall::SELECTOR, refundReceiptCall::SELECTOR]);
    let refund = refundReceiptCall::abi_decode(&bundle.calls[1].calldata).unwrap();
    assert_eq!(refund.kind, 34, "a pending deposit's leaf");
    assert_eq!((refund.leaf.depositId, refund.leaf.amount), (id, pending.deposit.amount.raw()));
    assert_eq!(refund.leaf.chainRef, Bytes::copy_from_slice(pending.chain_ref.as_bytes()));
    let error = exit_bundle(Path::new(REFERENCE), seq, ExitSubject::Deposit(id + 1)).unwrap_err();
    assert!(error.to_string().contains("holds no credited, pending or held receipt"), "{error:#}");
}

#[test]
fn the_builder_reproduces_a_published_logs_root_whatever_signed_it() {
    let mut checkpoints = Vec::new();
    log::read_file(Path::new(REFERENCE), |entry| {
        if let LogEntry::Checkpoint(cp) = entry {
            checkpoints.push((cp.seq, cp.root));
        }
        Ok(())
    })
    .unwrap();
    let (seq, root) = checkpoints.pop().expect("the reference log checkpoints");
    let bundle = exit_bundle(Path::new(REFERENCE), seq, ExitSubject::House).unwrap();
    assert_eq!(format!("{:#x}", bundle.root), root);
    assert!(bundle.house_pending_deposits.iter().all(|pending| pending.verify(bundle.root)));
    assert!(!bundle.house_pending_deposits.is_empty(), "the V38 session leaves a house deposit pending");
    let error = exit_bundle(Path::new(REFERENCE), seq + 1, ExitSubject::House).unwrap_err();
    assert!(error.to_string().contains("no checkpoint at seq"), "{error:#}");
}

#[tokio::test]
async fn a_torn_suffix_past_the_checkpoint_still_gives_exit_proofs() {
    // Both log shapes: the segmented log the node writes, and a single file as a downloaded
    // archive or copied segment arrives.
    let dir = crate::test_support::scratch_dir("exit-proof-torn-suffix");
    let node = crate::test_support::TestSequencer::start(&dir, 16);
    node.submit(crate::test_support::deposit(100, "0xfeed:1")).await;
    node.submit(crate::test_support::deposit(40, "0xfeed:2")).await;
    node.stop().await;
    let checkpoint = crate::test_support::log_entries(&dir)
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp.seq),
            _ => None,
        })
        .max()
        .expect("the shutdown checkpoint");
    let mut segments: Vec<std::path::PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|item| item.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".jsonl"))
                .is_some_and(|stem| stem.parse::<u64>().is_ok())
        })
        .collect();
    segments.sort();
    assert!(!segments.is_empty());
    // A whole-file read breaks on the torn write, which must not block proofs the complete
    // prefix supports; the segmented log leaves its open segment's cut line out.
    let torn = b"{\"type\":\"command\",\"sequenced\":{\"seq\":";
    let mut single = segments.iter().flat_map(|path| fs::read(path).unwrap()).collect::<Vec<_>>();
    single.extend_from_slice(torn);
    let single_path = dir.with_extension("whole.jsonl");
    fs::write(&single_path, &single).unwrap();
    use std::io::Write;
    fs::OpenOptions::new().append(true).open(segments.last().unwrap()).unwrap().write_all(torn).unwrap();
    assert!(log::read_file(&single_path, |_| Ok(())).is_err(), "the torn suffix breaks a whole-file read");
    let from_dir = exit_bundle(&dir, checkpoint, ExitSubject::House).unwrap();
    let from_file = exit_bundle(&single_path, checkpoint, ExitSubject::House).unwrap();
    assert_eq!(from_dir.root, from_file.root, "the same prefix, whatever its suffix");
    assert!(from_dir.house_pending_deposits.iter().all(|pending| pending.verify(from_dir.root)));
}
