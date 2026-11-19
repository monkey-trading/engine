// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {StateTree} from "./StateTree.sol";

// The kind bytes of the receipt leaves the exit checks: a held deposit's from protocol `V77`, a
// pending deposit's, and an unfinalised credit's. A held deposit shares its byte with the queue leaf,
// and a caller may name any reference, an eight-byte one that reproduces a queue key included; what
// keeps one leaf from proving as the other is the value, four ABI words for a receipt to five for a
// queued request.
uint8 constant HELD_DEPOSIT_LEAF = 33;
uint8 constant PENDING_DEPOSIT_LEAF = 34;
uint8 constant UNFINALISED_CREDIT_LEAF = 35;

/// @notice The vault's record of each deposit's payer, amount and beneficiary, which a receipt the
/// exit root holds is checked against.
interface DepositRecords {
    function depositRecords(uint256 id) external view returns (address payer, uint96 amount);
    function depositAccounts(uint256 id) external view returns (address account);
}

/// @title ExitVerifier
/// @notice Checks the engine's state tree leaves the vault's exit pays against, and values an
/// account at the root's marks as the engine's equity does, counting its unfinalised principal only
/// where the deploying vault's own deposit records hold it. The vault deploys it, and alone records
/// through it each registered account's running equity and the credits counted into it. Amounts and
/// prices are `int128` in the engine's nine-decimal raw units; the leaf layouts are those
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

    /// @notice The exit terms leaf: one past the highest vault deposit id the root holds; the
    /// finalised custody frontier's deposit count, below which every deposit has settled, so the exit
    /// attributes only the ids from it to the cursor; the pending and held principal; the principal of
    /// the ids below the cursor that the root does not hold and the gaps do not name, which the exit
    /// reserves and keeps; the principal of the live receipts the root holds; and the gaps, the lowest
    /// 64 ids below the cursor the root does not hold, in increasing order, which only a
    /// reorganisation the engine was still applying leaves and which the exit refunds.
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

    /// @notice A receipt the root holds at a vault deposit id, as its leaf binds it: a receipt-backed
    /// pending deposit's, and from protocol `V77` a held one's or a credited receipt's whose principal
    /// still awaits finality; `account` is zero for the house. `chainRef` is the receipt's reference,
    /// which keys the leaf, and `siblings` prove it.
    struct ReceiptLeaf {
        bytes chainRef;
        uint64 depositId;
        address account;
        address payer;
        int128 amount;
        bytes32[] siblings;
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

    /// @notice The engine's fixed-point scale.
    int256 private constant SCALE = 1e9;
    /// @notice One USDC base unit in the engine's raw units.
    int256 private constant USDC_UNIT = 1e3;

    /// @notice The vault that deployed this verifier, whose deposit records receipts are checked
    /// against, and which alone registers claims through it.
    DepositRecords public immutable vault;

    /// @notice Each registered account's equity at the exit root in raw units: its balance less its
    /// unfinalised principal, its positions at their marks, and each counted credit's principal the
    /// vault holds.
    mapping(address account => int256 equity) public exitEquity;
    /// @notice The USDC base units of each registered account's equity already returned as its claim.
    mapping(address account => uint256 claimed) public exitClaimed;
    /// @notice Whether the vault has registered each account's equity, which its credits wait for.
    mapping(address account => bool registered) public exitRegistered;
    /// @notice The unfinalised credits counted into a claim, by vault deposit id, each once.
    mapping(uint64 depositId => bool counted) public creditCounted;

    error InvalidAccountProof();
    error AlreadyRegistered();
    error NotVault();
    error UnregisteredAccount(address account);
    /// @notice A market quoted in another currency has PnL and no FX mark to convert it at, which no
    /// engine state holds.
    error UnvaluedPosition(uint32 market);

    constructor() {
        vault = DepositRecords(msg.sender);
    }

    modifier onlyVault() {
        if (msg.sender != address(vault)) revert NotVault();
        _;
    }

    /// @notice Registers `account`'s equity at `root` for the vault and returns the USDC base units it
    /// claims: its balance less its unfinalised principal, with each position valued at its market's
    /// mark as the engine values it, floored at zero and rounded down. The proof must carry exactly
    /// the account leaf's `openPositions` positions, in strictly increasing markets, each with its
    /// market's leaf, so no position can be left out. The unfinalised principal joins the claim only
    /// through `creditClaim`. Each account registers once.
    /// Reverts with `NotVault` for any caller but the vault, `AlreadyRegistered` for an account
    /// registered before, `InvalidAccountProof` when any leaf fails to prove or the positions are
    /// incomplete or out of order, and `UnvaluedPosition` for a market it cannot value.
    function accountClaim(
        bytes32 root,
        address account,
        AccountState calldata state,
        PositionState[] calldata positions,
        MarketState[] calldata markets
    ) external onlyVault returns (uint256) {
        if (exitRegistered[account]) revert AlreadyRegistered();
        int256 equity = _provenEquity(root, account, state, positions, markets);
        exitRegistered[account] = true;
        return _claim(account, equity);
    }

    /// @notice The USDC base units `account` claims at `root` once its equity and `credits` are
    /// registered, without registering anything, so a wallet can check its proof before the exit and
    /// at any time during it: its balance less its unfinalised principal, with each position at its
    /// market's mark, plus the principal of each credit whose deposit the vault's records match,
    /// floored at zero and rounded down, as `accountClaim` and `creditClaim` together come to.
    /// `credits` must be in strictly increasing deposit ids, so none counts twice.
    /// Reverts with `InvalidAccountProof` when any leaf fails to prove, the positions are incomplete
    /// or out of order, or a credit is another account's or out of order, and `UnvaluedPosition` for
    /// a market it cannot value.
    function previewClaim(
        bytes32 root,
        address account,
        AccountState calldata state,
        PositionState[] calldata positions,
        MarketState[] calldata markets,
        ReceiptLeaf[] calldata credits
    ) external view returns (uint256) {
        int256 equity = _provenEquity(root, account, state, positions, markets);
        for (uint256 i; i < credits.length; ++i) {
            ReceiptLeaf calldata credit = credits[i];
            if (i > 0 && credit.depositId <= credits[i - 1].depositId) revert InvalidAccountProof();
            equity += _creditPrincipal(root, account, credit);
        }
        return _usdcClaim(equity);
    }

    /// @notice Counts `credits`, unfinalised credit leaves of the registered `account` at `root`, each
    /// once, into its equity for the vault, and returns the USDC base units its claim grows by: the
    /// principal of each credit whose deposit the vault's records match in payer, amount and
    /// beneficiary, recovering first any shortfall its equity had. A credit a reorganisation orphaned
    /// or moved counts and adds nothing, and one already counted is passed over, so batches may
    /// overlap. Credits register in any batches, so no list need fit one transaction, and anyone may
    /// submit them, since a credit only ever adds to its own account.
    /// Reverts with `NotVault` for any caller but the vault, `UnregisteredAccount` before the account's
    /// equity is registered, and `InvalidAccountProof` for an uncounted credit that does not prove or
    /// is another account's.
    function creditClaim(bytes32 root, address account, ReceiptLeaf[] calldata credits)
        external
        onlyVault
        returns (uint256)
    {
        if (!exitRegistered[account]) revert UnregisteredAccount(account);
        int256 held;
        for (uint256 i; i < credits.length; ++i) {
            ReceiptLeaf calldata credit = credits[i];
            if (creditCounted[credit.depositId]) continue;
            held += _creditPrincipal(root, account, credit);
            creditCounted[credit.depositId] = true;
        }
        return _claim(account, held);
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

    /// @notice Whether `leaf`, a receipt leaf of kind `kind` (`HELD_DEPOSIT_LEAF`,
    /// `PENDING_DEPOSIT_LEAF` or `UNFINALISED_CREDIT_LEAF`), proves against `root`, and whether the
    /// vault's records of its deposit id have its payer, amount and beneficiary, so the vault holds the
    /// deposit the root does. Any other kind proves nothing.
    function provesReceipt(bytes32 root, ReceiptLeaf calldata leaf, uint8 kind)
        external
        view
        returns (bool proven, bool matched)
    {
        proven = kind >= HELD_DEPOSIT_LEAF && kind <= UNFINALISED_CREDIT_LEAF && _provesReceipt(root, leaf, kind);
        matched = proven && _recorded(leaf);
    }

    /// @notice Whether `siblings` prove the leaf at `key` with `valueHash` against `root`, for the
    /// vault's claims and withdrawals.
    function provesLeaf(bytes32 root, bytes32 key, bytes32 valueHash, bytes32[] calldata siblings)
        external
        pure
        returns (bool)
    {
        return StateTree.proves(root, key, valueHash, siblings);
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

    /// @dev Adds `added` raw units to `account`'s equity and returns the USDC base units its claim
    /// grows by: what its equity, floored at zero and rounded down, holds beyond what it was already
    /// returned. Its equity only grows once registered, so neither ever falls.
    function _claim(address account, int256 added) private returns (uint256 grown) {
        int256 equity = exitEquity[account] + added;
        exitEquity[account] = equity;
        uint256 total = _usdcClaim(equity);
        grown = total - exitClaimed[account];
        exitClaimed[account] = total;
    }

    /// @dev The USDC base units `equity` raw units come to as a claim: floored at zero and rounded down.
    function _usdcClaim(int256 equity) private pure returns (uint256) {
        return equity > 0 ? uint256(equity / USDC_UNIT) : 0;
    }

    /// @dev The principal `credit`, an unfinalised credit leaf of `account` at `root`, adds to its
    /// equity: its amount where the vault's records match its deposit, and nothing where they do not.
    /// Reverts with `InvalidAccountProof` when it is another account's or does not prove.
    function _creditPrincipal(bytes32 root, address account, ReceiptLeaf calldata credit)
        private
        view
        returns (int256)
    {
        if (credit.account != account || !_provesReceipt(root, credit, UNFINALISED_CREDIT_LEAF)) {
            revert InvalidAccountProof();
        }
        return _recorded(credit) ? int256(credit.amount) : int256(0);
    }

    /// @dev Whether `leaf` proves against `root` under the leaf kind `kind`, keyed by its reference.
    function _provesReceipt(bytes32 root, ReceiptLeaf calldata leaf, uint8 kind) private pure returns (bool) {
        bytes32 key = keccak256(abi.encodePacked(kind, leaf.chainRef));
        bytes32 value = keccak256(abi.encode(leaf.depositId, leaf.account, leaf.payer, leaf.amount));
        return StateTree.proves(root, key, value, leaf.siblings);
    }

    /// @dev Whether the vault's records of deposit `leaf.depositId` have `leaf`'s payer, amount and
    /// beneficiary.
    function _recorded(ReceiptLeaf calldata leaf) private view returns (bool) {
        (address payer, uint96 amount) = vault.depositRecords(leaf.depositId);
        return payer == leaf.payer && int256(uint256(amount)) * USDC_UNIT == leaf.amount
            && vault.depositAccounts(leaf.depositId) == leaf.account;
    }

    /// @dev `account`'s equity at `root` in raw units, its unfinalised principal left out, once its
    /// account leaf and exactly its `openPositions` positions, in strictly increasing markets, prove
    /// with their markets.
    function _provenEquity(
        bytes32 root,
        address account,
        AccountState calldata state,
        PositionState[] calldata positions,
        MarketState[] calldata markets
    ) private pure returns (int256 equity) {
        if (positions.length != state.openPositions || markets.length != positions.length) {
            revert InvalidAccountProof();
        }
        bytes32 key = keccak256(abi.encodePacked(ACCOUNT_LEAF, account));
        if (!StateTree.proves(root, key, _accountHash(state), state.siblings)) revert InvalidAccountProof();
        equity = int256(state.balance) - state.unfinalisedDeposits;
        for (uint256 i; i < positions.length; ++i) {
            if (i > 0 && positions[i].market <= positions[i - 1].market) revert InvalidAccountProof();
            equity += _provenPnl(root, account, positions[i], markets[i]);
        }
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
