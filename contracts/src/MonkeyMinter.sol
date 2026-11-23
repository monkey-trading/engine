// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {MonkeyToken} from "./MonkeyToken.sol";
import {StateTree} from "./StateTree.sol";
import {Vault} from "./Vault.sol";

/// @title MonkeyMinter
/// @notice MONKEY's only minter, for good: it deploys the token, and mints to an account what its
/// cumulative emission entitlement, proven against a root of the vault it follows, adds to what it
/// was already minted. One record of what each account was minted spans every vault it has
/// followed, so a later vault's log, which carries each account's entitlement, mints only the rest.
/// @notice The timelock names the vault. It moves it to another only once the guardian has retired
/// the current vault's signer, from which point that vault's roots claim nothing.
/// @dev Not upgradeable, and no pause stops a claim: the vault decides which of its roots claim
/// (`Vault.claimRoot`), and this contract mints only what the root proves beyond the record.
contract MonkeyMinter {
    /// @notice Emission periods between halvings, as the engine counts them.
    uint256 public constant PERIODS_PER_HALVING = 26_280;

    /// @notice The largest initial period budget the engine accepts: the one whose nominal maximum
    /// supply fits a `uint128`.
    uint256 public constant MAX_PERIOD_BUDGET = type(uint128).max / (2 * PERIODS_PER_HALVING);

    /// @notice The first byte of an emission entitlement leaf's key in the engine's state tree.
    uint8 private constant ENTITLEMENT_LEAF = 8;

    /// @notice The deployment's timelock, which alone names the vault.
    TimelockController public immutable governance;

    /// @notice MONKEY, which this contract alone mints, capped at the schedule's nominal maximum supply.
    MonkeyToken public immutable monkey;

    /// @notice The vault whose checkpoint roots mint; zero until the timelock names one.
    Vault public vault;

    /// @notice MONKEY base units minted to each account so far, across every vault followed.
    mapping(address account => uint256 claimed) public emissionsClaimed;

    /// @notice The timelock moved minting from `previous`, zero for the first, to `next`.
    event VaultSet(address indexed previous, address indexed next);

    /// @notice `amount` MONKEY base units minted to `account`, bringing its total to `entitled`, the
    /// entitlement checkpoint `seq` of the vault followed proves.
    event EmissionsClaimed(address indexed account, uint256 amount, uint256 entitled, uint64 seq);

    error InvalidGovernance();
    error UnauthorizedGovernance();
    error InvalidPeriodBudget(uint256 periodBudget);
    error InvalidVault();
    error SignerLive(address signer);
    error VaultNotSet();
    error InvalidEntitlementProof();
    error NothingToClaim(uint256 entitled, uint256 claimed);

    /// @param governance_ The deployment's timelock.
    /// @param periodBudget_ The engine's initial emission period budget in MONKEY base units, which
    /// sets the token's cap at `2 * PERIODS_PER_HALVING * periodBudget_`.
    constructor(TimelockController governance_, uint256 periodBudget_) {
        if (address(governance_).code.length == 0 || governance_.getMinDelay() == 0) revert InvalidGovernance();
        if (periodBudget_ == 0 || periodBudget_ > MAX_PERIOD_BUDGET) revert InvalidPeriodBudget(periodBudget_);
        governance = governance_;
        monkey = new MonkeyToken(2 * PERIODS_PER_HALVING * periodBudget_);
    }

    /// @notice Names the vault whose roots mint: a vault under this timelock, which answers
    /// `checkpointSigner()` as every later move reads it. The first needs no other condition; a later
    /// one only once the current vault's signer is retired (`checkpointSigner` zero), so no root of it
    /// claims.
    function setVault(Vault next) external {
        if (msg.sender != address(governance)) revert UnauthorizedGovernance();
        if (address(next).code.length == 0 || next.governance() != governance) revert InvalidVault();
        next.checkpointSigner();
        Vault previous = vault;
        if (address(previous) != address(0)) {
            address signer = previous.checkpointSigner();
            if (signer != address(0)) revert SignerLive(signer);
        }
        vault = next;
        emit VaultSet(address(previous), address(next));
    }

    /// @notice Mints to `account` the MONKEY its cumulative entitlement `entitled` adds to what it
    /// was already minted, once `siblings` prove that entitlement against the root of the followed
    /// vault's checkpoint `checkpointSeq` (`Vault.claimRoot`). Anyone may relay a claim; the tokens
    /// always go to `account`.
    /// @dev The record moves before the mint, and the token makes no call out.
    /// @return amount The base units minted.
    function claimEmissions(address account, uint256 entitled, uint64 checkpointSeq, bytes32[] calldata siblings)
        external
        returns (uint256 amount)
    {
        Vault followed = vault;
        if (address(followed) == address(0)) revert VaultNotSet();
        bytes32 root = followed.claimRoot(checkpointSeq);
        bytes32 key = keccak256(abi.encodePacked(ENTITLEMENT_LEAF, account));
        if (!StateTree.proves(root, key, keccak256(abi.encode(entitled)), siblings)) revert InvalidEntitlementProof();
        uint256 claimed = emissionsClaimed[account];
        if (entitled <= claimed) revert NothingToClaim(entitled, claimed);
        amount = entitled - claimed;
        emissionsClaimed[account] = entitled;
        emit EmissionsClaimed(account, amount, entitled, checkpointSeq);
        monkey.mint(account, amount);
    }
}
