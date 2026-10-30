// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ExitVerifier} from "../../src/ExitVerifier.sol";
import {Vault} from "../../src/Vault.sol";
import {MockUSDC} from "../mocks/MockUSDC.sol";
import {GovernanceTest} from "./GovernanceTest.sol";
import {TreeBuilder} from "./TreeBuilder.sol";

/// @notice A vault with short exit timings and a state tree the test fills leaf by leaf, encoded as
/// `crates/core/src/merkle/` lays the leaves out, then posts as a checkpoint.
abstract contract ExitTest is GovernanceTest {
    uint256 internal constant KEY = 0x1234;
    uint64 internal constant PERIOD = 7 days;
    uint64 internal constant WINDOW = 3 days;
    uint64 internal constant STALE = 1 days;
    int128 internal constant UNIT = 1e3;

    MockUSDC internal usdc;
    Vault internal vault;
    bytes32[] internal keys;
    bytes32[] internal values;
    uint64 internal seq;

    function setUp() public virtual {
        vm.warp(1_800_000_000);
        governance = _deployGovernance();
        usdc = new MockUSDC();
        vault = new Vault(usdc, vm.addr(KEY), governance, multisig, 1e18, PERIOD, WINDOW, STALE);
    }

    /// @dev Deposits `amount` base units from `payer` for `account`.
    function _deposit(address payer, address account, uint256 amount) internal returns (uint256 id) {
        usdc.mint(payer, amount);
        vm.startPrank(payer);
        usdc.approve(address(vault), amount);
        id = vault.depositFor(account, amount);
        vm.stopPrank();
    }

    /// @dev Adds the leaf at `key` with `valueHash`.
    function _leaf(bytes32 key, bytes32 valueHash) internal {
        keys.push(key);
        values.push(valueHash);
    }

    function _addTerms(uint64 cursor, int128 pending, int128 held) internal returns (ExitVerifier.ExitTerms memory) {
        return _addTerms(cursor, 0, pending, held, 0, 0, new uint64[](0));
    }

    /// @dev Exit terms whose root does not hold the deposit ids `gaps` in the span from `base` to
    /// `cursor`, whose principal, with any unnamed, `unheld` carries, and whose live receipts hold
    /// `heldReceipts`.
    function _addTerms(
        uint64 cursor,
        uint64 base,
        int128 pending,
        int128 held,
        int128 unheld,
        int128 heldReceipts,
        uint64[] memory gaps
    ) internal returns (ExitVerifier.ExitTerms memory) {
        _leaf(
            keccak256(abi.encodePacked(uint8(24))),
            keccak256(abi.encode(cursor, base, pending, held, unheld, heldReceipts, gaps))
        );
        return ExitVerifier.ExitTerms(cursor, base, pending, held, unheld, heldReceipts, gaps);
    }

    /// @dev An account leaf with `balance` and no positions, toxicity, or unfinalised principal.
    function _addAccount(address account, int128 balance) internal returns (ExitVerifier.AccountState memory state) {
        state.balance = balance;
        _leaf(_accountKey(account), _accountHash(state));
    }

    function _addApproval(address account, uint256 amount, uint64 nonce)
        internal
        returns (ExitVerifier.Approval memory approval)
    {
        approval = ExitVerifier.Approval(account, amount, nonce, 1);
        _leaf(keccak256(abi.encodePacked(uint8(32), account, nonce)), keccak256(abi.encode(account, amount, nonce, 1)));
    }

    function _addQueued(address account, int128 remaining, uint64 requestedSeq)
        internal
        returns (ExitVerifier.QueuedWithdrawal memory queued)
    {
        queued = ExitVerifier.QueuedWithdrawal(account, remaining * 2, remaining, requestedSeq, requestedSeq);
        _leaf(
            keccak256(abi.encodePacked(uint8(33), requestedSeq)),
            keccak256(abi.encode(account, remaining * 2, remaining, requestedSeq, requestedSeq))
        );
    }

    function _addDistribution(address account, uint256 entitled) internal {
        _leaf(keccak256(abi.encodePacked(uint8(13), account)), keccak256(abi.encode(entitled)));
    }

    function _addPending(bytes memory chainRef, uint64 id, address account, address payer, int128 amount)
        internal
        returns (ExitVerifier.PendingDeposit memory pending)
    {
        pending = ExitVerifier.PendingDeposit(id, account, payer, amount);
        _leaf(keccak256(abi.encodePacked(uint8(34), chainRef)), keccak256(abi.encode(id, account, payer, amount)));
    }

    /// @dev Posts the tree as the next checkpoint and returns its root.
    function _post() internal returns (bytes32 root) {
        root = TreeBuilder.root(keys, values);
        bool hasParent = vault.hasCheckpoint();
        uint64 parentSeq = vault.latestSeq();
        bytes32 parentRoot = vault.latestRoot();
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(vault),
                ++seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, digest);
        vault.postCheckpoint(seq, root, hasParent, parentSeq, parentRoot, abi.encodePacked(r, s, v));
    }

    /// @dev The siblings proving the leaf at `key` in the tree as it stands.
    function _siblings(bytes32 key) internal view returns (bytes32[] memory) {
        for (uint256 i; i < keys.length; ++i) {
            if (keys[i] == key) return TreeBuilder.proof(keys, values, i);
        }
        revert("no such leaf");
    }

    /// @dev Waits out the exit period and starts the exit with `terms`.
    function _startExit(ExitVerifier.ExitTerms memory terms) internal {
        vm.warp(uint256(vault.lastCheckpointAt()) + PERIOD);
        vault.startExit(terms, _siblings(keccak256(abi.encodePacked(uint8(24)))));
    }

    function _withAccountProof(address account, ExitVerifier.AccountState memory state)
        internal
        view
        returns (ExitVerifier.AccountState memory)
    {
        state.siblings = _siblings(_accountKey(account));
        return state;
    }

    function _accountKey(address account) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(uint8(1), account));
    }

    /// @dev The account leaf's value hash, as `AccountLeaf::value_hash` computes it.
    function _accountHash(ExitVerifier.AccountState memory state) internal pure returns (bytes32) {
        bytes memory leaf = abi.encode(state.balance, state.withdrawalNonce, state.toxicityBps, state.openPositions);
        if (state.unfinalisedDeposits != 0) {
            return keccak256(
                bytes.concat(leaf, abi.encode(state.toxicityFraction, state.toxicityHistory, state.unfinalisedDeposits))
            );
        }
        if (state.toxicityFraction == 0 && state.toxicityHistory == 0) return keccak256(leaf);
        return keccak256(bytes.concat(leaf, abi.encode(state.toxicityFraction, state.toxicityHistory)));
    }

    function _none()
        internal
        pure
        returns (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets)
    {
        positions = new ExitVerifier.PositionState[](0);
        markets = new ExitVerifier.MarketState[](0);
    }
}
