// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {LogHeads} from "../src/LogHeads.sol";
import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {Vault} from "../src/Vault.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";

/// The registry's vault and schedule, named together by the timelock and moved to another pair only
/// while rotation is off and no drawn turn runs; the pair it leaves cannot act on the one it follows.
contract CustodyTest is ScheduleTest {
    uint256 private constant NEXT_KEY = 0xbeef;

    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal carol = makeAddr("carol");

    function setUp() public {
        _deploySchedule();
    }

    function test_TheTimelockNamesTheVaultAndTheScheduleTogether() public {
        _deployRegistry();
        OperatorSchedule own = _newSchedule();
        vm.expectRevert(OperatorRegistry.UnauthorizedGovernance.selector);
        registry.setCustody(address(vault), own);
        vm.expectEmit(address(registry));
        emit OperatorRegistry.CustodySet(address(vault), address(own));
        vm.prank(address(governance));
        registry.setCustody(address(vault), own);
        assertEq(address(registry.vault()), address(vault));
        assertEq(address(registry.schedule()), address(own));
    }

    function test_TheScheduleMustBeThisRegistrysOverLogHeadsThatFollowTheVault() public {
        OperatorSchedule other = schedule;
        _deployRegistry();
        vm.expectRevert(OperatorRegistry.InvalidSchedule.selector);
        vm.prank(address(governance));
        registry.setCustody(address(vault), other);

        // This registry's schedule, over log heads that follow another vault.
        Vault elsewhere = _newVault(NEXT_KEY);
        OperatorSchedule unpaired = _scheduleOver(new LogHeads(elsewhere));
        vm.expectRevert(OperatorRegistry.InvalidSchedule.selector);
        vm.prank(address(governance));
        registry.setCustody(address(vault), unpaired);

        OperatorSchedule own = _newSchedule();
        vm.expectRevert(OperatorRegistry.ZeroAddress.selector);
        vm.prank(address(governance));
        registry.setCustody(makeAddr("no code"), own);
        assertEq(address(registry.schedule()), address(0));

        vm.prank(address(governance));
        registry.setCustody(address(elsewhere), unpaired);
        assertEq(address(registry.vault()), address(elsewhere));
    }

    function test_ARegistryFollowsAVaultAloneUntilItNamesASchedule() public {
        _deployRegistry();
        OperatorSchedule none = OperatorSchedule(address(0));
        vm.prank(address(governance));
        registry.setCustody(address(vault), none);
        assertEq(address(registry.vault()), address(vault));
        assertEq(address(registry.schedule()), address(0));

        OperatorSchedule own = _newSchedule();
        vm.prank(address(governance));
        registry.setCustody(address(vault), own);
        vm.expectRevert(OperatorRegistry.InvalidSchedule.selector);
        vm.prank(address(governance));
        registry.setCustody(address(vault), none);
        assertEq(address(registry.schedule()), address(own), "once named, a schedule stays named");
    }

    function test_TheCustodyMovesOnlyWhileRotationIsOff() public {
        (Vault next, OperatorSchedule nextSchedule) = _nextPair();
        _setRotation(true);
        vm.expectRevert(OperatorRegistry.RotationActive.selector);
        vm.prank(address(governance));
        registry.setCustody(address(next), nextSchedule);
        _setRotation(false);
        vm.prank(address(governance));
        registry.setCustody(address(next), nextSchedule);
        assertEq(address(registry.schedule()), address(nextSchedule));
    }

    function test_TheCustodyMovesOnlyOnceTheDrawnTurnsHaveRun() public {
        _rotateOne(alice);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        vm.prank(address(governance));
        registry.setRotation(false);
        (Vault next, OperatorSchedule nextSchedule) = _nextPair();
        uint64 until = schedule.turnsRunUntil();
        assertGt(until, block.timestamp);
        vm.expectRevert(abi.encodeWithSelector(OperatorRegistry.TurnsPending.selector, until));
        vm.prank(address(governance));
        registry.setCustody(address(next), nextSchedule);

        vm.warp(until);
        vm.prank(address(governance));
        registry.setCustody(address(next), nextSchedule);
        assertEq(address(registry.vault()), address(next));
        assertEq(registry.operatorGeneration(), 0, "the new schedule names no turn");
    }

    /// The move as the launch runbook batches it: the old pair stays on chain, and neither its schedule
    /// nor its log heads can act on the vault the registry now follows.
    function test_TheOldScheduleAndLogHeadsCannotActOnTheNewVault() public {
        Vault old = vault;
        OperatorSchedule oldSchedule = schedule;
        LogHeads oldHeads = logHeads;
        _runOneTurnAndStop(alice, bob);
        vm.prank(multisig);
        old.retireSigner();

        (Vault next, OperatorSchedule nextSchedule) = _nextPair();
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setCustody, (address(next), nextSchedule)));
        _govern(address(next), abi.encodeCall(Vault.setRegistry, (address(registry))));
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.installOperator, (carol, _key(carol), _digest(9), CONFIG))
        );
        assertEq(address(nextSchedule.logHeads().vault()), address(next));
        assertEq(address(oldHeads.vault()), address(old));

        // The new vault takes a promotion and a generation only from the registry's schedule.
        address standby = _freshStandbyKey();
        vm.startPrank(address(oldSchedule));
        vm.expectRevert(Vault.UnauthorizedSchedule.selector);
        next.promoteSigner(standby);
        vm.expectRevert(Vault.UnauthorizedSchedule.selector);
        next.reserveGeneration();
        vm.stopPrank();
        vm.expectRevert(OperatorSchedule.NoTurn.selector);
        oldSchedule.promote();

        // With rotation back on, the old set still draws in the old schedule, but its draw reaches the
        // new vault for a generation and is refused there.
        _setRotation(true);
        uint64 later = _nextTurn() + 1;
        _relay(later, keccak256("later seed"));
        vm.warp(oldSchedule.drawDeadline(later));
        vm.expectRevert(Vault.UnauthorizedSchedule.selector);
        oldSchedule.draw(later);
        assertEq(next.signerEpoch(), 1);
        assertEq(next.checkpointSigner(), vm.addr(NEXT_KEY));
        assertEq(next.generationCounter(), 0, "nothing reserved a generation on the new vault");

        // The old log heads follow the old vault, whose signer is retired: no key posts there, the new
        // vault's included.
        _expectHeadRefused(oldHeads, privateKeyOf[makeAddr("enclave")]);
        _expectHeadRefused(oldHeads, NEXT_KEY);
    }

    function test_TheTimelockMovesTheRegistryBackOnceIdle() public {
        Vault old = vault;
        OperatorSchedule oldSchedule = schedule;
        (Vault next, OperatorSchedule nextSchedule) = _nextPair();
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setCustody, (address(next), nextSchedule)));
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setCustody, (address(old), oldSchedule)));
        assertEq(address(registry.vault()), address(old));
        assertEq(address(registry.schedule()), address(oldSchedule));
    }

    /// @dev Registers and rotates `a` and `b`, draws the next turn, turns rotation off and waits out the
    /// turns.
    function _runOneTurnAndStop(address a, address b) private {
        _rotateTwo(a, b);
        uint64 turn = _nextTurn() + 1;
        _relay(turn, keccak256("seed"));
        _draw(turn);
        _setRotation(false);
        vm.warp(schedule.turnsRunUntil());
    }

    /// @dev A launch vault on another token with `key` as its signer, and a schedule for this registry
    /// over log heads that follow it.
    function _nextPair() private returns (Vault next, OperatorSchedule nextSchedule) {
        next = _newVault(NEXT_KEY);
        nextSchedule = _scheduleOver(new LogHeads(next));
    }

    function _newVault(uint256 key) private returns (Vault) {
        return new Vault(new MockUSDC(), vm.addr(key), governance, multisig, 7 days, 3 days, 1 days);
    }

    /// @dev A schedule for this registry and the receiver, over `heads`, with the test heartbeat rule.
    function _scheduleOver(LogHeads heads) private returns (OperatorSchedule) {
        return new OperatorSchedule(
            registry,
            receiver,
            heads,
            _liveness(),
            _livenessMinimums(),
            _livenessMaximums(),
            _fitting(_livenessMinimums(), _livenessMaximums(), SWITCH_BUDGET),
            SWITCH_BUDGET,
            BLOCK_MILLIS
        );
    }

    /// @dev A first head for the current signer epoch of `heads`' vault, signed by `key`, is refused.
    function _expectHeadRefused(LogHeads heads, uint256 key) private {
        uint64 epoch = heads.vault().signerEpoch();
        bytes32 link = keccak256("link");
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.log-head.v1",
                uint64(block.chainid),
                address(heads.vault()),
                epoch,
                uint64(1),
                link,
                uint64(0),
                bytes32(0)
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        heads.post(1, link, 0, bytes32(0), abi.encodePacked(r, s, v));
    }
}
