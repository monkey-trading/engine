// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {StateTree} from "./StateTree.sol";

/// @title ExitVerifier
/// @notice Checks the engine's state tree leaves the vault's exit pays against, and values an
/// account at the root's marks as the engine's equity does. It holds nothing; the vault deploys it.
/// Amounts and prices are `int128` in the engine's nine-decimal raw units; the leaf layouts are those
/// `crates/core/src/merkle/` documents.
contract ExitVerifier {
    /// @notice An account leaf's fields, with its root-to-leaf siblings.
    struct AccountState {
        int128 balance;
        int128 unfinalisedDeposits;
        uint64 withdrawalNonce;
        int64 toxicityBps;
        uint32 openPositions;
        int128 toxicityFraction;
        int128 toxicityHistory;
        bytes32[] siblings;
    }

    /// @notice One of the account's position leaves, with its siblings.
    struct PositionState {
        uint32 market;
        int128 qty;
        int128 entry;
        bytes32[] siblings;
    }

    /// @notice The leaf of a position's market, with its siblings. `quoteCurrency` is 0 for USD, 1
    /// for EUR, 2 for GBP and 3 for JPY.
    struct MarketState {
        uint32 market;
        bool marked;
        int128 mark;
        uint8 quoteCurrency;
        uint32 fxMarket;
        bool fxMarked;
        int128 fxMark;
        int128 clientQty;
        int128 clientCost;
        bytes32 stateHash;
        bytes32[] siblings;
    }

    /// @notice The exit terms leaf: one past the highest vault deposit id the root holds, the pending
    /// and held principal, the principal of the ids below the cursor the root does not hold (the
    /// named gaps included, which the exit reserves and of which it refunds only the named), and the
    /// lowest 64 of those ids, in increasing order, which only a reorganisation the engine was still
    /// applying leaves.
    struct ExitTerms {
        uint64 depositCursor;
        uint64 depositBase;
        int128 pending;
        int128 held;
        int128 unheld;
        int128 heldReceipts;
        uint64[] gaps;
    }

    /// @notice A queued withdrawal request's leaf.
    struct QueuedWithdrawal {
        address account;
        int128 amount;
        int128 remaining;
        uint64 requestedSeq;
        uint64 ticket;
    }

    /// @notice A receipt-backed pending deposit's leaf; `account` is zero for the house.
    struct PendingDeposit {
        uint64 depositId;
        address account;
        address payer;
        int128 amount;
    }

    /// @notice A withdrawal approval's leaf, as the vault's signed withdrawals prove it.
    struct Approval {
        address account;
        uint256 amount;
        uint64 nonce;
        uint64 approvedSeq;
    }

    uint8 private constant ACCOUNT_LEAF = 1;
    uint8 private constant MARKET_LEAF = 2;
    uint8 private constant POSITION_LEAF = 7;
    uint8 private constant DISTRIBUTION_LEAF = 13;
    uint8 private constant EXIT_TERMS_LEAF = 24;
    uint8 private constant APPROVAL_LEAF = 32;
    uint8 private constant QUEUE_LEAF = 33;
    uint8 private constant PENDING_DEPOSIT_LEAF = 34;

    /// @notice The engine's fixed-point scale.
    int256 private constant SCALE = 1e9;
    /// @notice One USDC base unit in the engine's raw units.
    int256 private constant USDC_UNIT = 1e3;

    error InvalidAccountProof();
    /// @notice A market quoted in another currency has PnL and no FX mark to convert it at, which no
    /// engine state holds.
    error UnvaluedPosition(uint32 market);

    /// @notice The USDC base units `account` exits with for its balance and positions at `root`: its
    /// equity with each position valued at its market's mark, as the engine values it, floored at
    /// zero and rounded down. The proof must carry exactly the account leaf's `openPositions`
    /// positions, in strictly increasing markets, each with its market's leaf, so no position can be
    /// left out.
    /// Reverts with `InvalidAccountProof` when any leaf fails to prove or the positions are
    /// incomplete or out of order, and with `UnvaluedPosition` for a market it cannot value.
    function accountClaim(
        bytes32 root,
        address account,
        AccountState calldata state,
        PositionState[] calldata positions,
        MarketState[] calldata markets
    ) external pure returns (uint256) {
        if (positions.length != state.openPositions || markets.length != positions.length) {
            revert InvalidAccountProof();
        }
        bytes32 key = keccak256(abi.encodePacked(ACCOUNT_LEAF, account));
        if (!StateTree.proves(root, key, _accountHash(state), state.siblings)) revert InvalidAccountProof();
        int256 equity = state.balance;
        for (uint256 i; i < positions.length; ++i) {
            if (i > 0 && positions[i].market <= positions[i - 1].market) revert InvalidAccountProof();
            equity += _provenPnl(root, account, positions[i], markets[i]);
        }
        return equity > 0 ? uint256(equity / USDC_UNIT) : 0;
    }

    /// @notice Whether `siblings` prove the exit terms leaf `terms` against `root`.
    function provesTerms(bytes32 root, ExitTerms calldata terms, bytes32[] calldata siblings)
        external
        pure
        returns (bool)
    {
        bytes32 key = keccak256(abi.encodePacked(EXIT_TERMS_LEAF));
        bytes32 value = keccak256(
            abi.encode(
                terms.depositCursor,
                terms.depositBase,
                terms.pending,
                terms.held,
                terms.unheld,
                terms.heldReceipts,
                terms.gaps
            )
        );
        return StateTree.proves(root, key, value, siblings);
    }

    /// @notice Whether `siblings` prove the queued request `queued` against `root`.
    function provesQueued(bytes32 root, QueuedWithdrawal calldata queued, bytes32[] calldata siblings)
        external
        pure
        returns (bool)
    {
        bytes32 key = keccak256(abi.encodePacked(QUEUE_LEAF, queued.requestedSeq));
        bytes32 value =
            keccak256(abi.encode(queued.account, queued.amount, queued.remaining, queued.requestedSeq, queued.ticket));
        return StateTree.proves(root, key, value, siblings);
    }

    /// @notice Whether `siblings` prove the pending deposit `pending`, keyed by its receipt's
    /// reference `chainRef`, against `root`.
    function provesPending(
        bytes32 root,
        bytes calldata chainRef,
        PendingDeposit calldata pending,
        bytes32[] calldata siblings
    ) external pure returns (bool) {
        bytes32 key = keccak256(abi.encodePacked(PENDING_DEPOSIT_LEAF, chainRef));
        bytes32 value = keccak256(abi.encode(pending.depositId, pending.account, pending.payer, pending.amount));
        return StateTree.proves(root, key, value, siblings);
    }

    /// @notice Whether `siblings` prove the withdrawal approval `approval` against `root`.
    function provesApproval(bytes32 root, Approval calldata approval, bytes32[] calldata siblings)
        external
        pure
        returns (bool)
    {
        bytes32 key = keccak256(abi.encodePacked(APPROVAL_LEAF, approval.account, approval.nonce));
        bytes32 value = keccak256(abi.encode(approval.account, approval.amount, approval.nonce, approval.approvedSeq));
        return StateTree.proves(root, key, value, siblings);
    }

    /// @notice Whether `siblings` prove `account`'s cumulative distribution `entitled` against `root`.
    function provesDistribution(bytes32 root, address account, uint256 entitled, bytes32[] calldata siblings)
        external
        pure
        returns (bool)
    {
        bytes32 key = keccak256(abi.encodePacked(DISTRIBUTION_LEAF, account));
        return StateTree.proves(root, key, keccak256(abi.encode(entitled)), siblings);
    }

    /// @dev The USD PnL of `account`'s `position` at its `market`'s mark, once both leaves prove
    /// against `root` and name the same market.
    function _provenPnl(bytes32 root, address account, PositionState calldata position, MarketState calldata market)
        private
        pure
        returns (int256)
    {
        if (market.market != position.market) revert InvalidAccountProof();
        bytes32 positionKey = keccak256(abi.encodePacked(POSITION_LEAF, account, position.market));
        bytes32 positionHash = keccak256(abi.encode(position.qty, position.entry));
        if (!StateTree.proves(root, positionKey, positionHash, position.siblings)) revert InvalidAccountProof();
        bytes32 marketKey = keccak256(abi.encodePacked(MARKET_LEAF, market.market));
        if (!StateTree.proves(root, marketKey, _marketHash(market), market.siblings)) revert InvalidAccountProof();
        return _usdPnl(position, market);
    }

    /// @dev The account leaf's value hash: the four-field struct, followed while the account holds
    /// unfinalised principal by its toxicity fraction, history and that principal, and otherwise,
    /// while either is nonzero, by its toxicity fraction and history.
    function _accountHash(AccountState calldata state) private pure returns (bytes32) {
        bytes memory leaf = abi.encode(state.balance, state.withdrawalNonce, state.toxicityBps, state.openPositions);
        if (state.unfinalisedDeposits != 0) {
            return keccak256(
                bytes.concat(leaf, abi.encode(state.toxicityFraction, state.toxicityHistory, state.unfinalisedDeposits))
            );
        }
        if (state.toxicityFraction == 0 && state.toxicityHistory == 0) return keccak256(leaf);
        return keccak256(bytes.concat(leaf, abi.encode(state.toxicityFraction, state.toxicityHistory)));
    }

    /// @dev The market leaf's value hash, `keccak256(abi.encode(MarketLeaf))`.
    function _marketHash(MarketState calldata market) private pure returns (bytes32) {
        return keccak256(
            bytes.concat(
                abi.encode(market.marked, market.mark, market.quoteCurrency, market.fxMarket, market.fxMarked),
                abi.encode(market.fxMark, market.clientQty, market.clientCost, market.stateHash)
            )
        );
    }

    /// @dev A position's price PnL in USD: `(mark - entry) * qty`, with the entry for a market not
    /// yet marked, rounded down, then converted at the market's FX mark rounded down, as
    /// `Position::unrealised` and `QuoteCurrency::to_usd` round from protocol `V5`.
    function _usdPnl(PositionState calldata position, MarketState calldata market) private pure returns (int256) {
        int256 mark = market.marked ? int256(market.mark) : int256(position.entry);
        int256 pnl = _floorDiv((mark - position.entry) * position.qty, SCALE);
        if (pnl == 0 || market.quoteCurrency == 0) return pnl;
        if (!market.fxMarked || market.fxMark <= 0 || market.quoteCurrency > 3) revert UnvaluedPosition(market.market);
        if (market.quoteCurrency == 3) return _floorDiv(pnl * SCALE, market.fxMark);
        return _floorDiv(pnl * market.fxMark, SCALE);
    }

    /// @dev `numerator / denominator` rounded toward negative infinity, for a positive denominator.
    function _floorDiv(int256 numerator, int256 denominator) private pure returns (int256 quotient) {
        quotient = numerator / denominator;
        if (numerator % denominator != 0 && numerator < 0) quotient -= 1;
    }
}
