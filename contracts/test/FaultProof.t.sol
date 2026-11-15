// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

import {FaultProof} from "../src/FaultProof.sol";
import {LogHeads} from "../src/LogHeads.sol";
import {Vault} from "../src/Vault.sol";
import {LivenessTest} from "./helpers/LivenessTest.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// `FaultProof.prove`: two heads of one signer epoch, turn, position and release that disagree at one
/// seq name the assignment's member once; every other pair, and a pair across a promotion's signer
/// change, proves nothing.
contract FaultProofTest is LivenessTest {
    FaultProof internal faults;
    address internal anyone = makeAddr("anyone");

    function setUp() public {
        _deployLiveness();
        faults = new FaultProof(vault);
        _startTurn();
    }

    /// @dev Two heads at the next seq with different links, as a rolled-back log would have the enclave
    /// sign, each built as the node builds a live head.
    function _conflict() internal view returns (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) {
        a = _nextHead();
        b = _nextHead();
        b.link = keccak256(abi.encode("rolled back", a.seq));
        b.signedAt = a.signedAt + 1;
    }

    function _prove(LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) internal returns (address) {
        vm.prank(anyone);
        return faults.prove(vault.signerEpoch(), a, _sign(a, _signerKey()), b, _sign(b, _signerKey()));
    }

    function _expectRevert(bytes memory reason, LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) internal {
        bytes memory sigA = _sign(a, _signerKey());
        bytes memory sigB = _sign(b, _signerKey());
        uint64 epoch = vault.signerEpoch();
        vm.expectRevert(reason);
        faults.prove(epoch, a, sigA, b, sigB);
    }

    function test_TwoHeadsDisagreeingAtOneSeqNameTheTurnsMemberOnce() public {
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        vm.expectEmit(address(faults));
        emit FaultProof.FaultProven(alice, turn, a.position, vault.signerEpoch(), a.seq);
        assertEq(_prove(a, b), alice);
        assertTrue(faults.proven(turn, a.position));

        // The same pair, or another conflict of the same assignment, proves nothing more.
        (LogHeads.HeadV2 memory c, LogHeads.HeadV2 memory d) = _conflict();
        (c.seq, d.seq) = (a.seq + 7, a.seq + 7);
        _expectRevert(abi.encodeWithSelector(FaultProof.AlreadyProven.selector, turn, a.position), c, d);
        _expectRevert(abi.encodeWithSelector(FaultProof.AlreadyProven.selector, turn, a.position), a, b);
    }

    function test_AHeadTheLogHeadsContractAcceptsIsOneTheProofReads() public {
        // The two contracts build one digest: a posted head and its rolled-back twin prove the fault.
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        _post(a);
        assertEq(_prove(a, b), alice);
    }

    function test_HeadsThatAgreeOrNameDifferentSeqsProveNothing() public {
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        LogHeads.HeadV2 memory same = abi.decode(abi.encode(a), (LogHeads.HeadV2));
        same.signedAt = a.signedAt + 5;
        _expectRevert(abi.encodeWithSelector(FaultProof.NotConflicting.selector), a, same);
        b.seq = a.seq + 1;
        _expectRevert(abi.encodeWithSelector(FaultProof.NotConflicting.selector), a, b);
    }

    function test_HeadsNamingDifferentAssignmentsOrReleasesProveNothing() public {
        bytes memory different = abi.encodeWithSelector(FaultProof.DifferentAssignment.selector);
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        b.turn = a.turn + 1;
        _expectRevert(different, a, b);
        (a, b) = _conflict();
        b.position = a.position + 1;
        _expectRevert(different, a, b);
        (a, b) = _conflict();
        b.release = keccak256("another release");
        _expectRevert(different, a, b);
    }

    function test_AReleaseOrGenerationTheAssignmentNeverHadProvesNothing() public {
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        bytes32 other = keccak256("another release");
        (a.release, b.release) = (other, other);
        _expectRevert(abi.encodeWithSelector(FaultProof.ReleaseNotAssigned.selector, other, a.generation), a, b);
        (a, b) = _conflict();
        b.generation = a.generation + 1;
        _expectRevert(abi.encodeWithSelector(FaultProof.ReleaseNotAssigned.selector, b.release, b.generation), a, b);
    }

    function test_AnUnassignedPositionProvesNothing() public {
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        (a.position, b.position) = (9, 9);
        _expectRevert(abi.encodeWithSelector(FaultProof.NoAssignment.selector, turn, uint16(9)), a, b);
    }

    function test_AHeadSignedByAnotherKeyOrEpochProvesNothing() public {
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        uint64 epoch = vault.signerEpoch();
        bytes memory sigA = _sign(a, _signerKey());
        bytes memory forged = _sign(b, uint256(keccak256("not the enclave")));
        vm.expectRevert(FaultProof.InvalidSigner.selector);
        faults.prove(epoch, a, sigA, b, forged);
        // Both signed by the epoch's key, but claimed for an epoch the digest does not bind.
        bytes memory sigB = _sign(b, _signerKey());
        vm.expectRevert(FaultProof.InvalidSigner.selector);
        faults.prove(epoch + 1, a, sigA, b, sigB);
        vm.expectRevert(abi.encodeWithSelector(ECDSA.ECDSAInvalidSignatureLength.selector, uint256(3)));
        faults.prove(epoch, a, hex"010203", b, sigB);
    }

    function test_APairAcrossAPromotionsSignerChangeProvesNothing() public {
        // The outgoing epoch's head, then a promotion: the new epoch's key signs a different link at
        // the same seq for the promoted assignment, the history the promotion discarded.
        LogHeads.HeadV2 memory before = _beat();
        uint64 outgoing = vault.signerEpoch();
        uint256 outgoingKey = _signerKey();
        _lapse();
        (uint64 promotedTurn, uint16 position) = schedule.promote();
        assertEq(promotedTurn, turn);
        uint64 promotedEpoch = vault.signerEpoch();
        assertEq(promotedEpoch, outgoing + 1);
        LogHeads.HeadV2 memory after_ = abi.decode(abi.encode(before), (LogHeads.HeadV2));
        after_.link = keccak256("the promoted member's history");
        (after_.position, after_.generation) = (position, schedule.turnOf(turn).generation);
        after_.release = registry.candidateOf(schedule.turnOf(turn).candidate).release;

        // Neither epoch's key signed both: under either epoch one signature fails.
        bytes memory sigBefore = _signAt(before, outgoingKey, outgoing);
        bytes memory sigAfter = _signAt(after_, _signerKey(), promotedEpoch);
        vm.expectRevert(FaultProof.DifferentAssignment.selector);
        faults.prove(outgoing, before, sigBefore, after_, sigAfter);
        after_.position = before.position;
        after_.release = before.release;
        after_.generation = before.generation;
        sigAfter = _signAt(after_, _signerKey(), promotedEpoch);
        vm.expectRevert(FaultProof.InvalidSigner.selector);
        faults.prove(outgoing, before, sigBefore, after_, sigAfter);
        vm.expectRevert(FaultProof.InvalidSigner.selector);
        faults.prove(promotedEpoch, before, sigBefore, after_, sigAfter);
    }

    function test_TheOutgoingEpochsFaultStaysProvableAfterAPromotion() public {
        // A fault signed before the promotion is still the outgoing assignment's, under its own epoch.
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        uint64 outgoing = vault.signerEpoch();
        bytes memory sigA = _sign(a, _signerKey());
        bytes memory sigB = _sign(b, _signerKey());
        _post(a);
        _lapse();
        schedule.promote();
        assertEq(faults.prove(outgoing, a, sigA, b, sigB), alice);
    }

    function test_NoScheduleProvesNothing() public {
        // A vault with no registry has no assignments to name.
        Vault bare =
            new Vault(new MockUSDC(), vault.checkpointSigner(), governance, multisig, 1e24, 7 days, 3 days, 1 days);
        FaultProof unbound = new FaultProof(bare);
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        bytes memory sigA = _signAt(a, _signerKey(), 1, address(bare));
        bytes memory sigB = _signAt(b, _signerKey(), 1, address(bare));
        vm.expectRevert(FaultProof.NoSchedule.selector);
        unbound.prove(1, a, sigA, b, sigB);
    }

    function testFuzz_OnlyAConflictAtOneSeqOfOneAssignmentProves(
        uint64 seqShift,
        bytes32 linkB,
        uint64 turnShift,
        uint16 positionShift
    ) public {
        (LogHeads.HeadV2 memory a, LogHeads.HeadV2 memory b) = _conflict();
        b.seq = a.seq + (seqShift % 3);
        b.link = linkB;
        b.turn = a.turn + (turnShift % 2);
        b.position = a.position + (positionShift % 2);
        bool conflicting = b.seq == a.seq && b.link != a.link;
        bool sameAssignment = b.turn == a.turn && b.position == a.position;
        if (!conflicting) {
            _expectRevert(abi.encodeWithSelector(FaultProof.NotConflicting.selector), a, b);
        } else if (!sameAssignment) {
            _expectRevert(abi.encodeWithSelector(FaultProof.DifferentAssignment.selector), a, b);
        } else {
            assertEq(_prove(a, b), alice);
        }
    }

    /// @dev `head` signed by `key` under `epoch` of this test's vault.
    function _signAt(LogHeads.HeadV2 memory head, uint256 key, uint64 epoch) internal view returns (bytes memory) {
        return _signAt(head, key, epoch, address(vault));
    }

    /// @dev `head` signed by `key` under `epoch` of `target`, built by hand as the node builds it.
    function _signAt(LogHeads.HeadV2 memory head, uint256 key, uint64 epoch, address target)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = keccak256(
            bytes.concat(
                abi.encodePacked(
                    "engine.log-head.v2",
                    uint64(block.chainid),
                    target,
                    epoch,
                    head.seq,
                    head.link,
                    head.parentSeq,
                    head.parentLink,
                    head.turn
                ),
                abi.encodePacked(
                    head.position,
                    head.release,
                    head.generation,
                    head.stamp,
                    head.signedAt,
                    head.hold,
                    head.confirmer,
                    head.blockNumber,
                    head.blockHash
                )
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }
}
