//! Protocol-version constructors for the shared harness.

use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::AdminCommand;
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::StakingConfig;
use engine_core::types::Millis;

use crate::common::{Harness, operator};

impl Harness {
    /// Genesis: parameters, BTC and ETH (24/7, 10% margin), AAPL (US hours, 20% margin), and house
    /// capital, at Tue 20 Oct 2026 10:00 New York.
    pub fn new() -> Self {
        let mut h = Harness::empty();
        h.genesis();
        h
    }

    /// `new`'s genesis under `V3`, after recording `auth`.
    pub fn v3(auth: ClientAuth) -> Self {
        let mut h = Harness::empty();
        h.admin(AdminCommand::SetClientAuth(auth));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V3 });
        h.genesis();
        h
    }

    /// `v3`, then `V4`, which allows dated schedule exceptions and zero house capital moves.
    pub fn v4(auth: ClientAuth) -> Self {
        let mut h = Harness::v3(auth);
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V4 });
        h
    }

    /// `v4` taking unsigned requests, then `V5`, which rounds position accounting in the house's
    /// favour.
    pub fn v5() -> Self {
        let mut h = Harness::v4(ClientAuth::Unsigned);
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V5 });
        h
    }

    /// `v5`, then `V6`, which floors the house exposure caps and checks close-outs after financing
    /// and parameter or listing changes.
    pub fn v6() -> Self {
        let mut h = Harness::v5();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V6 });
        h
    }

    /// `version`, with unsigned requests, and a house that starts with no capital, so house NAV is
    /// zero until traders win or lose.
    pub fn zero_house(version: ProtocolVersion) -> Self {
        let mut h = Harness::empty();
        h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
        h.admin(AdminCommand::ActivateProtocol { version });
        h.house_capital = Fixed::ZERO;
        h.genesis();
        h
    }

    /// `new`'s genesis under `V8`, after recording `auth` and the harness operator.
    pub fn v8(auth: ClientAuth) -> Self {
        let mut h = Harness::empty();
        h.admin(AdminCommand::SetClientAuth(auth));
        h.admin(AdminCommand::SetOperator(operator()));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 });
        h.genesis();
        h
    }

    /// `v8` taking unsigned requests, then `V9`, whose state root is the state tree's.
    pub fn v9() -> Self {
        let mut h = Harness::v8(ClientAuth::Unsigned);
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 });
        h
    }

    /// `v9`, then `V10`, which rounds spreads and margin in the house's favour.
    pub fn v10() -> Self {
        let mut h = Harness::v9();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V10 });
        h
    }

    /// `v10`, then `V11`, which averages markouts by notional, to nine decimals of a bp, against the
    /// index when they are scored.
    pub fn v11() -> Self {
        let mut h = Harness::v10();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V11 });
        h
    }

    /// `v11`, then `V12`, which brings every market to the sequenced time before each command and
    /// counts a venue quote stamped ahead at once.
    pub fn v12() -> Self {
        let mut h = Harness::v11();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V12 });
        h
    }

    /// `v12`, then `V13`, which re-evaluates stress before every fill and after every command, and on
    /// activation turns the harness parameters' divergence trigger into a share of the market bound.
    pub fn v13() -> Self {
        let mut h = Harness::v12();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V13 });
        h
    }

    /// `v13`, then `V14`, which finances only markets whose benchmark has a rate and caps each
    /// account's resting orders.
    pub fn v14() -> Self {
        let mut h = Harness::v13();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V14 });
        h
    }

    /// `v14`, then `V15`, which lets an order that can only reduce the account's position in its
    /// market skip the minimum-notional check at placement.
    pub fn v15() -> Self {
        let mut h = Harness::v14();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V15 });
        h
    }

    /// `v15`, then `V16`, which clamps each account's toxicity history to a lowered memory.
    pub fn v16() -> Self {
        let mut h = Harness::v15();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V16 });
        h
    }

    /// `v16`, then `V17`, which keeps a market's move window across a halt.
    pub fn v17() -> Self {
        let mut h = Harness::v16();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 });
        h
    }

    /// `v17`, then `V18` and `V19`, with an emission schedule the operator signs that starts now
    /// with `period_budget` base units a period.
    pub fn emitting(period_budget: u128) -> Self {
        let mut h = Harness::v17();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V19 });
        let schedule = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(period_budget) };
        h.operator_admin(AdminCommand::InitEmissions(schedule));
        h
    }

    /// `v17`, then `V18`'s dated benchmark rates, `V19`, `V20`, and `V21`, which refuses quotes
    /// stamped outside a market's hours and prices markets from roll-adjusted futures contracts.
    pub fn v21() -> Self {
        let mut h = Harness::v17();
        for version in [ProtocolVersion::V18, ProtocolVersion::V19, ProtocolVersion::V20, ProtocolVersion::V21] {
            h.admin(AdminCommand::ActivateProtocol { version });
        }
        h
    }

    /// `v21`, then `V25`, with a staking config the operator signs that starts now with the given
    /// liquidity floor, epoch length, and unstake cooldown.
    pub fn staking(liquidity_floor: Fixed, epoch_ms: Millis, cooldown_ms: Millis) -> Self {
        let mut h = Harness::v21();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V25 });
        let config = StakingConfig { liquidity_floor, epoch_ms, start_ms: h.ts, cooldown_ms };
        h.operator_admin(AdminCommand::InitStaking(config));
        h
    }

    /// `v21`, then `V22`, `V23`, and `V33`, which keeps a quote stale under a market's old freshness
    /// limit stale across a relisting, bounds the held-quote closure check after a long gap, and
    /// bounds the rollovers and corporate boundaries one command catches up on.
    pub fn v33() -> Self {
        Harness::v21().into_v33()
    }

    /// This harness, after `V22`, `V23`, and `V33`.
    pub fn into_v33(mut self) -> Self {
        for version in [ProtocolVersion::V22, ProtocolVersion::V23, ProtocolVersion::V33] {
            self.admin(AdminCommand::ActivateProtocol { version });
        }
        self
    }

    /// `v33`, then `V35`, which bounds the operator's profit fee and close-out level changes, price
    /// rule changes, and key rotation. Its launch rates are the genesis parameters'.
    pub fn v35() -> Self {
        let mut h = Harness::v33();
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V35 });
        h
    }

    /// `staking` with a one-day epoch and a three-day cooldown over a 1m floor, then `V37`, whose
    /// operator key follows the operator registry's mirrored elections.
    pub fn v37() -> Self {
        const DAY_MS: Millis = 24 * 60 * 60 * 1000;
        let mut h = Harness::staking(Fixed::from_int(1_000_000), DAY_MS, 3 * DAY_MS);
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V37 });
        h
    }
}
