//! MONKEY staking and USDC distributions for V25.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::event::Reject;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::StakingConfig;

use crate::common::{Harness, f};

/// Activates V25, has the operator initialise staking once, mirrors a stake and an unstake under
/// their chain references with a refused duplicate each, and closes epoch 0 above the liquidity
/// floor once house capital lifts the distributable excess.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V25 });
    let config =
        StakingConfig { liquidity_floor: f("100"), epoch_ms: 600_000, start_ms: h.ts, cooldown_ms: 86_400_000 };
    h.operator_admin(AdminCommand::InitStaking(config));
    let again = h.operate(AdminCommand::InitStaking(StakingConfig {
        liquidity_floor: f("200"),
        epoch_ms: 600_000,
        start_ms: h.ts,
        cooldown_ms: 86_400_000,
    }));
    assert!(
        again.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("already"))),
        "{again:?}"
    );

    // Mirrors credit once under their chain reference; a repeat is refused, not credited twice.
    let staker = h.signer(80);
    let staked = h.cmd(Command::Stake {
        account: staker,
        amount: TokenUnits(1_000),
        block: 25_000_000,
        chain_ref: "0xstake:25:0".into(),
    });
    assert!(
        staked.iter().any(
            |e| matches!(e, Event::Staked { account, amount } if *account == staker && *amount == TokenUnits(1_000))
        ),
        "{staked:?}"
    );
    let repeat = h.cmd(Command::Stake {
        account: staker,
        amount: TokenUnits(1_000),
        block: 25_000_001,
        chain_ref: "0xstake:25:0".into(),
    });
    assert!(
        repeat.iter().any(|e| matches!(e, Event::StakingRejected { reason: Reject::DuplicateReference, .. })),
        "{repeat:?}"
    );
    let unstaked = h.cmd(Command::Unstake {
        account: staker,
        amount: TokenUnits(400),
        block: 25_000_002,
        chain_ref: "0xunstake:25:0".into(),
    });
    assert!(
        unstaked.iter().any(
            |e| matches!(e, Event::Unstaked { account, amount } if *account == staker && *amount == TokenUnits(400))
        ),
        "{unstaked:?}"
    );
    // An unstake above the stake is refused without mutating it.
    let over = h.cmd(Command::Unstake {
        account: staker,
        amount: TokenUnits(601),
        block: 25_000_003,
        chain_ref: "0xunstake:25:1".into(),
    });
    assert!(
        over.iter().any(|e| matches!(e, Event::StakingRejected { reason: Reject::InsufficientStake, .. })),
        "{over:?}"
    );

    // House capital lifts the excess over the floor; the first command after the epoch ends
    // distributes it to the sole staker in whole USDC base units, reducing house cash.
    h.cmd(Command::HouseCapital { amount: f("10000000"), chain_ref: "0xhouse:25".into() });
    let cash = h.engine.house_cash();
    h.advance(600_001);
    let closed = h.cmd(Command::Tick);
    assert!(
        closed.iter().any(|e| matches!(e, Event::DistributionAllocated { account, epoch: 0, amount }
            if *account == staker && amount.0 > 0)),
        "{closed:?}"
    );
    assert!(h.engine.house_cash() < cash, "the allocation reduces house cash");
    assert_eq!(h.engine.staked(staker), TokenUnits(600));
}
