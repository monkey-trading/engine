use engine_core::emissions::TokenUnits;
use engine_core::staking::UsdcUnits;
use engine_core::types::Side;
use serde_json::json;

use super::*;

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

/// Closes `epoch` as a command at `ts` would, with these events, through the same `observe`.
fn close(rig: &mut Rig, ts: Millis, mut events: Vec<Event>, epoch: u64, allocated: u128) {
    events.push(Event::DistributionEpochClosed {
        epoch,
        nav: Fixed::from_int(1_500),
        floor: Fixed::from_int(1_000),
        excess: UsdcUnits(allocated + 7),
        allocated: UsdcUnits(allocated),
    });
    rig.stats.observe(&rig.engine, ts, &events);
}

fn share(account: AccountId, epoch: u64, amount: u128) -> Event {
    Event::OperatorShareAllocated { account, epoch, amount: UsdcUnits(amount) }
}

fn stakers(account: AccountId, epoch: u64, amount: u128) -> Event {
    Event::DistributionAllocated { account, epoch, amount: UsdcUnits(amount) }
}

#[test]
fn a_close_records_stakers_every_operator_share_recipient_and_the_skipped_range() {
    let mut rig = Rig::staking();
    let third = AccountId([9; 20]);
    let events = vec![
        Event::DistributionEpochsSkipped { first: 0, last: 1 },
        share(OTHER, 2, 100),
        share(third, 2, 50),
        stakers(TRADER, 2, 300),
        stakers(OTHER, 2, 150),
    ];
    close(&mut rig, T0 + 3 * 86_400_000 + 5, events, 2, 600);
    let page = json(rig.stats.distributions(&rig.engine, None, 10).unwrap());
    let epoch = &page["epochs"][0];
    assert_eq!(epoch["epoch"], 2);
    assert_eq!(epoch["close_ms"], T0 + 3 * 86_400_000 + 5);
    assert_eq!(epoch["ended_ms"], T0 + 3 * 86_400_000);
    assert_eq!(epoch["close_seq"], rig.engine.last_seq().unwrap());
    assert_eq!((epoch["nav"].as_str(), epoch["floor"].as_str()), (Some("1500"), Some("1000")));
    assert_eq!(epoch["excess_usdc_units"], "607");
    assert_eq!(epoch["allocated_usdc_units"], "600");
    assert_eq!((epoch["stakers_usdc_units"].as_str(), epoch["stakers"].as_u64()), (Some("450"), Some(2)));
    assert_eq!(
        epoch["operator_shares"],
        json!([
            { "account": OTHER.to_string(), "amount": "100" },
            { "account": third.to_string(), "amount": "50" },
        ])
    );
    assert_eq!(epoch["skipped"], json!({ "first": 0, "last": 1 }));
    assert_eq!(page["from_genesis"], true);
    assert_eq!(page["begins_seq"], 0);
}

#[test]
fn a_close_with_nothing_to_allocate_has_no_recipients_and_no_skip() {
    let mut rig = Rig::staking();
    close(&mut rig, T0 + 86_400_000, vec![], 0, 0);
    let epoch = &json(rig.stats.distributions(&rig.engine, None, 10).unwrap())["epochs"][0];
    assert_eq!((epoch["stakers"].as_u64(), epoch["stakers_usdc_units"].as_str()), (Some(0), Some("0")));
    assert_eq!((&epoch["operator_shares"], &epoch["skipped"]), (&json!([]), &serde_json::Value::Null));
}

#[test]
fn one_closes_allocations_do_not_leak_into_the_next() {
    let mut rig = Rig::staking();
    close(&mut rig, T0 + 86_400_000, vec![share(OTHER, 0, 10), stakers(TRADER, 0, 90)], 0, 100);
    close(&mut rig, T0 + 2 * 86_400_000, vec![], 1, 0);
    let page = json(rig.stats.distributions(&rig.engine, None, 10).unwrap());
    assert_eq!(page["epochs"][0]["operator_shares"], json!([]));
    assert_eq!(page["epochs"][1]["operator_shares"].as_array().unwrap().len(), 1);
}

#[test]
fn epochs_page_newest_first_and_the_cursor_continues_where_a_page_stopped() {
    let mut rig = Rig::staking();
    for epoch in 0..5 {
        close(&mut rig, T0 + (epoch as i64 + 1) * 86_400_000, vec![], epoch, 0);
    }
    let first = json(rig.stats.distributions(&rig.engine, None, 2).unwrap());
    let epochs = |page: &serde_json::Value| -> Vec<u64> {
        page["epochs"].as_array().unwrap().iter().map(|e| e["epoch"].as_u64().unwrap()).collect()
    };
    assert_eq!((epochs(&first), first["next"].as_u64()), (vec![4, 3], Some(3)));
    let second = json(rig.stats.distributions(&rig.engine, first["next"].as_u64(), 2).unwrap());
    assert_eq!((epochs(&second), second["next"].as_u64()), (vec![2, 1], Some(1)));
    let last = json(rig.stats.distributions(&rig.engine, Some(1), 2).unwrap());
    assert_eq!((epochs(&last), &last["next"]), (vec![0], &serde_json::Value::Null));
    // A cursor past the newest epoch starts at the newest; one at the oldest is empty.
    assert_eq!(epochs(&json(rig.stats.distributions(&rig.engine, Some(99), 1).unwrap())), vec![4]);
    assert!(epochs(&json(rig.stats.distributions(&rig.engine, Some(0), 1).unwrap())).is_empty());
    assert!(rig.stats.distributions(&rig.engine, None, 0).is_err());
    assert!(rig.stats.distributions(&rig.engine, None, 201).is_err());
}

#[test]
fn a_full_history_drops_the_oldest_epoch_and_says_it_is_truncated() {
    let mut cfg = dev_config();
    cfg.stats.distribution_limit = 3;
    let mut rig = Rig::recording_into(Stats::new(&cfg).unwrap());
    let before = json(rig.stats.distributions(&rig.engine, None, 10).unwrap());
    assert_eq!(
        (before["truncated"].clone(), before["retained_from_epoch"].clone()),
        (json!(false), serde_json::Value::Null)
    );
    for epoch in 0..5 {
        close(&mut rig, T0 + (epoch as i64 + 1) * 86_400_000, vec![], epoch, 0);
    }
    let page = json(rig.stats.distributions(&rig.engine, None, 10).unwrap());
    assert_eq!(page["epochs"].as_array().unwrap().len(), 3);
    assert_eq!((page["truncated"].clone(), page["retained_from_epoch"].clone()), (json!(true), json!(2)));
    assert!(page["next"].is_null());
}

#[test]
fn a_real_close_through_the_engine_is_recorded_with_what_it_paid_stakers() {
    let mut rig = Rig::staking();
    let stake = |n: u32, who, amount| Command::Stake {
        account: who,
        amount: TokenUnits(amount),
        block: 1,
        chain_ref: format!("0xs:{n}"),
    };
    rig.apply(T0 + 1_000, stake(0, TRADER, 100));
    rig.apply(T0 + 2_000, stake(1, OTHER, 300));
    // Losses lift house NAV above the 1000 floor, so the close has something to distribute.
    rig.trade(T0 + 3_000, TRADER, Side::Buy, 3);
    rig.price(T0 + 3_500, 99_000);
    rig.trade(T0 + 4_000, TRADER, Side::Sell, 3);
    let events = rig.apply(T0 + 86_400_000 + 10, Command::Tick);
    let allocated: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::DistributionAllocated { account, amount, .. } => Some((*account, amount.0)),
            _ => None,
        })
        .collect();
    assert_eq!(allocated.len(), 2, "{events:?}");
    let page = json(rig.stats.distributions(&rig.engine, None, 10).unwrap());
    let epoch = &page["epochs"][0];
    assert_eq!(epoch["epoch"], 0);
    assert_eq!(epoch["stakers"], 2);
    assert_eq!(epoch["stakers_usdc_units"], allocated.iter().map(|(_, amount)| amount).sum::<u128>().to_string());
    assert_eq!(epoch["allocated_usdc_units"], epoch["stakers_usdc_units"]);
    assert_eq!(epoch["operator_shares"], json!([]));
}

/// What a page costs from a full history: the seek and clone under the lock, and the JSON. Run with
/// `cargo test -p engine-node --lib stats::tests::distributions::page_cost -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not a check"]
fn page_cost_from_a_full_history() {
    let mut cfg = dev_config();
    cfg.stats.distribution_limit = 100_000;
    let rig = Rig::recording_into(Stats::new(&cfg).unwrap());
    let epochs = cfg.stats.distribution_limit as u64;
    for epoch in 0..epochs {
        let row = crate::stats::distributions::EpochClose {
            epoch,
            close_seq: epoch,
            close_ms: T0 + epoch as i64,
            ended_ms: T0 + epoch as i64,
            nav: Fixed::from_int(1_500),
            floor: Fixed::from_int(1_000),
            excess_usdc_units: UsdcUnits(600),
            allocated_usdc_units: UsdcUnits(590),
            stakers_usdc_units: UsdcUnits(472),
            stakers: 40,
            operator_shares: vec![crate::stats::distributions::OperatorShare {
                account: OTHER,
                amount: UsdcUnits(118),
            }],
            skipped: None,
        };
        rig.stats.lock().distributions.push(row);
    }
    let runs = 200;
    let (mut seek, mut bytes) = (std::time::Duration::ZERO, 0);
    let started = std::time::Instant::now();
    for _ in 0..runs {
        let at = std::time::Instant::now();
        let page = rig.stats.distributions(&rig.engine, Some(epochs / 2), 200).unwrap();
        seek += at.elapsed();
        bytes = serde_json::to_vec(&page).unwrap().len();
    }
    println!(
        "distributions, {epochs} epochs held: a 200-epoch page from the middle takes {:?} under the lock, {:?} with its JSON, {bytes} bytes",
        seek / runs,
        started.elapsed() / runs
    );
}
