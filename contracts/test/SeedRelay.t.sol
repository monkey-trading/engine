// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {IInbox, SeedRelay} from "../src/SeedRelay.sol";
import {SeedReceiver} from "../src/SeedReceiver.sol";
import {MockInbox} from "./mocks/MockInbox.sol";

/// The relay against recorded mainnet data (`test/fixtures/seed-relay/mainnet.json`, which
/// `scripts/seed-relay-fixtures.sh` regenerates): a finalized Fulu block, its state's branches to
/// `fork.current_version` and to two epochs' mixes, and the last block before a missed slot. Each
/// proven seed is checked against the prevRandao of the first execution block after its epoch, which
/// mainnet's execution layer records apart from the beacon state.
contract SeedRelayTest is Test {
    /// EIP-4788's contract, its runtime code as the EIP specifies it, and the address the protocol
    /// writes each block's parent beacon root from.
    address internal constant BEACON_ROOTS = 0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02;
    bytes internal constant BEACON_ROOTS_CODE =
        hex"3373fffffffffffffffffffffffffffffffffffffffe14604d57602036146024575f5ffd5b5f35801560495762001fff810690815414603c575f5ffd5b62001fff01545f5260205ff35b5f5ffd5b62001fff42064281555f359062001fff015500";
    address internal constant SYSTEM = 0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE;
    /// The ring buffer holds this many timestamps, about 27 hours of 12-second slots.
    uint64 internal constant HISTORY_BUFFER_LENGTH = 8191;
    bytes4 internal constant FULU = 0x06000000;
    bytes4 internal constant ELECTRA = 0x05000000;

    string internal json;
    uint64 internal genesis;
    uint64 internal turn;
    uint64 internal epoch;
    uint64 internal timestamp;
    MockInbox internal inbox;
    SeedRelay internal relay;
    SeedReceiver internal receiver;
    HeaderRoot internal hasher;

    function setUp() public {
        json = vm.readFile("test/fixtures/seed-relay/mainnet.json");
        genesis = uint64(vm.parseJsonUint(json, ".beaconGenesisTime"));
        turn = uint64(vm.parseJsonUint(json, ".turn"));
        epoch = uint64(vm.parseJsonUint(json, ".proof.epoch"));
        timestamp = uint64(vm.parseJsonUint(json, ".timestamp"));
        vm.etch(BEACON_ROOTS, BEACON_ROOTS_CODE);
        inbox = new MockInbox();
        (relay, receiver) = deploy(genesis, forks(FULU, ELECTRA));
        recordBeaconRoot(timestamp, vm.parseJsonBytes32(json, ".proof.beaconRoot"));
        hasher = new HeaderRoot(inbox, address(receiver), genesis, forks(FULU, ELECTRA));
    }

    function test_ProvesAMainnetEpochsMixAndSendsItToTheReceiver() public {
        assertEq(relay.seedEpoch(turn), epoch, "mainnet's schedule fixes the fixture's epoch");
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        assertEq(witness.seed, vm.parseJsonBytes32(json, ".firstBlockAfter.prevRandao"));
        assertEq(witness.seed, vm.parseJsonBytes32(json, ".firstBlockAfter.beaconApiRandao"));
        vm.warp(timestamp + 1 hours);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectEmit(address(relay));
        emit SeedRelay.SeedRelayed(turn, epoch, witness.seed, 0);
        uint256 ticket = relay.relay{value: value}(turn, witness, fees);

        MockInbox.Ticket memory sent = inbox.ticket(ticket);
        assertEq(sent.sender, address(relay));
        assertEq(sent.to, address(receiver));
        assertEq(sent.value, value);
        assertEq(sent.excessFeeRefundAddress, address(this), "unspent fees refund to the caller");
        assertEq(
            sent.callValueRefundAddress, address(receiver), "the receiver, which cannot cancel, is the beneficiary"
        );
        assertEq(sent.data, abi.encodeCall(SeedReceiver.receiveSeed, (turn, epoch, witness.seed)));

        deliver(sent);
        vm.warp(relay.drawDeadline(turn));
        assertEq(receiver.seedOf(turn), witness.seed);
    }

    function test_ProvesAMixFromAHeaderAtExactlyTheFirstSlotAfterTheEpoch() public {
        // The relayer anchors on a finalized checkpoint block, the first slot of an epoch, so a turn
        // first becomes ready with a header at slot (epoch + 1) * 32: the first slot the relay takes.
        // The header is synthetic, with the fixture's state and its root recorded in EIP-4788.
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        uint256 first = (uint256(epoch) + 1) * 32;
        witness.header.slot = uint64(first);
        recordBeaconRoot(timestamp, hasher.rootOf(witness.header));
        vm.warp(timestamp + 1 hours);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        uint256 ticket = relay.relay{value: value}(turn, witness, fees);
        assertEq(inbox.ticket(ticket).data, abi.encodeCall(SeedReceiver.receiveSeed, (turn, epoch, witness.seed)));

        // One slot earlier is still inside the epoch.
        witness.header.slot = uint64(first - 1);
        uint64 earlier = timestamp + 12;
        recordBeaconRoot(earlier, hasher.rootOf(witness.header));
        witness.timestamp = earlier;
        vm.warp(earlier + 1 hours);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.EpochNotEnded.selector, epoch, first - 1));
        relay.relay{value: value}(turn, witness, fees);
    }

    function test_RevertWhen_TheBlockIsTheLastBeforeAMissedSlot() public {
        uint64 missed = uint64(vm.parseJsonUint(json, ".missed.proof.epoch"));
        uint64 later = deployFor(missed);
        uint64 before = uint64(vm.parseJsonUint(json, ".missed.lastBlockBefore.timestamp"));
        recordBeaconRoot(before, vm.parseJsonBytes32(json, ".missed.lastBlockBefore.beaconRoot"));
        SeedRelay.Witness memory witness = witnessAt(".missed.proof", before);
        witness.header = headerAt(".missed.lastBlockBefore.header");
        // The first block of the next epoch follows a missed slot, so its parent is still in the epoch.
        assertLt(witness.header.slot, (uint256(missed) + 1) * 32);
        vm.warp(before + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.EpochNotEnded.selector, missed, witness.header.slot));
        relay.relay{value: value}(later, witness, fees);
    }

    function test_ProvesTheMixOfAnEpochThatEndedBeforeAMissedSlot() public {
        uint64 missed = uint64(vm.parseJsonUint(json, ".missed.proof.epoch"));
        uint64 later = deployFor(missed);
        SeedRelay.Witness memory witness = witnessAt(".missed.proof", timestamp);
        assertEq(witness.seed, vm.parseJsonBytes32(json, ".missed.firstBlockAfter.prevRandao"));
        assertGt(
            vm.parseJsonUint(json, ".missed.firstBlockAfter.slot"), (uint256(missed) + 1) * 32, "after the missed slot"
        );
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        uint256 ticket = relay.relay{value: value}(later, witness, fees);
        assertEq(inbox.ticket(ticket).data, abi.encodeCall(SeedReceiver.receiveSeed, (later, missed, witness.seed)));
    }

    function test_RevertWhen_TheWitnessProvesAnotherEpochsMix() public {
        // A turn whose epoch is one before the fixture's: the same branch sits at the wrong index.
        uint64 later = deployFor(epoch - 1);
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectRevert(SeedRelay.InvalidSeedProof.selector);
        relay.relay{value: value}(later, witness, fees);
    }

    function test_RevertWhen_TheEpochSharesTheMixIndexOfAnother() public {
        // `randao_mixes` reuses an index every 65,536 epochs; the slot bounds tell them apart.
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        uint64 later = deployFor(epoch + 65536);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.EpochNotEnded.selector, epoch + 65536, witness.header.slot));
        relay.relay{value: value}(later, witness, fees);
        later = deployFor(epoch - 65536);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.MixOverwritten.selector, epoch - 65536, witness.header.slot));
        relay.relay{value: value}(later, witness, fees);
    }

    function test_RevertWhen_TheBeaconRootHasLeftTheWindow() public {
        uint64 later = deployFor(epoch);
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.warp(timestamp + 1);
        relay.relay{value: value}(later, witness, fees);
        // 8191 slots on, the chain writes the same ring entry, and the block's root is gone.
        recordBeaconRoot(timestamp + HISTORY_BUFFER_LENGTH * 12, keccak256("a later block"));
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.BeaconRootUnavailable.selector, timestamp));
        relay.relay{value: value}(later, witness, fees);
        // A time no block had is refused the same way.
        witness.timestamp = timestamp + 1;
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.BeaconRootUnavailable.selector, timestamp + 1));
        relay.relay{value: value}(later, witness, fees);
    }

    function test_RevertWhen_TheChainHasNoBeaconRootContract() public {
        // A call to an address without code succeeds and returns nothing.
        vm.etch(BEACON_ROOTS, "");
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.BeaconRootUnavailable.selector, timestamp));
        relay.relay{value: value}(turn, witness, fees);
    }

    function test_RevertWhen_TheDrawDeadlineHasPassed() public {
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        uint64 deadline = relay.drawDeadline(turn);
        vm.warp(deadline);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.DrawDeadlinePassed.selector, turn, deadline));
        relay.relay{value: value}(turn, witness, fees);
    }

    function test_RevertWhen_TheHeaderIsNotTheBeaconRoots() public {
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        witness.header.proposerIndex += 1;
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectPartialRevert(SeedRelay.HeaderMismatch.selector);
        relay.relay{value: value}(turn, witness, fees);
        // The genuine header still proves.
        witness.header.proposerIndex -= 1;
        relay.relay{value: value}(turn, witness, fees);
    }

    function test_RevertWhen_TheStatesForkIsNotAccepted() public {
        uint64 later = deployFor(epoch, forks(ELECTRA, ELECTRA));
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.ForkNotAccepted.selector, FULU));
        relay.relay{value: value}(later, witness, fees);
        // Claiming the accepted fork does not prove it: the state's version is Fulu's.
        witness.forkVersion = ELECTRA;
        vm.expectRevert(SeedRelay.InvalidForkProof.selector);
        relay.relay{value: value}(later, witness, fees);
    }

    function testFuzz_RevertWhen_AnyPartOfAProofChanges(uint256 position, uint8 bit) public {
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        bytes32 flip = bytes32(uint256(1) << bit);
        position = bound(position, 0, 31);
        if (position < 22) {
            witness.seedProof[position] ^= flip;
            vm.expectRevert(SeedRelay.InvalidSeedProof.selector);
        } else if (position < 30) {
            witness.forkProof[position - 22] ^= flip;
            vm.expectRevert(SeedRelay.InvalidForkProof.selector);
        } else if (position == 30) {
            witness.seed ^= flip;
            vm.expectRevert(SeedRelay.InvalidSeedProof.selector);
        } else {
            witness.forkVersion ^= bytes4(uint32(1) << (bit % 32));
            // A version this relay does not accept is refused before its branch is read.
            if (relay.acceptedFork(witness.forkVersion)) vm.expectRevert(SeedRelay.InvalidForkProof.selector);
            else vm.expectRevert(abi.encodeWithSelector(SeedRelay.ForkNotAccepted.selector, witness.forkVersion));
        }
        relay.relay{value: value}(turn, witness, fees);
    }

    function test_RevertWhen_AProofHasTheWrongLength() public {
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        bytes32[] memory seedProof = witness.seedProof;
        witness.seedProof = new bytes32[](21);
        vm.expectRevert(SeedRelay.InvalidSeedProof.selector);
        relay.relay{value: value}(turn, witness, fees);
        witness.seedProof = seedProof;
        witness.forkProof = new bytes32[](9);
        vm.expectRevert(SeedRelay.InvalidForkProof.selector);
        relay.relay{value: value}(turn, witness, fees);
    }

    function test_ARelayedSeedSentTwiceIsRecordedOnce() public {
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        // Anyone may relay a turn again, in case a ticket fails; the receiver keeps the first.
        uint256 first = relay.relay{value: value}(turn, witness, fees);
        uint256 second = relay.relay{value: value}(turn, witness, fees);
        deliver(inbox.ticket(first));
        MockInbox.Ticket memory replay = inbox.ticket(second);
        vm.prank(receiver.relayAlias());
        (bool ok, bytes memory reason) = replay.to.call(replay.data);
        assertFalse(ok);
        assertEq(reason, abi.encodeWithSelector(SeedReceiver.AlreadyRelayed.selector, turn));
    }

    function test_RevertWhen_TheTicketIsUnderpaid() public {
        SeedRelay.Witness memory witness = witnessAt(".proof", timestamp);
        vm.warp(timestamp + 1);
        (SeedRelay.Fees memory fees, uint256 value) = feesFor(witness);
        vm.expectRevert(abi.encodeWithSelector(MockInbox.InsufficientValue.selector, value, value - 1));
        relay.relay{value: value - 1}(turn, witness, fees);
    }

    function test_RevertWhen_TheDeploymentNamesNothingToTrust() public {
        vm.expectRevert(SeedRelay.ZeroAddress.selector);
        new SeedRelay(IInbox(address(0)), address(receiver), genesis, forks(FULU, FULU));
        vm.expectRevert(SeedRelay.ZeroAddress.selector);
        new SeedRelay(inbox, address(0), genesis, forks(FULU, FULU));
        vm.expectRevert(SeedRelay.NoForks.selector);
        new SeedRelay(inbox, address(receiver), genesis, new bytes4[](0));
        vm.expectRevert(abi.encodeWithSelector(SeedRelay.ForkNotAccepted.selector, bytes4(0)));
        new SeedRelay(inbox, address(receiver), genesis, forks(FULU, bytes4(0)));
    }

    // ── Helpers ──

    /// A relay and receiver pair with `forks_`, as the deployment pairs them: the receiver names the
    /// relay's address before the relay exists.
    function deploy(uint64 genesis_, bytes4[] memory forks_) internal returns (SeedRelay, SeedReceiver) {
        address next = vm.computeCreateAddress(address(this), vm.getNonce(address(this)) + 1);
        SeedReceiver receiver_ = new SeedReceiver(next, genesis_);
        SeedRelay relay_ = new SeedRelay(inbox, address(receiver_), genesis_, forks_);
        assertEq(address(relay_), next);
        return (relay_, receiver_);
    }

    /// Redeploys with a genesis time that makes `target` the epoch of the turn two after the
    /// fixture's, whose draw deadline is two days later; returns that turn. The header's slots are
    /// mainnet's either way.
    function deployFor(uint64 target) internal returns (uint64) {
        return deployFor(target, forks(FULU, ELECTRA));
    }

    function deployFor(uint64 target, bytes4[] memory forks_) internal returns (uint64 later) {
        later = turn + 2;
        uint64 shifted =
            target > epoch ? genesis + 2 days - (target - epoch) * 384 : genesis + 2 days + (epoch - target) * 384;
        (relay, receiver) = deploy(shifted, forks_);
        assertEq(relay.seedEpoch(later), target);
    }

    /// What the EIP-4788 system call writes at the start of the block at `time`.
    function recordBeaconRoot(uint64 time, bytes32 root) internal {
        vm.warp(time);
        vm.prank(SYSTEM);
        (bool ok,) = BEACON_ROOTS.call(abi.encode(root));
        assertTrue(ok);
    }

    function witnessAt(string memory key, uint64 time) internal view returns (SeedRelay.Witness memory witness) {
        witness.timestamp = time;
        witness.header = headerAt(string.concat(key, ".header"));
        witness.forkVersion = bytes4(vm.parseJsonBytes(json, string.concat(key, ".forkVersion")));
        witness.forkProof = vm.parseJsonBytes32Array(json, string.concat(key, ".forkProof"));
        witness.seed = vm.parseJsonBytes32(json, string.concat(key, ".seed"));
        witness.seedProof = vm.parseJsonBytes32Array(json, string.concat(key, ".seedProof"));
    }

    function headerAt(string memory key) internal view returns (SeedRelay.BeaconBlockHeader memory) {
        return SeedRelay.BeaconBlockHeader({
            slot: uint64(vm.parseJsonUint(json, string.concat(key, ".slot"))),
            proposerIndex: uint64(vm.parseJsonUint(json, string.concat(key, ".proposerIndex"))),
            parentRoot: vm.parseJsonBytes32(json, string.concat(key, ".parentRoot")),
            stateRoot: vm.parseJsonBytes32(json, string.concat(key, ".stateRoot")),
            bodyRoot: vm.parseJsonBytes32(json, string.concat(key, ".bodyRoot"))
        });
    }

    /// The Inbox's own fee for the message, at 1 gwei for 100,000 gas on Arbitrum.
    function feesFor(SeedRelay.Witness memory witness) internal view returns (SeedRelay.Fees memory fees, uint256) {
        bytes memory message = abi.encodeCall(SeedReceiver.receiveSeed, (turn, epoch, witness.seed));
        fees = SeedRelay.Fees(inbox.calculateRetryableSubmissionFee(message.length, 0), 100_000, 1 gwei);
        return (fees, fees.maxSubmissionCost + fees.gasLimit * fees.maxFeePerGas);
    }

    /// What Arbitrum does with a ticket: calls its target from the sender's alias.
    function deliver(MockInbox.Ticket memory sent) internal {
        address alias_;
        unchecked {
            alias_ = address(uint160(sent.sender) + uint160(0x1111000000000000000000000000000000001111));
        }
        vm.prank(alias_);
        (bool ok,) = sent.to.call(sent.data);
        assertTrue(ok);
    }

    function forks(bytes4 a, bytes4 b) internal pure returns (bytes4[] memory list) {
        list = new bytes4[](2);
        (list[0], list[1]) = (a, b);
    }
}

/// `SeedRelay`'s header hashing, for a test to record a synthetic header's root.
contract HeaderRoot is SeedRelay {
    constructor(IInbox inbox_, address receiver_, uint64 genesis_, bytes4[] memory forks_)
        SeedRelay(inbox_, receiver_, genesis_, forks_)
    {}

    function rootOf(BeaconBlockHeader calldata header) external pure returns (bytes32) {
        return hashTreeRoot(header);
    }
}
