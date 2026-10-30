// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ExitVerifier} from "../src/ExitVerifier.sol";
import {ExitTest} from "./helpers/ExitTest.sol";

/// The exit against roots the engine computed. `crates/core/tests/fixtures/exit.json` holds the
/// engine's exit proofs for a trading session (positions in a USD, a euro and a yen market, an
/// approval, and a queued remainder) and a custody session (credited, pending, held and house
/// receipts); `crates/core/tests/exit/fixture.rs` checks it against the engine, including that each
/// claim is the engine's own equity.
contract ExitVectorTest is ExitTest {
    string private fixture;

    function setUp() public override {
        super.setUp();
        fixture = vm.readFile(string.concat(vm.projectRoot(), "/../crates/core/tests/fixtures/exit.json"));
    }

    function testTheEngineTradingProofsRegisterTheEnginesValuationApprovalAndQueuedRemainder() public {
        usdc.mint(address(vault), 1_000_000e6);
        _postFixture(".trading");
        _startFixtureExit(".trading");
        address account = vm.parseJsonAddress(fixture, ".trading.account.account");
        vault.registerExit(account, _account(".trading"), _positions(".trading"), _markets(".trading"));
        uint256 claim = vm.parseJsonUint(fixture, ".trading.claimUsdc");
        assertEq(vault.exitClaims(account), claim);

        ExitVerifier.Approval memory approval = ExitVerifier.Approval(
            vm.parseJsonAddress(fixture, ".trading.approval.account"),
            vm.parseJsonUint(fixture, ".trading.approval.amountUsdc"),
            uint64(vm.parseJsonUint(fixture, ".trading.approval.nonce")),
            uint64(vm.parseJsonUint(fixture, ".trading.approval.approvedSeq"))
        );
        vault.registerExitWithdrawal(approval, vm.parseJsonBytes32Array(fixture, ".trading.approval.siblings"));
        ExitVerifier.QueuedWithdrawal memory queued = ExitVerifier.QueuedWithdrawal(
            vm.parseJsonAddress(fixture, ".trading.queued.account"),
            _int128(".trading.queued.amount"),
            _int128(".trading.queued.remaining"),
            uint64(vm.parseJsonUint(fixture, ".trading.queued.requestedSeq")),
            uint64(vm.parseJsonUint(fixture, ".trading.queued.ticket"))
        );
        vault.registerExitQueued(queued, vm.parseJsonBytes32Array(fixture, ".trading.queued.siblings"));
        uint256 total = claim + approval.amount + vm.parseJsonUint(fixture, ".trading.queued.remainingUsdc");
        assertEq(vault.exitClaims(account), total);

        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(account), total);
        assertEq(usdc.balanceOf(account), total);
    }

    function testTheEngineCustodyProofsReserveTheirTermsAndRefundPendingDepositsToTheirPayer() public {
        address client = vm.parseJsonAddress(fixture, ".custody.account.account");
        address payer = vm.parseJsonAddress(fixture, ".custody.pending.payer");
        // The vault's deposits 0 to 3, as the engine's receipts name them.
        _deposit(client, client, 100e6);
        _deposit(payer, vm.parseJsonAddress(fixture, ".custody.pending.account"), 40e6);
        _deposit(makeAddr("sanctioned"), client, 30e6);
        usdc.mint(payer, 12e6);
        vm.startPrank(payer);
        usdc.approve(address(vault), 12e6);
        vault.depositToHouse(12e6);
        vm.stopPrank();
        _postFixture(".custody");
        _startFixtureExit(".custody");
        assertEq(vault.exitDepositCursor(), 4);
        assertEq(vault.exitRefundReserve(), 52e6);
        assertEq(vault.exitHeldReserve(), 30e6);

        vault.registerExit(client, _account(".custody"), _positions(".custody"), _markets(".custody"));
        assertEq(vault.exitClaims(client), vm.parseJsonUint(fixture, ".custody.claimUsdc"));
        assertEq(vault.exitClaims(client), 100e6, "unfinalised principal is the account's");
        _refundPending(".custody.pending");
        _refundPending(".custody.house");
        assertEq(usdc.balanceOf(payer), 52e6);
        assertEq(vault.exitRefundReserve(), 0);
        vm.warp(block.timestamp + WINDOW);
        assertEq(vault.claimExit(client), 100e6);
        assertEq(usdc.balanceOf(address(vault)), 30e6, "the held principal stays");
    }

    function _postFixture(string memory session) private {
        seq = uint64(vm.parseJsonUint(fixture, string.concat(session, ".seq")));
        bytes32 root = vm.parseJsonBytes32(fixture, string.concat(session, ".root"));
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2", uint64(block.chainid), address(vault), seq, root, false, uint64(0), bytes32(0)
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, digest);
        vault.postCheckpoint(seq, root, false, 0, 0, abi.encodePacked(r, s, v));
    }

    function _startFixtureExit(string memory session) private {
        string memory terms = string.concat(session, ".terms");
        vm.warp(block.timestamp + PERIOD);
        vault.startExit(
            ExitVerifier.ExitTerms(
                uint64(vm.parseJsonUint(fixture, string.concat(terms, ".depositCursor"))),
                _int128(string.concat(terms, ".pending")),
                _int128(string.concat(terms, ".held")),
                _int128(string.concat(terms, ".unheld")),
                _uint64s(string.concat(terms, ".gaps"))
            ),
            vm.parseJsonBytes32Array(fixture, string.concat(terms, ".siblings"))
        );
    }

    function _account(string memory session) private view returns (ExitVerifier.AccountState memory state) {
        string memory a = string.concat(session, ".account");
        state.balance = _int128(string.concat(a, ".balance"));
        state.unfinalisedDeposits = _int128(string.concat(a, ".unfinalisedDeposits"));
        state.withdrawalNonce = uint64(vm.parseJsonUint(fixture, string.concat(a, ".withdrawalNonce")));
        state.toxicityBps = int64(vm.parseJsonInt(fixture, string.concat(a, ".toxicityBps")));
        state.openPositions = uint32(vm.parseJsonUint(fixture, string.concat(a, ".openPositions")));
        state.toxicityFraction = _int128(string.concat(a, ".toxicityFraction"));
        state.toxicityHistory = _int128(string.concat(a, ".toxicityHistory"));
        state.siblings = vm.parseJsonBytes32Array(fixture, string.concat(a, ".siblings"));
    }

    function _positions(string memory session) private view returns (ExitVerifier.PositionState[] memory positions) {
        positions = new ExitVerifier.PositionState[](_count(session));
        for (uint256 i; i < positions.length; ++i) {
            string memory p = string.concat(session, ".positions[", vm.toString(i), "]");
            positions[i] = ExitVerifier.PositionState(
                uint32(vm.parseJsonUint(fixture, string.concat(p, ".market"))),
                _int128(string.concat(p, ".qty")),
                _int128(string.concat(p, ".entry")),
                vm.parseJsonBytes32Array(fixture, string.concat(p, ".siblings"))
            );
        }
    }

    function _markets(string memory session) private view returns (ExitVerifier.MarketState[] memory markets) {
        markets = new ExitVerifier.MarketState[](_count(session));
        for (uint256 i; i < markets.length; ++i) {
            string memory m = string.concat(session, ".markets[", vm.toString(i), "]");
            markets[i].market = uint32(vm.parseJsonUint(fixture, string.concat(m, ".market")));
            markets[i].marked = vm.parseJsonBool(fixture, string.concat(m, ".marked"));
            markets[i].mark = _int128(string.concat(m, ".mark"));
            markets[i].quoteCurrency = uint8(vm.parseJsonUint(fixture, string.concat(m, ".quoteCurrency")));
            markets[i].fxMarket = uint32(vm.parseJsonUint(fixture, string.concat(m, ".fxMarket")));
            markets[i].fxMarked = vm.parseJsonBool(fixture, string.concat(m, ".fxMarked"));
            markets[i].fxMark = _int128(string.concat(m, ".fxMark"));
            markets[i].clientQty = _int128(string.concat(m, ".clientQty"));
            markets[i].clientCost = _int128(string.concat(m, ".clientCost"));
            markets[i].stateHash = vm.parseJsonBytes32(fixture, string.concat(m, ".stateHash"));
            markets[i].siblings = vm.parseJsonBytes32Array(fixture, string.concat(m, ".siblings"));
        }
    }

    function _refundPending(string memory path) private {
        ExitVerifier.PendingDeposit memory pending = ExitVerifier.PendingDeposit(
            uint64(vm.parseJsonUint(fixture, string.concat(path, ".depositId"))),
            vm.parseJsonAddress(fixture, string.concat(path, ".account")),
            vm.parseJsonAddress(fixture, string.concat(path, ".payer")),
            _int128(string.concat(path, ".amount"))
        );
        vault.refundPendingDeposit(
            bytes(vm.parseJsonString(fixture, string.concat(path, ".chainRef"))),
            pending,
            vm.parseJsonBytes32Array(fixture, string.concat(path, ".siblings"))
        );
    }

    function _count(string memory session) private view returns (uint256) {
        return vm.parseJsonUint(fixture, string.concat(session, ".account.openPositions"));
    }

    function _uint64s(string memory path) private view returns (uint64[] memory values) {
        uint256[] memory raw = vm.parseJsonUintArray(fixture, path);
        values = new uint64[](raw.length);
        for (uint256 i; i < raw.length; ++i) {
            values[i] = uint64(raw[i]);
        }
    }

    function _int128(string memory path) private view returns (int128) {
        return int128(vm.parseJsonInt(fixture, path));
    }
}
