//! Actual API, log, signer, Vault and FIFO assertions across crashes and returning liquidity.

use std::path::Path;
use std::time::Duration;

use alloy::primitives::{B256, U256};
use alloy::sol_types::{SolCall, SolEvent};
use anyhow::{Context, ensure};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::{Event, Reject};
use engine_core::types::{AccountId, Side};
use tokio::time::{sleep, timeout};

use crate::log::{CheckpointDomain, LogEntry};
use crate::sequencer::withdrawal_scenario::Barrier;
use crate::sequencer::withdrawal_scenario::client::{Client, User};
use crate::sequencer::withdrawal_scenario::node::Node;
use crate::sequencer::withdrawal_scenario::rpc::{
    WithdrawalPaid, balanceOfCall, latestSeqCall, totalWithdrawnCall, usedNoncesCall,
};
use crate::sequencer::{SignedWithdrawal, replay_path_with_trust};
use crate::test_support::LOADED_WAIT;

/// Exercises local deposit, exact conversion, malformed proofs/signatures, both crash boundaries,
/// old accepted roots, actual duplicate transactions and healthy post-payment reconciliation.
/// Error cases:
/// - any real node/chain step or behavioral assertion fails
pub(super) async fn payments_and_crashes() -> anyhow::Result<()> {
    let mut node = Node::start("payments-and-crashes").await?;
    let trader = User::new(1)?;
    node.chain.deposit(trader.address(), 100, false).await?;
    node.client.declare(&trader).await?;
    node.client.wait_balance(&trader, "100").await?;
    node.client.wait_finalised(&trader).await?;
    node.wait_withdrawals_enabled(last_seq(&node.dir.join("log"))?).await?;
    let mut stream = node.client.stream().await?;
    let request = node.client.withdrawal(&trader, "10.0000009").await?;
    let events = node.client.request_withdrawal(&request).await?;
    ensure!(
        approvals(&events) == [(trader.account, Fixed::from_int(10), 1)],
        "base-unit approval differs from debit: {events:?}"
    );
    node.client.wait_balance(&trader, "90").await?;
    let first = stream.payout(trader.account, 10).await?;
    ensure!(
        entries(&node.dir.join("log"))?.contains(&LogEntry::Withdrawal(first.clone())),
        "public authorization preceded its log entry"
    );
    // Only the account's signature reissues, and only an authorization that is expired or retired.
    let expires_at = first.authorization.as_ref().context("typed payout")?.expires_at;
    let other = User::new(2)?;
    let (status, _) = node.client.reissue(&other, trader.account, first.nonce, expires_at).await?;
    ensure!(status == 401, "another account's signature reissued: {status}");
    let (status, _) = node.client.reissue(&trader, trader.account, 99, 0).await?;
    ensure!(status == 404, "an unknown approval answered {status}");
    let (status, body) = node.client.reissue(&trader, trader.account, first.nonce, expires_at + 1).await?;
    ensure!(status == 400 && body.to_string().contains("not the one replaced"), "stale: {status} {body}");
    let (status, body) = node.client.reissue(&trader, trader.account, first.nonce, expires_at).await?;
    ensure!(status == 400 && body.to_string().contains("usable until"), "a usable authorization: {status} {body}");
    let mut invalid = node.chain.claim(&first)?;
    if let Some(sibling) = invalid.siblings.first_mut() {
        *sibling ^= B256::with_last_byte(1);
    } else {
        invalid.siblings.push(B256::ZERO);
    }
    node.chain.refuses(invalid.abi_encode()).await?;
    let mut invalid = node.chain.claim(&first)?;
    let mut bytes = invalid.signature.to_vec();
    bytes[0] ^= 1;
    invalid.signature = bytes.into();
    node.chain.refuses(invalid.abi_encode()).await?;

    let interrupted = node.client.withdrawal(&trader, "11").await?;
    let nonce = interrupted.auth.as_ref().context("signed request")?.nonce;
    std::fs::write(node.dir.join("barrier.arm"), serde_json::to_vec(&Barrier { account: trader.account, nonce })?)?;
    let caller = node.client.clone();
    let pending = interrupted.clone();
    let submission = tokio::spawn(async move { caller.post("/v1/withdrawals", &pending).await });
    timeout(LOADED_WAIT, async {
        while !node.dir.join("barrier.reached").exists() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("before-approval-log hook was not reached")?;
    node.crash().await?;
    ensure!(submission.await?.is_err(), "pre-log command was acknowledged");
    let snapshot = entries(&node.dir.join("log"))?;
    ensure!(
        !snapshot.iter().any(|entry| matches!(entry, LogEntry::Command(command)
        if matches!(&command.sequenced.cmd, Command::AccessWithdraw { request, .. } if request.auth.as_ref().is_some_and(|auth| auth.nonce == nonce)))),
        "RAM-only approval entered the public log before the barrier"
    );
    std::fs::remove_file(node.dir.join("barrier.arm"))?;
    std::fs::remove_file(node.dir.join("barrier.reached"))?;
    drop(stream);
    node.restart().await?;
    node.client.wait_balance(&trader, "90").await?;
    ensure!(node.client.nonce(&trader).await? == nonce, "pre-log crash consumed a request nonce");
    node.wait_withdrawals_enabled(0).await?;
    let mut stream = node.client.stream().await?;
    node.client.request_withdrawal(&interrupted).await?;
    let durable = stream.payout(trader.account, 11).await?;
    ensure!(
        entries(&node.dir.join("log"))?.contains(&LogEntry::Withdrawal(durable.clone())),
        "stream payout is not durable"
    );
    node.crash().await?;
    drop(stream);
    node.restart().await?;
    node.wait_withdrawals_enabled(0).await?;
    node.client.wait_balance(&trader, "79").await?;
    let mut stream = node.client.stream().await?;
    let recovered_first = stream.payout(trader.account, 10).await?;
    let recovered = stream.payout(trader.account, 11).await?;
    ensure!(
        recovered.nonce == durable.nonce && recovered.approved_seq == durable.approved_seq,
        "restart changed payout identity"
    );
    ensure!(
        node.chain.read(node.chain.vault, latestSeqCall {}).await?
            > recovered_first.authorization.as_ref().context("typed payout")?.checkpoint_seq,
        "old-root claim did not use an older accepted checkpoint"
    );
    pay(&node, &recovered_first).await?;
    let paid = node.chain.read(node.chain.vault, totalWithdrawnCall {}).await?;
    node.chain.duplicate_reverts(node.chain.claim(&recovered_first)?.abi_encode()).await?;
    ensure!(
        node.chain.read(node.chain.vault, totalWithdrawnCall {}).await? == paid,
        "duplicate increased paid counter"
    );
    pay(&node, &recovered).await?;

    // A new release requires the real worker to reconcile the reduced token balance against its
    // independently read cumulative paid counter, not a manually supplied healthy flag.
    let next = node.client.withdrawal(&trader, "12").await?;
    node.client.request_withdrawal(&next).await?;
    pay(&node, &stream.payout(trader.account, 12).await?).await?;
    node.client.wait_balance(&trader, "67").await?;
    ensure!(
        node.chain.read(node.chain.vault, totalWithdrawnCall {}).await? == U256::from(33_000_000),
        "wrong cumulative paid units"
    );
    ensure!(
        node.chain.read(node.chain.token, balanceOfCall { owner: node.chain.vault }).await? == U256::from(67_000_000),
        "wrong final Vault collateral"
    );
    drop(stream);
    node.stop().await?;
    let log = entries(&node.dir.join("log"))?;
    ensure!(log.iter().filter(|entry| matches!(entry, LogEntry::Command(command) if matches!(command.sequenced.cmd, Command::AccessWithdraw { .. }))).count() == 3,
        "crash/retry duplicated a withdrawal command");
    replay(&node)?;
    println!(
        "PASS: actual TLS/stream withdrawals, exact units, tamper refusal, double claim, pre-log SIGKILL, durable restart, old root and paid reconciliation"
    );
    Ok(())
}

/// Realizes trader wins beyond custody, reserves public FIFO requests, realizes a loss without
/// fabricating tokens, and pays partial heads before followers as actual deposits arrive.
/// Error cases:
/// - any real node/chain step, public queue, log or accounting assertion fails
pub(super) async fn shortfall_and_fifo() -> anyhow::Result<()> {
    let mut node = Node::start("shortfall-and-fifo").await?;
    let a = User::new(1)?;
    let b = User::new(2)?;
    let loser = User::new(3)?;
    node.chain.deposit(node.chain.funder, 10, true).await?;
    // Payout capacity counts only credited USDC, so the head's partial amount needs this credit.
    node.client.wait_house_cash("10").await?;
    for user in [&a, &b, &loser] {
        node.chain.deposit(user.address(), 10, false).await?;
        node.client.declare(user).await?;
        node.client.wait_balance(user, "10").await?;
    }
    // House principal was deposited first, so it is final once every trader deposit is: capacity
    // then counts the full collateral the FIFO head is approved against below.
    for user in [&a, &b, &loser] {
        node.client.wait_finalised(user).await?;
    }
    node.client.wait_mark(100).await?;
    filled(&node.client.order(&a, Side::Buy, "1").await?)?;
    filled(&node.client.order(&b, Side::Buy, "1").await?)?;
    node.services.set_price(200);
    node.client.wait_mark(200).await?;
    filled(&node.client.order(&a, Side::Sell, "1").await?)?;
    filled(&node.client.order(&b, Side::Sell, "1").await?)?;
    node.client.wait_balance(&a, "110").await?;
    node.client.wait_balance(&b, "110").await?;
    filled(&node.client.order(&loser, Side::Buy, "0.1").await?)?;
    node.wait_withdrawals_enabled(last_seq(&node.dir.join("log"))?).await?;
    let mut stream = node.client.stream().await?;
    let request = node.client.withdrawal(&a, "110").await?;
    let events = node.client.request_withdrawal(&request).await?;
    ensure!(
        approvals(&events) == [(a.account, Fixed::from_int(40), 1)],
        "head was not partially approved against actual collateral: {events:?}"
    );
    let request = node.client.withdrawal(&b, "110").await?;
    let events = node.client.request_withdrawal(&request).await?;
    ensure!(approvals(&events).is_empty(), "follower passed the partial head: {events:?}");
    queue(&node.client, &[(a.account, "70"), (b.account, "110")]).await?;
    node.client.wait_balance(&a, "0").await?;
    node.client.wait_balance(&b, "0").await?;
    let account = node.client.get(&format!("/v1/accounts/{}/withdrawals", b.account)).await?;
    ensure!(
        account["queue"][0]["position"] == 2 && account["queue"][0]["remaining"] == "110",
        "account queue position is not public FIFO position"
    );
    let rejection = node.client.order(&a, Side::Buy, "1").await?;
    ensure!(
        rejection
            .iter()
            .any(|event| matches!(event, Event::OrderRejected { reason: Reject::InsufficientMargin { .. }, .. })),
        "reserved cash still margins trades"
    );
    pay(&node, &stream.payout(a.account, 40).await?).await?;
    ensure!(
        node.chain.read(node.chain.token, balanceOfCall { owner: node.chain.vault }).await? == U256::ZERO,
        "partial approval exceeded physical funds"
    );
    node.services.set_price(100);
    node.client.wait_mark(100).await?;
    node.client.wait_balance(&loser, "0").await?;
    queue(&node.client, &[(a.account, "70"), (b.account, "110")]).await?;
    ensure!(
        node.chain.read(node.chain.vault, totalWithdrawnCall {}).await? == U256::from(40_000_000),
        "trader loss manufactured a payment"
    );
    ensure!(
        node.chain.read(node.chain.token, balanceOfCall { owner: node.chain.vault }).await? == U256::ZERO,
        "trader loss minted collateral"
    );

    node.chain.deposit(loser.address(), 40, false).await?;
    node.client.wait_finalised(&loser).await?;
    pay(&node, &stream.payout(a.account, 40).await?).await?;
    queue(&node.client, &[(a.account, "30"), (b.account, "110")]).await?;
    node.chain.deposit(node.chain.funder, 60, true).await?;
    let head = stream.payout(a.account, 30).await?;
    let follower = stream.payout(b.account, 30).await?;
    ensure!(
        head.approved_seq == follower.approved_seq,
        "one replenishment should finish the head before partially approving its follower"
    );
    pay(&node, &head).await?;
    pay(&node, &follower).await?;
    queue(&node.client, &[(b.account, "80")]).await?;
    node.chain.deposit(loser.address(), 80, false).await?;
    node.client.wait_finalised(&loser).await?;
    pay(&node, &stream.payout(b.account, 80).await?).await?;
    queue(&node.client, &[]).await?;
    node.client.wait_balance(&loser, "120").await?;
    ensure!(
        node.chain.read(node.chain.vault, totalWithdrawnCall {}).await? == U256::from(220_000_000),
        "FIFO payments differ from collateral deposits"
    );
    ensure!(
        node.chain.read(node.chain.token, balanceOfCall { owner: node.chain.vault }).await? == U256::ZERO,
        "all approved liquidity should be paid"
    );
    drop(stream);
    node.stop().await?;
    let log = entries(&node.dir.join("log"))?;
    let events = log
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => command.events.as_ref(),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    ensure!(
        approvals(&events)
            == [
                (a.account, Fixed::from_int(40), 1),
                (a.account, Fixed::from_int(40), 2),
                (a.account, Fixed::from_int(30), 3),
                (b.account, Fixed::from_int(30), 1),
                (b.account, Fixed::from_int(80), 2)
            ],
        "public log violates FIFO approval order"
    );
    ensure!(events.iter().any(|event| matches!(event, Event::Fill(fill) if fill.account == loser.account && fill.realised_pnl.is_negative())), "scenario never realized the actual trader loss");
    ensure!(
        events.iter().filter(|event| matches!(event, Event::WithdrawalQueued { .. })).count() == 2,
        "public log omitted queue reservations"
    );
    replay(&node)?;
    println!(
        "PASS: actual trader wins, partial FIFO head, public queue/reservations, real losses without minted liquidity, deposit/house replenishment and ordered payments"
    );
    Ok(())
}

/// Pays a stream authorization and authenticates the exact token changes and Vault event.
/// Error cases:
/// - verification, EVM payment, nonce/counter or event fields differ
pub(super) async fn pay(node: &Node, signed: &SignedWithdrawal) -> anyhow::Result<()> {
    let call = node.chain.claim(signed)?;
    let amount = call.withdrawal.amount;
    let owner = call.withdrawal.account;
    let before = node.chain.read(node.chain.token, balanceOfCall { owner }).await?;
    let vault_before = node.chain.read(node.chain.token, balanceOfCall { owner: node.chain.vault }).await?;
    let paid_before = node.chain.read(node.chain.vault, totalWithdrawnCall {}).await?;
    let receipt = node.chain.send(node.chain.payer, node.chain.vault, call.abi_encode()).await?;
    ensure!(
        node.chain.read(node.chain.token, balanceOfCall { owner }).await? == before + amount,
        "signed amount differs from recipient credit"
    );
    ensure!(
        node.chain.read(node.chain.token, balanceOfCall { owner: node.chain.vault }).await? == vault_before - amount,
        "signed amount differs from Vault debit"
    );
    ensure!(
        node.chain.read(node.chain.vault, totalWithdrawnCall {}).await? == paid_before + amount,
        "paid counter differs from token transfer"
    );
    ensure!(
        node.chain.read(node.chain.vault, usedNoncesCall { account: owner, nonce: signed.nonce }).await?,
        "paid nonce not recorded"
    );
    let event = receipt
        .logs()
        .iter()
        .filter(|log| log.address() == node.chain.vault)
        .find_map(|log| WithdrawalPaid::decode_log(&log.inner).ok())
        .context("authenticated WithdrawalPaid event")?
        .data;
    ensure!(
        event.account == owner
            && event.amount == amount
            && event.nonce == signed.nonce
            && Some(event.approvedSeq) == signed.approved_seq,
        "Vault payment event does not identify the stream approval"
    );
    Ok(())
}

/// Checks exact globally visible queue order and remaining reservations.
/// Error cases:
/// - API pagination/order/remaining values differ
async fn queue(client: &Client, expected: &[(AccountId, &str)]) -> anyhow::Result<()> {
    let value = client.get("/v1/withdrawals").await?;
    let queue = value["queue"].as_array().context("public queue")?;
    ensure!(queue.len() == expected.len(), "public queue length differs: {value}");
    for (index, (account, remaining)) in expected.iter().enumerate() {
        ensure!(
            queue[index]["account"] == serde_json::to_value(account)?
                && queue[index]["remaining"] == *remaining
                && queue[index]["position"] == index + 1,
            "public FIFO queue differs: {value}"
        );
    }
    Ok(())
}

/// Selects deterministic payout events without treating signatures as payments.
fn approvals(events: &[Event]) -> Vec<(AccountId, Fixed, u64)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::WithdrawalApproved { account, amount, nonce } => Some((*account, *amount, *nonce)),
            _ => None,
        })
        .collect()
}

/// Requires one fee-free actual engine fill rather than a rejected or resting request.
/// Error cases:
/// - a fill is missing/duplicated or the configured fee is nonzero
fn filled(events: &[Event]) -> anyhow::Result<()> {
    let fills = events
        .iter()
        .filter_map(|event| match event {
            Event::Fill(fill) => Some(fill),
            _ => None,
        })
        .collect::<Vec<_>>();
    ensure!(fills.len() == 1 && fills[0].profit_fee.is_zero(), "fee-free scripted order did not fill: {events:?}");
    Ok(())
}

/// Reads complete log lines without opening a second writer or repairing a crashed log.
/// Error cases:
/// - files cannot be read or a complete line is malformed
pub(super) fn entries(dir: &Path) -> anyhow::Result<Vec<LogEntry>> {
    let mut paths =
        std::fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.path())).collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.as_bytes().first().is_some_and(u8::is_ascii_digit))
            && path.extension() == Some(std::ffi::OsStr::new("jsonl"))
    });
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        let bytes = std::fs::read(path)?;
        for line in bytes.split_inclusive(|byte| *byte == b'\n').filter(|line| line.last() == Some(&b'\n')) {
            entries.push(serde_json::from_slice(line)?);
        }
    }
    Ok(entries)
}

/// The highest sequence the log holds: everything the node has acknowledged so far.
/// Error cases:
/// - the log cannot be read or holds no command
pub(super) fn last_seq(dir: &Path) -> anyhow::Result<u64> {
    entries(dir)?
        .iter()
        .filter_map(|entry| if let LogEntry::Command(command) = entry { Some(command.sequenced.seq) } else { None })
        .max()
        .context("log holds no command")
}

/// Replays actual persisted roots, events and authorizations under the independently known signer.
/// Error cases:
/// - full replay fails or the log does not end under the latest protocol
pub(super) fn replay(node: &Node) -> anyhow::Result<()> {
    let replayed = replay_path_with_trust(
        &node.dir.join("log"),
        Some(CheckpointDomain { chain_id: 31337, vault: AccountId(node.chain.vault.into()) }),
        Some(node.chain.signer),
    )?;
    ensure!(
        replayed.engine.protocol_version() == engine_core::protocol::ProtocolVersion::LATEST,
        "node boot did not activate the latest protocol"
    );
    Ok(())
}
