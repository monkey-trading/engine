//! Assembles the book.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use engine_core::access::AccessPolicy;
use engine_core::account::{Position, RestingOrder};
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::types::{AccountId, MarketId, Side};
use engine_core::volume_tier::{AccountVolume, DayVolume, WINDOW_DAYS, day_of};
use engine_core::{Engine, Fixed};
use serde_json::json;

use crate::common::close_spread::{attest, fund, rest, source_key, trade};
use crate::common::launch_book::genesis::{BOOK_TIME, price_market, production_harness, publish_benchmarks};
use crate::common::launch_book::mix::{Mix, Rng, market_weights};
use crate::common::session_keys::{HOUR_MS, register};
use crate::common::{CHAIN_ID, Client, Harness, VAULT, f};

/// The book's mark in every market.
pub const MARK: i64 = 100;
/// The market whose price closes out `Named::closeouts`, one account to a step of a dollar down from
/// the mark, and the position each holds in it, whose loss takes the whole account below its level
/// (each holds one unit in every other market, so that loss is most of its margin):
/// ETH-USD, where the book's in-band resting orders are sells, which a fall does not reach.
pub const CLOSEOUT_MARKET: MarketId = 2;
const CLOSEOUT_QTY: &str = "3000";

/// How many of each named account a book makes. Each is a real client, signing its own commands.
#[derive(Clone, Copy, Debug)]
pub struct NamedCounts {
    /// Accounts holding a position in every market, which a benchmark adds to.
    pub adders: usize,
    /// Accounts holding nothing, which a benchmark opens a position for.
    pub openers: usize,
    /// Accounts holding a position in every market, each set to fall below its close-out level one
    /// step of the price later than the one before, so one price closes out one of them.
    pub closeouts: usize,
}

/// The named accounts' clients.
pub struct Named {
    pub adders: Vec<Client>,
    pub openers: Vec<Client>,
    /// In the order they fall below the close-out level, as `closeout_mark` steps `CLOSEOUT_MARKET` down.
    pub closeouts: Vec<Client>,
}

/// The price of `CLOSEOUT_MARKET` after `steps` steps down from the mark, as a decimal.
pub fn closeout_mark(steps: usize) -> String {
    (MARK - steps as i64).to_string()
}

/// A book and what it holds.
pub struct Book {
    pub h: Harness,
    pub named: Named,
    /// The named accounts, which come before the others in the book's addresses.
    first_filler: u64,
    /// Accounts holding a position in each market.
    pub holders: BTreeMap<MarketId, usize>,
    pub positions: usize,
    pub orders: usize,
    /// Accounts at each volume tier, and accounts resting orders.
    pub tiers: Vec<usize>,
    pub resting_accounts: usize,
    /// How long the assembly took, and how large the encoding it read was.
    pub assembled_in: Duration,
    pub encoded_bytes: usize,
}

impl Book {
    /// Applies `cmd` at the harness clock, in the engine's own sequence and with no record of it
    /// kept: the book's log stops where the book begins.
    pub fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let seq = self.h.engine.last_seq().map_or(0, |seq| seq + 1);
        self.h.engine.apply(&Sequenced { seq, ts: self.h.ts, cmd }).expect("in sequence")
    }

    /// `command` signed by the operator with its next nonce, applied as `apply` does.
    pub fn operate(&mut self, command: AdminCommand) -> Vec<Event> {
        let operator = Client { expires_at: self.h.ts + 60_000, ..Client::operator() };
        let nonce = self.h.engine.next_request_nonce(operator.account);
        self.apply(operator.admin(nonce, command))
    }

    /// How many accounts are named ones.
    pub fn named_total(&self) -> usize {
        self.first_filler as usize
    }

    /// The `index`th account that is not a named one.
    pub fn account(&self, index: u64) -> AccountId {
        address(0xb7, self.first_filler + index)
    }
}

/// The price of `mark` plus `micro` millionths.
fn price(mark: i64, micro: i64) -> Fixed {
    f(&format!("{}.{:06}", mark + micro.div_euclid(1_000_000), micro.rem_euclid(1_000_000)))
}

fn address(prefix: u8, index: u64) -> AccountId {
    let mut bytes = [prefix; 20];
    bytes[12..].copy_from_slice(&index.to_be_bytes());
    AccountId(bytes)
}

fn key_of(account: AccountId) -> String {
    serde_json::to_string(&account).expect("an address encodes")
}

/// Appends `"key":value` to a JSON object under construction.
fn entry(out: &mut String, key: &str, value: &str) {
    if out.len() > 1 {
        out.push(',');
    }
    out.push_str(key);
    out.push(':');
    out.push_str(value);
}

/// Builds the book.
pub fn build(mix: &Mix, counts: NamedCounts) -> Book {
    let began = Instant::now();
    let named_total = counts.adders + counts.openers + counts.closeouts;
    assert!(mix.accounts >= named_total, "the named accounts are part of the book");
    let mut h = production_harness();
    publish_benchmarks(&mut h);
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    let ids: Vec<MarketId> = h.engine.markets().keys().copied().collect();
    let mark = MARK.to_string();
    for id in &ids {
        price_market(&mut h, *id, &mark);
    }

    // The named accounts, by signed commands.
    // The units of each market worth `position_usd`: its price is `MARK` in its own currency, and a
    // euro or a pound is `MARK` dollars and a dollar `MARK` yen.
    let units: BTreeMap<MarketId, Fixed> = ids
        .iter()
        .map(|id| {
            let per_unit = match h.engine.markets()[id].spec.quote_currency {
                QuoteCurrency::Usd => MARK,
                QuoteCurrency::Eur { .. } | QuoteCurrency::Gbp { .. } => MARK * MARK,
                QuoteCurrency::Jpy { .. } => 1,
            };
            (*id, Fixed::from_int(mix.position_usd).div_int(per_unit))
        })
        .collect();
    let balance = mix.balance.to_string();
    let mut next_key = 0u32;
    let mut clients = |count: usize| -> Vec<Client> {
        (0..count)
            .map(|_| {
                next_key += 1;
                let mut scalar = [0x11; 32];
                scalar[28..].copy_from_slice(&next_key.to_be_bytes());
                let key = k256::ecdsa::SigningKey::from_slice(&scalar).expect("a valid scalar");
                let account = engine_core::eip712::address(key.verifying_key());
                let domain = engine_core::eip712::domain(CHAIN_ID, VAULT);
                Client { key, account, domain, expires_at: BOOK_TIME + 280_000 }
            })
            .collect()
    };
    let named = Named {
        adders: clients(counts.adders),
        openers: clients(counts.openers),
        closeouts: clients(counts.closeouts),
    };
    for client in named.adders.iter().chain(&named.openers).chain(&named.closeouts) {
        attest(&mut h, client);
        fund(&mut h, client, &balance);
    }
    for (client, closeout) in named.adders.iter().map(|c| (c, false)).chain(named.closeouts.iter().map(|c| (c, true))) {
        for id in &ids {
            let size = match (closeout, *id == CLOSEOUT_MARKET) {
                (true, true) => CLOSEOUT_QTY.to_owned(),
                (true, false) => "1".to_owned(),
                (false, _) => units[id].to_string(),
            };
            let events = trade(&mut h, client, *id, Side::Buy, &size, &mark);
            assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{id}: {events:?}");
        }
    }
    // The template account rests an order and holds two session keys, which the rest copy.
    let template = named.adders.first().expect("an adder is the template");
    let events = rest(&mut h, template, ids[0], Side::Buy, "1", OrderKind::Limit { price: f("50") });
    assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "{events:?}");
    for index in 0..mix.session_keys as u8 {
        let key = crate::common::session_keys::fresh_key(index);
        let cmd = register(&h, template, &key, BOOK_TIME + 285_000 + i64::from(index));
        let events = h.cmd(cmd);
        assert!(matches!(events[..], [Event::SessionKeyRegistered { .. }]), "{events:?}");
    }
    // Two small moves a horizon apart, so every fill is scored, then the mark again once the opening
    // lots have matured.
    for step in ["100.01", "100"] {
        h.advance(6_000);
        for id in &ids {
            price_market(&mut h, *id, step);
        }
    }
    h.advance(60_000);
    for id in &ids {
        price_market(&mut h, *id, &mark);
    }
    h.engine.restore_indexes();

    // Each close-out account's cash leaves it a dollar of equity at the mark its step brings, below
    // the close-out level of even one position, so the step closes every position it holds. One step
    // up, the position's smaller loss lifts its equity above the level of the whole account, so no
    // earlier step touches it.
    let mut closeout_cash = Vec::new();
    for (index, client) in named.closeouts.iter().enumerate() {
        let mut account = h.engine.accounts()[&client.account].clone();
        account.balance = Fixed::ZERO;
        let at = Some((CLOSEOUT_MARKET, f(&closeout_mark(index + 1)), None));
        let (equity, _) = h.engine.equity_and_margin(&account, at);
        closeout_cash.push(f("1") - equity);
    }

    let mut state = serde_json::to_value(&h.engine).expect("encoding");
    let next_order = state["next_order_id"].as_u64().expect("an order id");
    let skeleton = {
        let mut account = h.engine.accounts()[&template.account].clone();
        account.positions.clear();
        account.opened_at.clear();
        account.orders.clear();
        account.balance = f(&balance);
        account
    };
    let order_template =
        h.engine.accounts()[&template.account].orders.values().next().cloned().expect("rests an order");
    let template_key = serde_json::to_value(template.account).unwrap().as_str().unwrap().to_owned();
    let toxicity = state["toxicity_scores"][&template_key].clone();
    let session_keys = state["session_keys"][&template_key].clone();
    let session_key_state =
        session_keys["keys"].as_object().and_then(|keys| keys.values().next().cloned()).expect("a key");
    let tiers = h.engine.params().expect("params").volume_tiers.clone();
    let today = day_of(h.ts);
    let book_of = |tier: usize| -> Option<String> {
        let cap = tiers[tier].threshold.div_int(tiers[tier].day_divisor);
        (tier > 0).then(|| {
            let days = (today - WINDOW_DAYS as i64 + 1..=today).map(|day| DayVolume { day, volume: cap }).collect();
            serde_json::to_string(&AccountVolume { days, ..AccountVolume::default() }).expect("a book encodes")
        })
    };
    let volume_books: Vec<Option<String>> = (0..mix.tiers.len()).map(book_of).collect();

    // Named accounts' entries are carried over; the closed-out ones' cash is reset.
    let mut accounts = String::from("{");
    let mut volumes = String::from("{");
    let mut scores = String::from("{");
    let mut keys = String::from("{");
    let mut eligibility = String::from("{");
    let mut nonces = String::from("{");
    let mut refs = String::from("[");
    for (key, account) in state["accounts"].as_object().expect("accounts") {
        let mut account = account.clone();
        if let Some(index) = named.closeouts.iter().position(|c| serde_json::to_value(c.account).unwrap() == json!(key))
        {
            account["balance"] = json!(closeout_cash[index]);
        }
        entry(&mut accounts, &json!(key).to_string(), &account.to_string());
    }
    for (target, source) in [
        (&mut volumes, "volumes"),
        (&mut scores, "toxicity_scores"),
        (&mut keys, "session_keys"),
        (&mut eligibility, "eligibility"),
    ] {
        for (key, value) in state[source].as_object().into_iter().flatten() {
            entry(target, &json!(key).to_string(), &value.to_string());
        }
    }
    // Each account has made the declaration the template has, so spent a request nonce, and holds a
    // credited reference for its funding; each session key it registered has a nonce entry too.
    let account_nonce = state["request_nonces"][&template_key].to_string();
    let key_nonce = {
        let address = session_keys["keys"].as_object().and_then(|keys| keys.keys().next()).expect("a key");
        state["request_nonces"][address].to_string()
    };
    for (key, value) in state["request_nonces"].as_object().expect("nonces") {
        entry(&mut nonces, &json!(key).to_string(), &value.to_string());
    }
    for reference in state["credited_refs"].as_array().expect("references") {
        if refs.len() > 1 {
            refs.push(',');
        }
        refs.push_str(&reference.to_string());
    }
    let professional = json!("professional").to_string();
    let weights =
        market_weights(&ids.iter().map(|id| (*id, h.engine.markets()[id].spec.symbol.clone())).collect::<Vec<_>>());
    let mut resting: BTreeMap<MarketId, String> = BTreeMap::new();
    let mut quantity: BTreeMap<MarketId, (Fixed, Fixed)> = BTreeMap::new();
    for id in &ids {
        let mut text = String::from("{");
        for (order, account) in state["markets"][id.to_string()]["resting"].as_object().into_iter().flatten() {
            entry(&mut text, &json!(order).to_string(), &account.to_string());
        }
        resting.insert(*id, text);
        quantity.insert(*id, (Fixed::ZERO, Fixed::ZERO));
    }
    let bands: BTreeMap<MarketId, i64> =
        ids.iter().map(|id| (*id, h.engine.markets()[id].spec.spread_tiers[0].tenth_bps())).collect();
    let entry_price = Fixed::from_int(MARK);

    let mut order_id = next_order;
    for index in named_total as u64..mix.accounts as u64 {
        let account_id = address(0xb7, index);
        let mut rng = Rng::new(index);
        let held = {
            let bucket = Rng::bucket(mix.holdings.iter().map(|(share, ..)| *share), rng.below(100));
            let (_, fewest, most) = mix.holdings[bucket];
            let count = fewest + rng.below((most - fewest + 1) as u64) as usize;
            if count >= ids.len() { ids.clone() } else { rng.markets(count, &weights) }
        };
        let tier = Rng::bucket(mix.tiers.iter().copied(), rng.below(100));
        let order_count = if rng.below(100) < mix.resting { tiers[tier].max_resting_orders } else { 0 };
        let mut account = skeleton.clone();
        for market in &held {
            let size = units[market];
            let position =
                Position { qty: if (index + u64::from(*market)) % 2 == 0 { size } else { -size }, entry: entry_price };
            account.positions.insert(*market, position);
            account.opened_at.insert(*market, h.ts - 3_600_000);
            let held = quantity.get_mut(market).expect("listed");
            held.0 += position.qty;
            held.1 += position.cost();
        }
        let key = key_of(account_id);
        for slot in 0..order_count {
            let market = held[slot % held.len()];
            let in_band = rng.below(100) < mix.in_band;
            let buy = slot % 2 == 0;
            let micro = bands[&market] * 500;
            // In-band orders buy where the market's id is odd and sell where it is even, so a fall in
            // one market and a rise in another leave the others' resting.
            let buy = if in_band { market % 2 == 1 } else { buy };
            let limit = match (in_band, buy) {
                (true, true) => price(MARK, micro),
                (true, false) => price(MARK, -micro),
                (false, true) => Fixed::from_int(MARK / 2),
                (false, false) => Fixed::from_int(MARK * 2),
            };
            let order = RestingOrder {
                order_id,
                market,
                side: if buy { Side::Buy } else { Side::Sell },
                kind: OrderKind::Limit { price: limit },
                ..order_template.clone()
            };
            account.orders.insert(order_id, order);
            entry(resting.get_mut(&market).expect("listed"), &format!("\"{order_id}\""), &key);
            order_id += 1;
        }
        entry(&mut accounts, &key, &serde_json::to_string(&account).expect("an account encodes"));
        if let Some(text) = &volume_books[tier] {
            entry(&mut volumes, &key, text);
        }
        entry(&mut scores, &key, &toxicity.to_string());
        let mut live = session_keys.clone();
        let key_addresses: Vec<String> = (0..mix.session_keys as u64)
            .map(|slot| key_of(address(0xb8, index * 16 + slot)).trim_matches('"').to_owned())
            .collect();
        live["keys"] = key_addresses
            .iter()
            .map(|address| (address.clone(), session_key_state.clone()))
            .collect::<serde_json::Map<_, _>>()
            .into();
        entry(&mut keys, &key, &live.to_string());
        entry(&mut eligibility, &key, &professional);
        entry(&mut nonces, &key, &account_nonce);
        for address in &key_addresses {
            entry(&mut nonces, &json!(address).to_string(), &key_nonce);
        }
        if refs.len() > 1 {
            refs.push(',');
        }
        refs.push_str(&json!(format!("0xfund:b7:{index}")).to_string());
    }

    // The encoding with the maps in place of their placeholders.
    let mut placeholders: Vec<(String, String)> = Vec::new();
    for (name, text) in [
        ("accounts", accounts),
        ("volumes", volumes),
        ("toxicity_scores", scores),
        ("session_keys", keys),
        ("eligibility", eligibility),
        ("request_nonces", nonces),
    ] {
        state[name] = json!(format!("@{name}@"));
        placeholders.push((format!("\"@{name}@\""), text + "}"));
    }
    state["credited_refs"] = json!("@credited_refs@");
    placeholders.push(("\"@credited_refs@\"".to_owned(), refs + "]"));
    for id in &ids {
        let market = &mut state["markets"][id.to_string()];
        let (qty, cost) = quantity[id];
        let base_qty: Fixed = market["client_qty"].as_str().expect("qty").parse().unwrap();
        let base_cost: Fixed = market["client_cost"].as_str().expect("cost").parse().unwrap();
        market["client_qty"] = json!(base_qty + qty);
        market["client_cost"] = json!(base_cost + cost);
        market["resting"] = json!(format!("@resting{id}@"));
        placeholders.push((format!("\"@resting{id}@\""), resting.remove(id).expect("listed") + "}"));
    }
    state["next_order_id"] = json!(order_id);
    state["markouts"] = json!([]);
    let mut text = serde_json::to_string(&state).expect("encoding");
    drop(state);
    for (placeholder, body) in placeholders {
        let at = text.find(&placeholder).expect("a placeholder");
        text.replace_range(at..at + placeholder.len(), &body);
    }
    let encoded_bytes = text.len();
    h.engine = serde_json::from_str::<Engine>(&text).expect("the book decodes");
    drop(text);
    h.engine.restore_indexes();
    h.log.clear();

    let counts = Counts::of(&h.engine, mix.tiers.len());
    check(&h, mix);
    Book {
        h,
        named,
        first_filler: named_total as u64,
        holders: counts.holders,
        positions: counts.positions,
        orders: counts.orders,
        tiers: counts.tiers,
        resting_accounts: counts.resting_accounts,
        assembled_in: began.elapsed(),
        encoded_bytes,
    }
}

/// What the book holds, counted from the engine over every account, the named ones included.
struct Counts {
    holders: BTreeMap<MarketId, usize>,
    positions: usize,
    orders: usize,
    tiers: Vec<usize>,
    resting_accounts: usize,
}

impl Counts {
    fn of(engine: &Engine, tiers: usize) -> Self {
        let mut counts =
            Counts { holders: BTreeMap::new(), positions: 0, orders: 0, tiers: vec![0; tiers], resting_accounts: 0 };
        for (id, account) in engine.accounts() {
            for market in account.positions.keys() {
                *counts.holders.entry(*market).or_default() += 1;
            }
            counts.positions += account.positions.len();
            counts.orders += account.orders.len();
            counts.resting_accounts += usize::from(!account.orders.is_empty());
            counts.tiers[engine.volume_tier(*id)] += 1;
        }
        counts
    }
}

/// What the engine reads back is what was written: the accounts, and each market's aggregates and
/// resting orders from the accounts'.
fn check(h: &Harness, mix: &Mix) {
    let engine = &h.engine;
    assert_eq!(engine.accounts().len(), mix.accounts);
    for (id, market) in engine.markets() {
        let positions = engine.accounts().values().filter_map(|account| account.positions.get(id));
        let (qty, cost) = positions.fold((Fixed::ZERO, Fixed::ZERO), |(q, c), p| (q + p.qty, c + p.cost()));
        assert_eq!((market.client_qty, market.client_cost), (qty, cost), "market {id}'s aggregates");
        let resting = engine.accounts().values().flat_map(|a| a.orders.values()).filter(|o| o.market == *id).count();
        assert_eq!(market.resting.len(), resting, "market {id}'s resting orders");
    }
}
