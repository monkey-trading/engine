//! The relayer's gas balance: `checkpoint_relayer_low` fires while it is below the configured
//! minimum and resolves once it is refilled.

use alloy::primitives::Address;
use alloy::primitives::utils::parse_ether;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::alerts::{Status, capture_until_dropped, pages_for};
use crate::checkpoints::RelayerGas;
use crate::checkpoints::relayer_gas::READ_EVERY;
use crate::checkpoints::tests::poster;
use crate::metrics::FloatGauge;

#[tokio::test(start_paused = true)]
async fn a_relayer_below_the_minimum_warns_until_it_is_refilled() {
    let (_guard, mut captured) = capture_until_dropped();
    let (mut poster, _work, _key) = poster();
    let balance = poster.chain.relayer_balance.clone();
    let exported = FloatGauge::default();
    poster.relayer_gas = Some(RelayerGas {
        relayer: Address::repeat_byte(9),
        min_balance: parse_ether("1").unwrap(),
        balance_eth: exported.clone(),
        low: Default::default(),
    });
    *balance.lock().unwrap() = Some(parse_ether("0.5").unwrap());
    let retry = poster.retry;
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, _status) = mpsc::channel(4);
    let task = tokio::spawn(poster.run(work_rx, tx));

    // The worker reads the balance on a retry tick every `READ_EVERY`; every read while low logs the
    // warning, and they are one episode.
    for _ in 0..3 {
        sleep(READ_EVERY + retry).await;
    }
    assert_eq!(exported.get(), 0.5);
    let mut lines = captured.take();
    assert!(lines.len() > 1, "each read while low logs the alert");
    let firing = pages_for(lines.clone());
    assert_eq!(firing.len(), 1, "{firing:?}");
    assert_eq!(firing[0].0, Status::Firing);
    assert!(firing[0].1.starts_with("checkpoint_relayer_low"), "{firing:?}");

    // A balance exactly at the minimum is funded: the episode resolves.
    *balance.lock().unwrap() = Some(parse_ether("1").unwrap());
    for _ in 0..3 {
        sleep(READ_EVERY + retry).await;
    }
    assert_eq!(exported.get(), 1.0);
    lines.extend(captured.take());
    let pages = pages_for(lines);
    assert_eq!(pages.iter().map(|page| page.0).collect::<Vec<_>>(), [Status::Firing, Status::Resolved], "{pages:?}");
    assert_eq!(pages[0].1, pages[1].1, "one episode");

    drop(work_tx);
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_relayer_balance_is_read_once_every_read_interval_whatever_the_retry() {
    let (mut poster, _work, _key) = poster();
    let balance = poster.chain.relayer_balance.clone();
    let exported = FloatGauge::default();
    poster.relayer_gas = Some(RelayerGas {
        relayer: Address::repeat_byte(9),
        min_balance: parse_ether("1").unwrap(),
        balance_eth: exported.clone(),
        low: Default::default(),
    });
    *balance.lock().unwrap() = Some(parse_ether("2").unwrap());
    let retry = poster.retry;
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, _status) = mpsc::channel(4);
    let task = tokio::spawn(poster.run(work_rx, tx));

    sleep(retry).await;
    assert_eq!(exported.get(), 2.0, "the first tick reads the balance");
    *balance.lock().unwrap() = Some(parse_ether("0.5").unwrap());
    sleep(READ_EVERY - retry * 2).await;
    assert_eq!(exported.get(), 2.0, "no tick before READ_EVERY reads it again");
    sleep(retry * 2).await;
    assert_eq!(exported.get(), 0.5, "the tick at READ_EVERY reads it");

    drop(work_tx);
    task.await.unwrap().unwrap();
}
