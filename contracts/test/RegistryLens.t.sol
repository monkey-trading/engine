// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {OperatorRegistry} from "../src/OperatorRegistry.sol";
import {RegistryLens} from "../src/RegistryLens.sol";
import {RegistryTest} from "./helpers/RegistryTest.sol";

/// The lens reads what the registry and the staking contract say, page by page, at 100 candidates.
contract RegistryLensTest is RegistryTest {
    uint256 internal constant LIMIT = 100;

    RegistryLens internal lens;
    address internal alice = makeAddr("alice");
    address internal bob = makeAddr("bob");
    address internal voter = makeAddr("voter");

    function setUp() public {
        _deployRegistry();
        lens = new RegistryLens(registry);
    }

    function test_TheLensNamesTheRegistryAndItsStaking() public view {
        assertEq(address(lens.registry()), address(registry));
        assertEq(address(lens.staking()), address(staking));
    }

    function test_AFreshRegistryReadsAsDeployed() public view {
        RegistryLens.Overview memory overview = lens.overview();
        assertFalse(overview.rotationEnabled);
        assertEq(abi.encode(overview.parameters), abi.encode(_parameters()));
        assertEq(overview.openEpoch, registry.openEpoch());
        assertEq(overview.openEpochStart, registry.openEpochStart());
        assertEq(overview.totalStaked, 0);
        assertEq(overview.candidateCount, 0);
        assertEq(overview.inEffect.candidate, deployerKey);
        assertEq(overview.inEffect.release, _release(0));
        assertEq(abi.encode(overview.named), abi.encode(overview.inEffect));
        assertEq(overview.pending.effectiveAt, 0);
        assertFalse(overview.pendingPromotion);
        assertEq(lens.candidates(0, LIMIT).length, 0);
    }

    function test_TheOverviewFollowsTheSwitchTheParametersAndTheStake() public {
        _stake(alice, 7_000e18);
        _stake(bob, 3_000e18);
        _setRotation(true);
        OperatorRegistry.Parameters memory changed = _parameters();
        changed.maxCandidates = 25;
        changed.minSelfBond = 2_000e18;
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (changed)));
        RegistryLens.Overview memory overview = lens.overview();
        assertTrue(overview.rotationEnabled);
        assertEq(abi.encode(overview.parameters), abi.encode(changed));
        assertEq(overview.totalStaked, 10_000e18);
    }

    function test_AHundredCandidatesReadPageByPageAsTheRegistryHasThem() public {
        _lift(LIMIT);
        address[] memory candidates = new address[](LIMIT);
        for (uint256 i = 0; i < LIMIT; ++i) {
            candidates[i] = makeAddr(string(abi.encode(i)));
            _candidate(candidates[i], MIN_BOND + i * 1e18, i + 1);
            // Every second candidate holds votes: its own stake and that of a delegator.
            if (i % 2 == 0) _delegate(candidates[i], candidates[i]);
        }
        // One large delegator makes the first candidate eligible; the others hold too small a share.
        _stake(voter, 200_000e18);
        _delegate(voter, candidates[0]);
        assertEq(registry.candidateCount(), LIMIT);
        assertEq(lens.overview().candidateCount, LIMIT);
        assertEq(lens.overview().totalStaked, staking.totalStaked());

        // One call reads the whole list inside a public endpoint's usual eth_call gas cap.
        uint256 gasBefore = gasleft();
        lens.candidates(0, LIMIT);
        emit log_named_uint("gas of one call for 100 candidates", gasBefore - gasleft());
        assertLt(gasBefore - gasleft(), 30_000_000);

        uint256 read = 0;
        for (uint256 start = 0; start < LIMIT; start += 30) {
            RegistryLens.Row[] memory page = lens.candidates(start, 30);
            assertEq(page.length, start + 30 > LIMIT ? LIMIT - start : 30);
            for (uint256 i = 0; i < page.length; ++i) {
                _assertRow(page[i], registry.candidateList(start + i));
                ++read;
            }
        }
        assertEq(read, LIMIT);
        uint256 withVotes = 0;
        uint256 eligible = 0;
        RegistryLens.Row[] memory all = lens.candidates(0, LIMIT);
        for (uint256 i = 0; i < all.length; ++i) {
            if (all[i].votes != 0) ++withVotes;
            if (all[i].eligible) ++eligible;
        }
        assertEq(withVotes, LIMIT / 2);
        assertEq(eligible, 1, "only the candidate the large delegator backs reaches the eligibility share");
        assertTrue(all[0].eligible);
    }

    function test_APageStopsAtTheEndOfTheListAndAnEmptyOneReadsEmpty() public {
        _candidate(alice, MIN_BOND, 1);
        _candidate(bob, MIN_BOND, 2);
        assertEq(lens.candidates(1, 10).length, 1);
        assertEq(lens.candidates(1, 10)[0].candidate, bob);
        assertEq(lens.candidates(2, 10).length, 0);
        assertEq(lens.candidates(50, 10).length, 0);
        assertEq(lens.candidates(0, 0).length, 0);
        assertEq(lens.candidates(0, type(uint256).max).length, 2, "an unbounded limit is the list");
    }

    function test_OnlyApprovedCandidatesAreListed() public {
        _candidate(alice, MIN_BOND, 1);
        // Bob registers but the timelock never approves its release.
        _stake(bob, MIN_BOND);
        vm.prank(bob);
        registry.register(_registration(_key(bob), MIN_BOND, 2), _authorization(bob));
        RegistryLens.Row[] memory rows = lens.candidates(0, LIMIT);
        assertEq(rows.length, 1);
        assertEq(rows[0].candidate, alice);
        assertEq(lens.overview().candidateCount, 1);
        // Withdrawing approval delists alice while it stays registered.
        _govern(
            address(registry), abi.encodeCall(OperatorRegistry.setReleaseApproval, (alice, _digest(1), CONFIG, false))
        );
        assertEq(lens.candidates(0, LIMIT).length, 0);
    }

    function test_ARowCarriesTheRegistrationTheVotesAndEligibility() public {
        _candidate(alice, 10_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _stake(voter, 5_000e18);
        _delegate(voter, alice);
        RegistryLens.Row[] memory rows = lens.candidates(0, LIMIT);
        assertEq(rows.length, 2);
        assertEq(rows[0].candidate, alice);
        assertEq(rows[0].adminKey, _key(alice));
        assertEq(rows[0].enclave, registry.candidateOf(alice).enclave);
        assertEq(rows[0].bond, MIN_BOND);
        assertEq(rows[0].release, _release(1));
        assertEq(rows[0].registration, 0);
        assertEq(rows[0].apiHostname, "api.candidate.example");
        assertEq(rows[0].handoverEndpoint, "handover.candidate.example:9443");
        assertEq(rows[0].votes, 15_000e18);
        assertTrue(rows[0].eligible);
        assertEq(rows[1].candidate, bob);
        assertEq(rows[1].registration, 1);
        assertEq(rows[1].votes, 0);
        assertFalse(rows[1].eligible, "no votes is below the eligibility share");
        assertEq(uint8(rows[1].role), uint8(RegistryLens.Role.Candidate));
    }

    function test_AnElectionMakesItsWinnerElectedThenTheOperator() public {
        _candidate(alice, 10_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        vm.warp(block.timestamp + MIN_TERM);
        registry.closeEpoch();
        OperatorRegistry.Election memory pending = registry.pendingElection();
        assertEq(pending.candidate, bob);

        RegistryLens.Overview memory overview = lens.overview();
        assertEq(overview.pending.candidate, bob);
        assertEq(overview.pending.effectiveAt, pending.effectiveAt);
        assertFalse(overview.pendingPromotion);
        assertEq(overview.inEffect.candidate, deployerKey);
        RegistryLens.Row[] memory rows = lens.candidates(0, LIMIT);
        assertEq(uint8(rows[0].role), uint8(RegistryLens.Role.Candidate));
        assertEq(uint8(rows[1].role), uint8(RegistryLens.Role.Elected));

        vm.warp(pending.effectiveAt);
        overview = lens.overview();
        assertEq(overview.pending.effectiveAt, 0);
        assertEq(overview.inEffect.candidate, bob);
        assertEq(overview.named.candidate, bob);
        rows = lens.candidates(0, LIMIT);
        assertEq(uint8(rows[0].role), uint8(RegistryLens.Role.Candidate));
        assertEq(uint8(rows[1].role), uint8(RegistryLens.Role.Operator));
    }

    function test_APromotionNamesItsStandbyAtOnceAndFlagsThePromotion() public {
        _candidate(alice, 10_000e18, 1);
        _candidate(bob, 20_000e18, 2);
        _delegate(alice, alice);
        _delegate(bob, bob);
        _setRotation(true);
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setVault, (address(vault))));
        vm.warp(block.timestamp + LIVENESS + 1);
        registry.promote();
        RegistryLens.Overview memory overview = lens.overview();
        assertTrue(overview.pendingPromotion);
        assertEq(overview.named.candidate, bob, "the vault names the promoted standby at once");
        assertEq(overview.inEffect.candidate, deployerKey);
        assertEq(uint8(lens.candidates(0, LIMIT)[1].role), uint8(RegistryLens.Role.Elected));
    }

    function test_ADeregisteredCandidateLeavesTheListWithoutGapsOrDuplicates() public {
        address[] memory candidates = new address[](5);
        for (uint256 i = 0; i < candidates.length; ++i) {
            candidates[i] = makeAddr(string(abi.encode(i)));
            _candidate(candidates[i], MIN_BOND, i + 1);
        }
        vm.prank(candidates[1]);
        registry.deregister();
        RegistryLens.Row[] memory rows = lens.candidates(0, LIMIT);
        assertEq(rows.length, 4);
        for (uint256 i = 0; i < rows.length; ++i) {
            _assertRow(rows[i], registry.candidateList(i));
            assertTrue(rows[i].candidate != candidates[1]);
            for (uint256 j = 0; j < i; ++j) {
                assertTrue(rows[i].candidate != rows[j].candidate);
            }
        }
    }

    /// @dev The row for `candidate` equals what the registry and the staking contract answer one by one.
    function _assertRow(RegistryLens.Row memory row, address candidate) internal view {
        OperatorRegistry.Candidate memory registered = registry.candidateOf(candidate);
        assertEq(row.candidate, candidate);
        assertEq(row.adminKey, registered.adminKey);
        assertEq(row.enclave, registered.enclave);
        assertEq(row.bond, registered.bond);
        assertEq(row.release, registered.release);
        assertEq(row.registration, registered.registration);
        assertEq(row.apiHostname, registered.apiHostname);
        assertEq(row.handoverEndpoint, registered.handoverEndpoint);
        assertEq(row.votes, staking.votes(candidate));
        assertEq(row.eligible, registry.isEligible(candidate));
        assertEq(uint8(row.role), uint8(RegistryLens.Role.Candidate));
    }

    /// @dev Raises the candidate cap to `cap`, as the timelock may within the deployment bounds.
    function _lift(uint256 cap) internal {
        OperatorRegistry.Parameters memory lifted = _parameters();
        lifted.maxCandidates = uint16(cap);
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setParameters, (lifted)));
    }
}
