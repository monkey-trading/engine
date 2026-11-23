//! What the chain says of a move to this node's vault, read at one finalized block through the custody quorum:
//! the source vault's signer and last accepted checkpoint, the registry's vault, rotation switch and operator in
//! effect, and the vault MONKEY's minter follows.

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};

use crate::custody::{Quorum, VaultReader};

// The getters the start reads, as `contracts/src/Vault.sol`, `OperatorRegistry.sol` and `MonkeyMinter.sol`
// declare them.
sol! {
    function checkpointSigner() external view returns (address);
    function hasCheckpoint() external view returns (bool);
    function latestSeq() external view returns (uint64);
    function checkpointRoots(uint64 seq) external view returns (bytes32);
    function vault() external view returns (address);
    function rotationSwitch() external view returns (bool);
    function operatorInEffect() external view returns (address candidate, address adminKey, bytes32 release, uint64 since);
}

/// The contracts a move's start reads, besides the custody vault the quorum is bound to.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MoveContracts {
    /// The source log's vault.
    pub source: Address,
    pub registry: Address,
    pub minter: Option<Address>,
}

/// What the chain says of the move at one finalized block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MoveState {
    /// The finalized block read.
    pub block: u64,
    /// The source vault's checkpoint signer: zero once the guardian has retired it.
    pub source_signer: Address,
    /// The source vault's last accepted checkpoint and its root, if it holds one.
    pub source_checkpoint: Option<(u64, B256)>,
    pub registry_vault: Address,
    pub rotation_switch: bool,
    /// The admin key of the registry's operator in effect.
    pub admin_key: Address,
    /// The vault MONKEY's minter mints against.
    pub minter_vault: Option<Address>,
}

/// The move's state at the quorum's finalized block, from providers that serve `chain_id` and a vault holding
/// `token`.
/// Error cases:
/// - the providers lack an identity, finality or call quorum, or name no finalized block
/// - they serve another chain or token than the configured deployment's
/// - a contract answers what does not decode as its getter's return
pub(crate) async fn read<R: VaultReader + Sync>(
    quorum: &Quorum<R>,
    contracts: MoveContracts,
    (chain_id, token): (u64, Address),
) -> anyhow::Result<MoveState> {
    let identity = quorum.identity().await?;
    let (served, held) = (identity.chain_id, identity.token);
    ensure!(
        (served, held) == (chain_id, Some(token)),
        "the providers serve chain {served} and token {held:?}, not the configured chain {chain_id} and token {token}"
    );
    let at = quorum.withdrawal_finality().await?.tagged.context("custody providers name no finalized block")?;
    let call = |operation, to, data: Vec<u8>| quorum.call_as(operation, at, to, data.into());
    let MoveContracts { source, registry, minter } = contracts;
    let source_signer = checkpointSignerCall::abi_decode_returns(
        &call("carry_source_signer", source, checkpointSignerCall {}.abi_encode()).await?,
    )
    .context("the source vault's checkpointSigner")?;
    let held = hasCheckpointCall::abi_decode_returns(
        &call("carry_source_checkpoint", source, hasCheckpointCall {}.abi_encode()).await?,
    )
    .context("the source vault's hasCheckpoint")?;
    let source_checkpoint = if held {
        let seq =
            latestSeqCall::abi_decode_returns(&call("carry_source_seq", source, latestSeqCall {}.abi_encode()).await?)
                .context("the source vault's latestSeq")?;
        let root = checkpointRootsCall::abi_decode_returns(
            &call("carry_source_root", source, checkpointRootsCall { seq }.abi_encode()).await?,
        )
        .context("the source vault's checkpointRoots")?;
        Some((seq, root))
    } else {
        None
    };
    let registry_vault =
        vaultCall::abi_decode_returns(&call("carry_registry_vault", registry, vaultCall {}.abi_encode()).await?)
            .context("the registry's vault")?;
    let rotation_switch = rotationSwitchCall::abi_decode_returns(
        &call("carry_registry_rotation", registry, rotationSwitchCall {}.abi_encode()).await?,
    )
    .context("the registry's rotationSwitch")?;
    let operator = operatorInEffectCall::abi_decode_returns(
        &call("carry_registry_operator", registry, operatorInEffectCall {}.abi_encode()).await?,
    )
    .context("the registry's operatorInEffect")?;
    let minter_vault = match minter {
        Some(minter) => Some(
            vaultCall::abi_decode_returns(&call("carry_minter_vault", minter, vaultCall {}.abi_encode()).await?)
                .context("the minter's vault")?,
        ),
        None => None,
    };
    Ok(MoveState {
        block: at.number,
        source_signer,
        source_checkpoint,
        registry_vault,
        rotation_switch,
        admin_key: operator.adminKey,
        minter_vault,
    })
}
