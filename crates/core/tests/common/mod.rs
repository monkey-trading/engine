//! Shared harness for the protocol tests: a fixed genesis, a sequencer that stamps commands like
//! the node's, and helpers that read events back.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

mod client;
pub use client::*;
pub mod emissions;
pub mod reference;
pub mod stress;
mod versions;

use std::collections::BTreeMap;

use chrono::TimeZone;
use engine_core::auth::{ClientAuth, Operator, RequestAuth};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::eip712::{self, B256, cancel_hash, order_hash, withdrawal_hash};
use engine_core::event::Event;
use engine_core::market::{FinancingSpec, MarketSpec, PriceRules, QuoteCurrency, Tier};
use engine_core::params::{
    DEFAULT_MAX_RESTING_ORDERS, NotionalWeighting, ProtocolParams, Rollover, StressLevel, StressTrigger,
    ToxicityParams, ToxicityTier,
};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};
use k256::ecdsa::SigningKey;

pub const BTC: MarketId = 1;
pub const ETH: MarketId = 2;
pub const AAPL: MarketId = 3;
/// EUR-USD, listed by `Harness::list_fx`.
pub const EURUSD: MarketId = 4;
/// USD-JPY, quoted in yen at its own index, listed by `Harness::list_fx`.
pub const USDJPY: MarketId = 5;
/// Germany 40, quoted in euros at EUR-USD, listed by `Harness::list_fx`.
pub const DE40: MarketId = 6;
/// A market id nothing lists.
pub const UNLISTED: MarketId = 99;
pub const HOUSE_CAPITAL: i64 = 100_000_000;
/// The custody chain and vault the signed harness records.
pub const CHAIN_ID: u64 = 31_337;
pub const VAULT: AccountId = AccountId([0xaa; 20]);
/// The seed of the operator key the `V8` harness records.
pub const OPERATOR_SEED: u8 = 0x42;

/// A sequenced engine with three markets and 100m of house capital (none from `zero_house`), plus a
/// record of the log and of the cash it credited.
pub struct Harness {
    pub engine: Engine,
    pub log: Vec<Sequenced>,
    pub ts: Millis,
    pub deposits: Fixed,
    /// House capital credited at genesis.
    pub house_capital: Fixed,
    /// Keys `cmd` signs unsigned client requests with, by account.
    pub signers: BTreeMap<AccountId, SigningKey>,
}

impl Harness {
    /// No commands yet, at the harness start time.
    fn empty() -> Self {
        Harness {
            engine: Engine::new(),
            log: vec![],
            ts: start(),
            deposits: Fixed::ZERO,
            house_capital: Fixed::from_int(HOUSE_CAPITAL),
            signers: BTreeMap::new(),
        }
    }

    /// `V3` with signed requests under `CHAIN_ID` and `VAULT`.
    pub fn signed() -> Self {
        Harness::v3(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT })
    }

    /// The account of the key from `seed`, whose unsigned requests `cmd` signs from now on.
    pub fn signer(&mut self, seed: u8) -> AccountId {
        let client = Client::new(seed);
        self.signers.insert(client.account, client.key);
        client.account
    }

    /// Parameters, three markets, and any house capital.
    fn genesis(&mut self) {
        self.admin(AdminCommand::SetParams(params()));
        self.admin(AdminCommand::UpsertMarket(market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
        self.admin(AdminCommand::UpsertMarket(market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
        self.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, us_equities(), 2_000)));
        if self.house_capital.is_positive() {
            self.cmd(Command::HouseCapital { amount: self.house_capital, chain_ref: "genesis".into() });
        }
    }

    /// `v6`, then `V7`, which allows markets quoted in other currencies.
    pub fn v7() -> Self {
        let mut h = Harness::v6();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V7 });
        h
    }

    /// `v7` with `list_fx`'s markets.
    pub fn fx() -> Self {
        let mut h = Harness::v7();
        h.list_fx();
        h
    }

    /// Lists EUR-USD (USD-quoted), USD-JPY (quoted in yen and converted at its own index), both with
    /// 2% margin, and Germany 40 (5% margin) quoted in euros and converted at EUR-USD; all 24/7.
    pub fn list_fx(&mut self) {
        let usdjpy = market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, 200);
        let yen = QuoteCurrency::Jpy { fx_market: USDJPY };
        let eurusd = market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 200);
        for spec in [eurusd, MarketSpec { quote_currency: yen, ..usdjpy }, de40(500)] {
            // Once the log records an operator, the operator signs listings.
            match self.engine.operator() {
                Some(_) => self.operator_admin(AdminCommand::UpsertMarket(spec)),
                None => self.admin(AdminCommand::UpsertMarket(spec)),
            }
        }
    }

    /// Sequences and applies one command at the current time, recording it in the log. An unsigned
    /// client request from a `signer` account is signed first, with its next nonce and an expiry a
    /// minute ahead.
    pub fn cmd(&mut self, cmd: Command) -> Vec<Event> {
        let cmd = self.sign(cmd);
        let sequenced = Sequenced { seq: self.log.len() as u64, ts: self.ts, cmd };
        let events = self.engine.apply(&sequenced).expect("in sequence");
        self.log.push(sequenced);
        events
    }

    /// `cmd` signed by its account's key, if it is an unsigned client request from a `signer`.
    fn sign(&self, cmd: Command) -> Command {
        let domain = eip712::domain(CHAIN_ID, VAULT);
        let expires_at = self.ts + 60_000;
        let auth = |account: AccountId, hash: &dyn Fn(u64) -> B256| {
            let key = self.signers.get(&account)?;
            let nonce = self.engine.next_request_nonce(account);
            Some(RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash(nonce)) })
        };
        match cmd {
            Command::PlaceOrder(mut req) if req.auth.is_none() => {
                req.auth = auth(req.account, &|nonce| order_hash(&domain, &req, nonce, expires_at));
                Command::PlaceOrder(req)
            }
            Command::CancelOrder { account, order_id, auth: None } => {
                let auth = auth(account, &|nonce| cancel_hash(&domain, account, order_id, nonce, expires_at));
                Command::CancelOrder { account, order_id, auth }
            }
            Command::Withdraw { account, amount, auth: None } => {
                let auth = auth(account, &|nonce| withdrawal_hash(&domain, account, amount, nonce, expires_at));
                Command::Withdraw { account, amount, auth }
            }
            cmd => cmd,
        }
    }

    /// Applies `command` signed by the harness operator with its next nonce and an expiry a minute
    /// ahead.
    pub fn operate(&mut self, command: AdminCommand) -> Vec<Event> {
        let operator = Client { expires_at: self.ts + 60_000, ..Client::operator() };
        let nonce = self.engine.next_request_nonce(operator.account);
        self.cmd(operator.admin(nonce, command))
    }

    /// `operate` signed by the key from `seed` instead of the harness operator's.
    pub fn operate_as(&mut self, seed: u8, command: AdminCommand) -> Vec<Event> {
        let signer = Client { expires_at: self.ts + 60_000, ..Client::new(seed) };
        let nonce = self.engine.next_request_nonce(signer.account);
        self.cmd(signer.admin(nonce, command))
    }

    /// `operate` for a command that must apply.
    pub fn operator_admin(&mut self, command: AdminCommand) {
        let events = self.operate(command);
        assert!(events.contains(&Event::AdminApplied), "admin rejected: {events:?}");
    }

    /// Applies an admin command that must succeed.
    pub fn admin(&mut self, cmd: AdminCommand) {
        let events = self.cmd(Command::Admin(cmd));
        assert!(events.contains(&Event::AdminApplied), "admin rejected: {events:?}");
    }

    /// Moves the sequencer clock forward by `ms`.
    pub fn advance(&mut self, ms: Millis) {
        self.ts += ms;
    }

    /// Moves the sequencer clock forward to `ts`.
    pub fn advance_to(&mut self, ts: Millis) {
        assert!(ts >= self.ts);
        self.ts = ts;
    }

    /// Two venues quoting the same mid, stamped now.
    pub fn price(&mut self, market: MarketId, mid: &str) -> Vec<Event> {
        self.quotes(market, &[mid, mid])
    }

    /// One venue per mid, stamped now.
    pub fn quotes(&mut self, market: MarketId, mids: &[&str]) -> Vec<Event> {
        let quotes = mids
            .iter()
            .enumerate()
            .map(|(i, mid)| SourceQuote { source: format!("venue{i}"), mid: f(mid), ts: self.ts, contract: None })
            .collect();
        self.cmd(Command::Prices { market, quotes })
    }

    /// Credits a deposit under a fresh chain reference.
    pub fn deposit(&mut self, account: AccountId, amount: &str) {
        let chain_ref = format!("0xdeposit{}", self.log.len());
        let events = self.cmd(Command::Deposit { account, amount: f(amount), chain_ref });
        assert!(matches!(events[..], [Event::Deposited { .. }]), "deposit rejected: {events:?}");
        self.deposits += f(amount);
    }

    /// Places an order with client id 7.
    pub fn order(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: &str,
        kind: OrderKind,
    ) -> Vec<Event> {
        self.cmd(Command::PlaceOrder(OrderRequest {
            account,
            client_id: 7,
            market,
            side,
            qty: f(qty),
            kind,
            reduce_only: false,
            auth: None,
        }))
    }

    /// Places a market order quoted at `quoted`, with the house's slippage bound.
    pub fn market_order(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: &str,
        quoted: &str,
    ) -> Vec<Event> {
        self.order(account, market, side, qty, OrderKind::Market { quoted: f(quoted), max_slippage_bps: None })
    }

    /// The account's (equity, margin requirement) in USD at current marks.
    pub fn equity_and_margin(&self, account: AccountId) -> (Fixed, Fixed) {
        self.engine.equity_and_margin(&self.engine.accounts()[&account], None)
    }

    /// The account's cash balance.
    pub fn balance(&self, account: AccountId) -> Fixed {
        self.engine.accounts()[&account].balance
    }

    /// Cash is never created or destroyed: client balances plus house cash equal deposits plus
    /// house capital, less approved withdrawals.
    pub fn assert_conserved(&self, withdrawn: Fixed) {
        let balances: Fixed = self.engine.accounts().values().map(|a| a.balance).sum();
        let expected = self.deposits + self.house_capital - withdrawn;
        assert_eq!(balances + self.engine.house_cash(), expected);
    }
}

/// The operator the `V8` harness records: `Client::operator`'s key under `CHAIN_ID` and `VAULT`.
pub fn operator() -> Operator {
    Operator { address: Client::operator().account, chain_id: CHAIN_ID, vault: VAULT }
}

/// Parses a decimal.
pub fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}

/// A quote from `source` at `mid`, stamped `ts`.
pub fn quote(source: &str, mid: &str, ts: Millis) -> SourceQuote {
    SourceQuote { source: source.into(), mid: f(mid), ts, contract: None }
}

/// A distinct account per `n`.
pub fn acct(n: u8) -> AccountId {
    AccountId([n; 20])
}

/// 10% profit fee, 50% close-out, one stress level tripped at 10bps divergence, a 200bps move, or
/// 80% cap use, and a 3bps toxicity add-on from a 20bps score, with markouts averaged by notional
/// from 10,000 of neutral history over at most 10m.
pub fn params() -> ProtocolParams {
    ProtocolParams {
        profit_fee_bps: 1_000,
        closeout_level_bps: 5_000,
        house_total_cap_bps: 20_000,
        stress_levels: vec![
            StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None },
            StressLevel {
                size_spread_multiplier_bps: 20_000,
                min_spread_bps: 5,
                trigger: Some(StressTrigger {
                    divergence_bps: Some(10),
                    move_bps: Some(200),
                    utilisation_bps: Some(8_000),
                    divergence_share_bps: None,
                }),
            },
        ],
        stress_move_window_ms: 60_000,
        stress_cooldown_ms: 300_000,
        toxicity: ToxicityParams {
            horizon_ms: 1_000,
            ewma_weight_bps: 5_000,
            notional: Some(NotionalWeighting { prior: Fixed::from_int(10_000), memory: Fixed::from_int(10_000_000) }),
            tiers: vec![ToxicityTier { min_score_bps: 20, extra_spread_bps: 3 }],
        },
        rollover: Rollover { tz: chrono_tz::Europe::London, hour: 22, minute: 0 },
        house_total_cap_floor: Fixed::ZERO,
        max_resting_orders: DEFAULT_MAX_RESTING_ORDERS,
    }
}

/// A market with one margin rate, size spreads of 0 / 2 / 5 bps at 1m and 5m, a 20bps slippage
/// bound, and a house cap of 50% of NAV.
pub fn market(id: MarketId, symbol: &str, class: AssetClass, schedule: Schedule, margin_bps: i64) -> MarketSpec {
    MarketSpec {
        id,
        symbol: symbol.into(),
        asset_class: class,
        quote_currency: QuoteCurrency::Usd,
        schedule,
        margin_tiers: vec![Tier { up_to: None, bps: margin_bps }],
        spread_tiers: vec![
            Tier { up_to: Some(Fixed::from_int(1_000_000)), bps: 0 },
            Tier { up_to: Some(Fixed::from_int(5_000_000)), bps: 2 },
            Tier { up_to: None, bps: 5 },
        ],
        price_rules: PriceRules { max_age_ms: 1_000, max_divergence_bps: stress::MAX_DIVERGENCE_BPS, min_sources: 2 },
        max_slippage_bps: 20,
        financing: FinancingSpec { benchmark: "SOFR".into(), markup_bps: 250, day_basis: 365 },
        house_cap_bps: 5_000,
        min_order_notional: Fixed::ZERO,
        house_cap_floor: Fixed::ZERO,
        contracts: vec![],
    }
}

/// Germany 40, 24/7, quoted in euros and converted at EUR-USD, with one margin rate.
pub fn de40(margin_bps: i64) -> MarketSpec {
    let spec = market(DE40, "DE40", AssetClass::Index, Schedule::AlwaysOpen, margin_bps);
    MarketSpec { quote_currency: QuoteCurrency::Eur { fx_market: EURUSD }, ..spec }
}

/// US equity exchange hours, Monday to Friday.
pub fn us_equities() -> Schedule {
    let windows: Vec<_> = ["Mon", "Tue", "Wed", "Thu", "Fri"]
        .iter()
        .map(|d| serde_json::json!({ "open": format!("{d} 09:30"), "close": format!("{d} 16:00") }))
        .collect();
    serde_json::from_value(serde_json::json!({ "kind": "weekly", "tz": "America/New_York", "windows": windows }))
        .unwrap()
}

/// Tue 20 Oct 2026 10:00 New York (US equities open).
pub fn start() -> Millis {
    ny(2026, 10, 20, 10, 0)
}

/// A New York wall-clock time as epoch millis.
pub fn ny(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}

pub mod events;
