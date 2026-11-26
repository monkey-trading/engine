// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {FaultProof} from "../src/FaultProof.sol";
import {GovernanceRecord} from "../src/GovernanceRecord.sol";
import {OperatorSchedule} from "../src/OperatorSchedule.sol";
import {ParameterVote} from "../src/ParameterVote.sol";
import {Vault} from "../src/Vault.sol";
import {ScheduleTest} from "./helpers/ScheduleTest.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// The record a schedule creates of the vote and fault proof contracts its nodes mirror: created over the
/// schedule and its vault, filled once and only by the registry's timelock, and only with a vote contract
/// over the schedule and a fault proof contract over its vault.
contract GovernanceRecordTest is ScheduleTest {
    uint64 internal constant NORMAL = 10 days;
    uint64 internal constant FAST = 1 hours;

    address internal stranger = makeAddr("stranger");

    ParameterVote internal votes;
    FaultProof internal faults;
    GovernanceRecord internal record;

    function setUp() public {
        _deploySchedule();
        votes = new ParameterVote(schedule, NORMAL, FAST);
        faults = new FaultProof(vault);
        record = schedule.governanceRecord();
    }

    function test_TheScheduleCreatesItsRecordOverItsVaultWithNothingRecorded() public view {
        assertEq(address(record.schedule()), address(schedule));
        assertEq(address(record.registry()), address(registry));
        assertEq(address(record.vault()), address(logHeads.vault()));
        // The registry names the schedule only with that vault.
        assertEq(address(record.vault()), address(registry.vault()));
        assertEq(address(record.vote()), address(0));
        assertEq(address(record.faultProof()), address(0));
    }

    function test_TheTimelockRecordsTheContractsOnce() public {
        vm.expectEmit(address(record));
        emit GovernanceRecord.GovernanceContractsSet(address(votes), address(faults));
        vm.prank(address(governance));
        record.setGovernanceContracts(votes, faults);
        assertEq(address(record.vote()), address(votes));
        assertEq(address(record.faultProof()), address(faults));

        // Neither other contracts that pass the checks nor the same ones are recorded again.
        ParameterVote laterVote = new ParameterVote(schedule, NORMAL, FAST);
        FaultProof laterFaults = new FaultProof(vault);
        vm.expectRevert(GovernanceRecord.GovernanceContractsRecorded.selector);
        vm.prank(address(governance));
        record.setGovernanceContracts(laterVote, laterFaults);
        vm.expectRevert(GovernanceRecord.GovernanceContractsRecorded.selector);
        vm.prank(address(governance));
        record.setGovernanceContracts(votes, faults);
        assertEq(address(record.vote()), address(votes));
        assertEq(address(record.faultProof()), address(faults));
    }

    function test_OnlyTheTimelockRecords() public {
        address[4] memory callers = [stranger, multisig, address(registry), address(schedule)];
        for (uint256 i = 0; i < callers.length; ++i) {
            vm.expectRevert(GovernanceRecord.UnauthorizedGovernance.selector);
            vm.prank(callers[i]);
            record.setGovernanceContracts(votes, faults);
        }
        assertEq(address(record.vote()), address(0));
    }

    function test_AVoteContractOverAnotherScheduleIsRefused() public {
        // Another schedule over the same registry and vault, which the registry does not name.
        ParameterVote elsewhere = new ParameterVote(_newSchedule(), NORMAL, FAST);
        vm.expectRevert(GovernanceRecord.InvalidGovernanceContracts.selector);
        vm.prank(address(governance));
        record.setGovernanceContracts(elsewhere, faults);
        assertEq(address(record.vote()), address(0));
    }

    function test_AFaultProofContractOverAnotherVaultIsRefused() public {
        Vault other = new Vault(new MockUSDC(), vault.checkpointSigner(), governance, multisig, 7 days, 3 days, 1 days);
        FaultProof elsewhere = new FaultProof(other);
        vm.expectRevert(GovernanceRecord.InvalidGovernanceContracts.selector);
        vm.prank(address(governance));
        record.setGovernanceContracts(votes, elsewhere);
        assertEq(address(record.faultProof()), address(0));
    }

    function test_AnAddressWithNoCodeIsRefused() public {
        vm.expectRevert();
        vm.prank(address(governance));
        record.setGovernanceContracts(ParameterVote(stranger), faults);
        vm.expectRevert();
        vm.prank(address(governance));
        record.setGovernanceContracts(votes, FaultProof(stranger));
        assertEq(address(record.vote()), address(0));
    }

    /// A schedule built by a test fixture names a record of its own, not its predecessor's.
    function test_EachScheduleCreatesItsOwnRecord() public {
        OperatorSchedule another = _newSchedule();
        assertTrue(address(another.governanceRecord()) != address(record));
        assertEq(address(another.governanceRecord().schedule()), address(another));
    }
}
