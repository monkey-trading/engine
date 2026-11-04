// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.26;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {Vault} from "./Vault.sol";

/// @title The engine's on-chain log heads (monkey-trading/engine#541).
/// @notice The enclave posts its newest released `(seq, link)` under an `engine.log-head.v1`
/// signature, signed off the sequencer. `link` is the cumulative command-prefix digest: a
/// running hash over every command's sealed bytes that checkpoints do not reset, so a posted head
/// commits to the whole history, not just the state since the last checkpoint. Every post names
/// its parent — the previously accepted head of the same signer epoch — and the contract requires
/// the parent to match what it holds, so a longer divergent copy cannot overwrite the anchor
/// without extending it. The first post of an epoch carries no parent: a signer rotation isolates
/// chains. A node restarting from a log the posted head does not hold — a truncation older than
/// the posting interval plus inclusion — refuses to start, naming the range, and anyone can watch
/// how far the posted head trails the node's signed head.
/// Trust: the same single accepted operator dependency as the vault's checkpoints — the attested
/// enclave key the vault names. There is no admin: the vault reference is immutable, and a new
/// vault deployment pairs with a new LogHeads.
contract LogHeads {
    Vault public immutable vault;

    struct Head {
        uint64 seq;
        bytes32 link;
    }

    /// The newest posted head per signer epoch; empty until the first post of that epoch.
    mapping(uint64 epoch => Head) public heads;

    event HeadPosted(uint64 indexed epoch, uint64 indexed seq, bytes32 link);

    error NotNewer(uint64 epoch, uint64 posted, uint64 held);
    error NotParent(uint64 epoch, uint64 parentSeq, bytes32 parentLink, uint64 heldSeq, bytes32 heldLink);
    error InvalidSigner();

    constructor(Vault vault_) {
        vault = vault_;
    }

    /// @notice Records the engine's newest released head for the current signer epoch, extending
    /// the head the contract already holds.
    /// @dev Anyone may relay the call; the signature, not the caller, is the authority. Gas is the
    /// relay incentive: a truncated log is the engine's and its clients' problem, and the post is
    /// what lets a restarted node refuse it. The parent requirement chains posts atomically: the
    /// signer rotation starts a fresh chain, so no parent is required while the epoch's head is
    /// empty.
    function post(uint64 seq, bytes32 link, uint64 parentSeq, bytes32 parentLink, bytes calldata signature) external {
        uint64 epoch = vault.signerEpoch();
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.log-head.v1", uint64(block.chainid), address(vault), epoch, seq, link, parentSeq, parentLink
            )
        );
        if (ECDSA.recover(digest, signature) != vault.checkpointSigner()) revert InvalidSigner();
        Head storage held = heads[epoch];
        if (held.seq != 0) {
            if (parentSeq != held.seq || parentLink != held.link) {
                revert NotParent(epoch, parentSeq, parentLink, held.seq, held.link);
            }
            if (seq <= held.seq) revert NotNewer(epoch, seq, held.seq);
        }
        held.seq = seq;
        held.link = link;
        emit HeadPosted(epoch, seq, link);
    }
}
