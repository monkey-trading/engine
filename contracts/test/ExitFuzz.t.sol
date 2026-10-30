// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Math} from "@openzeppelin-contracts/utils/math/Math.sol";

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {ExitTest} from "./helpers/ExitTest.sol";

/// Fuzzed exit maths: a position's valuation against a reference that rounds through exact
/// magnitudes, and the shortfall's pro-rata payment.
contract ExitFuzzTest is ExitTest {
    address internal trader = makeAddr("trader");
    uint32 internal constant MARKET = 7;
    uint32 internal constant FX_MARKET = 4;

    /// @notice One fuzzed position's inputs.
    struct Case {
        int128 balance;
        int128 qty;
        int128 entry;
        int128 mark;
        int128 fx;
        uint8 currency;
        bool marked;
    }

    /// @dev One position in a USD, euro, pound or yen market at any mark, with any FX mark: the
    /// claim is the balance plus the PnL, each step rounded toward negative infinity, floored at
    /// zero, in whole base units.
    function testFuzzAPositionIsValuedAsTheEngineRoundsIt(Case memory c) public {
        c.balance = int128(bound(c.balance, -1e27, 1e27));
        c.qty = int128(bound(c.qty, -1e24, 1e24));
        c.entry = int128(bound(c.entry, 1, 1e21));
        c.mark = int128(bound(c.mark, 1, 1e21));
        c.fx = int128(bound(c.fx, 1, 1e12));
        c.currency %= 4;
        assertEq(_claim(c), _expected(c));
    }

    /// @dev The verifier's claim for `c`'s account, proven against a posted tree of its three leaves.
    function _claim(Case memory c) private returns (uint256) {
        ExitVerifier.AccountState memory state;
        state.balance = c.balance;
        state.openPositions = 1;
        _leaf(_accountKey(trader), _accountHash(state));
        bytes32 positionKey = keccak256(abi.encodePacked(uint8(7), trader, MARKET));
        _leaf(positionKey, keccak256(abi.encode(c.qty, c.entry)));
        ExitVerifier.MarketState[] memory markets = new ExitVerifier.MarketState[](1);
        markets[0].market = MARKET;
        markets[0].marked = c.marked;
        markets[0].mark = c.marked ? c.mark : int128(0);
        markets[0].quoteCurrency = c.currency;
        markets[0].fxMarket = c.currency == 0 ? 0 : FX_MARKET;
        markets[0].fxMarked = c.currency != 0;
        markets[0].fxMark = c.currency == 0 ? int128(0) : c.fx;
        bytes32 marketKey = keccak256(abi.encodePacked(uint8(2), MARKET));
        _leaf(marketKey, _marketHash(markets[0]));
        bytes32 root = _post();
        state.siblings = _siblings(_accountKey(trader));
        markets[0].siblings = _siblings(marketKey);
        ExitVerifier.PositionState[] memory positions = new ExitVerifier.PositionState[](1);
        positions[0] = ExitVerifier.PositionState(MARKET, c.qty, c.entry, _siblings(positionKey));
        return vault.exitVerifier().accountClaim(root, trader, state, positions, markets);
    }

    /// @dev The claim `c` should value at.
    function _expected(Case memory c) private pure returns (uint256) {
        int256 price = c.marked ? c.mark : c.entry;
        int256 pnl = _floor((price - c.entry) * c.qty, 1e9);
        if (c.currency == 1 || c.currency == 2) pnl = _floor(pnl * c.fx, 1e9);
        if (c.currency == 3) pnl = _floor(pnl * 1e9, c.fx);
        int256 equity = c.balance + pnl;
        return equity > 0 ? uint256(equity) / 1e3 : 0;
    }

    /// @dev Any registered claims against any holdings: each is paid its claim times the pool over
    /// the total, rounded down, so together they never take more than the pool, and all in full
    /// when the vault covers them.
    function testFuzzAShortfallPaysEachClaimTheSameFractionAndNeverMoreThanTheVaultHolds(
        uint96[3] memory claims,
        uint96 held
    ) public {
        address[3] memory accounts = [makeAddr("a"), makeAddr("b"), makeAddr("c")];
        uint256 total;
        for (uint256 i; i < 3; ++i) {
            claims[i] = uint96(bound(claims[i], 1, 1e15));
            _addAccount(accounts[i], int128(uint128(claims[i])) * UNIT);
            total += claims[i];
        }
        held = uint96(bound(held, 1, 2 * total));
        usdc.mint(address(vault), held);
        ExitVerifier.ExitTerms memory terms = _addTerms(0, 0, 0);
        _post();
        _startExit(terms);
        (ExitVerifier.PositionState[] memory positions, ExitVerifier.MarketState[] memory markets) = _none();
        for (uint256 i; i < 3; ++i) {
            ExitVerifier.AccountState memory state;
            state.balance = int128(uint128(claims[i])) * UNIT;
            vault.registerExit(accounts[i], _withAccountProof(accounts[i], state), positions, markets);
        }
        vm.warp(block.timestamp + WINDOW);
        uint256 paid;
        uint256 pool = Math.min(held, total);
        for (uint256 i; i < 3; ++i) {
            uint256 amount = vault.claimExit(accounts[i]);
            assertEq(amount, Math.mulDiv(claims[i], pool, total));
            assertLe(amount, claims[i]);
            paid += amount;
        }
        assertLe(paid, held);
        if (held >= total) assertEq(paid, total);
        assertEq(vault.houseRemainder(), held - pool);
    }

    /// @dev `numerator / denominator` toward negative infinity, through magnitudes: a different route
    /// from the verifier's.
    function _floor(int256 numerator, int256 denominator) private pure returns (int256) {
        if (numerator >= 0) return numerator / denominator;
        uint256 magnitude = uint256(-numerator);
        return -int256(Math.ceilDiv(magnitude, uint256(denominator)));
    }

    function _marketHash(ExitVerifier.MarketState memory market) private pure returns (bytes32) {
        return keccak256(
            abi.encode(
                market.marked,
                market.mark,
                market.quoteCurrency,
                market.fxMarket,
                market.fxMarked,
                market.fxMark,
                market.clientQty,
                market.clientCost,
                market.stateHash
            )
        );
    }
}
