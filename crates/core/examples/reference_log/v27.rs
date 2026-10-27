//! Authenticated staking mirror identities and completed scan anchors from V27.

use alloy_primitives::B256;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;

use crate::common::Harness;

/// Commits inherited mirror identities, a completed scan anchor, and a new stake and unstake.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V27 });
    let staker = h.signer(80);
    assert!(h.cmd(Command::StakingProgress { block: 25_000_003, block_hash: B256::repeat_byte(27) }).is_empty());
    let staked = h.cmd(Command::Stake {
        account: staker,
        amount: TokenUnits(100),
        block: 27_000_000,
        chain_ref: "0xstake:27:0".into(),
    });
    assert!(staked.iter().any(
        |event| matches!(event, Event::Staked { account, amount } if *account == staker && *amount == TokenUnits(100))
    ));
    let unstaked = h.cmd(Command::Unstake {
        account: staker,
        amount: TokenUnits(100),
        block: 27_000_001,
        chain_ref: "0xunstake:27:0".into(),
    });
    assert!(unstaked.iter().any(
        |event| matches!(event, Event::Unstaked { account, amount } if *account == staker && *amount == TokenUnits(100))
    ));
    assert!(h.cmd(Command::StakingProgress { block: 27_000_001, block_hash: B256::repeat_byte(28) }).is_empty());
}
