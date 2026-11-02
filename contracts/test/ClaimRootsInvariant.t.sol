// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Vault} from "../src/Vault.sol";
import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {TreeBuilder} from "./helpers/TreeBuilder.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// @dev Posts checkpoints whose cumulative amounts only grow, claims at any recorded one, and changes
/// the signer. A claim the vault should refuse is made too, and must change nothing.
contract ClaimRootsHandler is GovernanceTest {
    uint256 private constant FIRST_KEY = 0x1234;
    uint8 private constant ENTITLEMENT_LEAF = 8;
    uint8 private constant DISTRIBUTION_LEAF = 13;

    MockUSDC private usdc;
    Vault private vault;
    MonkeyToken private monkey;
    address[3] private actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("carol")];
    /// @dev The newest cumulative amount of each actor, by claim kind.
    uint256[3] private emissions;
    uint256[3] private distributions;

    uint256 public signerKey = FIRST_KEY;
    bool public retired;
    uint64[] private seqs;
    mapping(uint64 seq => uint256[3] amounts) private emissionsAt;
    mapping(uint64 seq => uint256[3] amounts) private distributionsAt;

    uint256 public funded;
    uint256 public olderRootClaims;
    uint256 public refusedOtherEpoch;
    uint256 public refusedRetired;

    constructor(MockUSDC token, Vault target, TimelockController timelock) {
        usdc = token;
        vault = target;
        monkey = target.monkey();
        governance = timelock;
    }

    function post(uint256 seed) external {
        if (retired) return;
        for (uint256 i; i < actors.length; ++i) {
            uint256 draw = uint256(keccak256(abi.encode(seed, i)));
            // An entitlement only grows, sometimes by nothing; what a distribution adds is set aside.
            uint256 emission = draw % 3 == 0 ? 0 : bound(draw >> 8, 1, 1e24);
            uint256 distribution = draw % 5 == 0 ? 0 : bound(draw >> 128, 1, 1e12);
            emissions[i] += emission;
            distributions[i] += distribution;
            usdc.mint(address(vault), distribution);
            funded += distribution;
        }
        uint64 seq = vault.hasCheckpoint() ? vault.latestSeq() + 1 : uint64(bound(seed, 0, 1));
        emissionsAt[seq] = emissions;
        distributionsAt[seq] = distributions;
        seqs.push(seq);
        (bytes32[] memory keys, bytes32[] memory values) = _leaves(seq);
        bytes32 root = TreeBuilder.root(keys, values);
        bool hasParent = vault.hasCheckpoint();
        uint64 parentSeq = vault.latestSeq();
        bytes32 parentRoot = vault.latestRoot();
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(vault),
                seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(signerKey, digest);
        vault.postCheckpoint(seq, root, hasParent, parentSeq, parentRoot, abi.encodePacked(r, s, v));
    }

    /// @dev Claims at a recorded checkpoint, whichever signer's it is, relayed by anyone.
    function claim(uint256 actorSeed, uint256 seqSeed, bool usdcClaim) external {
        if (seqs.length == 0) return;
        uint256 index = actorSeed % actors.length;
        uint64 seq = seqs[seqSeed % seqs.length];
        address account = actors[index];
        uint256 entitled = usdcClaim ? distributionsAt[seq][index] : emissionsAt[seq][index];
        uint256 claimed = usdcClaim ? vault.distributionsClaimed(account) : vault.emissionsClaimed(account);
        if (entitled == 0) return;
        (bytes32[] memory keys, bytes32[] memory values) = _leaves(seq);
        bytes32[] memory siblings = TreeBuilder.proof(keys, values, 2 * index + (usdcClaim ? 1 : 0));
        uint256 paidBefore = vault.distributionsPaid();
        uint256 supplyBefore = monkey.totalSupply();

        bool refused = true;
        if (vault.checkpointEpochAt(seq) != vault.signerEpoch()) {
            vm.expectRevert(Vault.InvalidSignerEpoch.selector);
            refusedOtherEpoch++;
        } else if (retired) {
            vm.expectRevert(Vault.NoSigner.selector);
            refusedRetired++;
        } else if (entitled <= claimed) {
            vm.expectRevert(abi.encodeWithSelector(Vault.NothingToClaim.selector, entitled, claimed));
        } else {
            refused = false;
            if (seq != vault.latestSeq()) olderRootClaims++;
        }
        uint256 amount = usdcClaim
            ? vault.claimDistribution(account, entitled, seq, siblings)
            : vault.claimEmissions(account, entitled, seq, siblings);
        if (refused) {
            assertEq(vault.distributionsPaid(), paidBefore, "a refused claim pays nothing");
            assertEq(monkey.totalSupply(), supplyBefore, "a refused claim mints nothing");
            return;
        }
        assertEq(amount, entitled - claimed, "a claim pays what its root adds");
    }

    function rotate(uint256 keySeed) external {
        uint256 next = bound(keySeed, FIRST_KEY, FIRST_KEY + 4);
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(next))));
        signerKey = next;
        retired = false;
    }

    function retire() external {
        if (retired) return;
        vm.prank(multisig);
        vault.retireSigner();
        retired = true;
    }

    function actorCount() external view returns (uint256) {
        return actors.length;
    }

    function actor(uint256 index) external view returns (address) {
        return actors[index];
    }

    function newestEmission(uint256 index) external view returns (uint256) {
        return emissions[index];
    }

    function newestDistribution(uint256 index) external view returns (uint256) {
        return distributions[index];
    }

    /// @dev Every actor's leaf of both kinds at `seq`, emission then distribution.
    function _leaves(uint64 seq) private view returns (bytes32[] memory keys, bytes32[] memory values) {
        keys = new bytes32[](2 * actors.length);
        values = new bytes32[](2 * actors.length);
        for (uint256 i; i < actors.length; ++i) {
            keys[2 * i] = keccak256(abi.encodePacked(ENTITLEMENT_LEAF, actors[i]));
            values[2 * i] = keccak256(abi.encode(emissionsAt[seq][i]));
            keys[2 * i + 1] = keccak256(abi.encodePacked(DISTRIBUTION_LEAF, actors[i]));
            values[2 * i + 1] = keccak256(abi.encode(distributionsAt[seq][i]));
        }
    }
}

contract ClaimRootsInvariantTest is GovernanceTest {
    MockUSDC private usdc;
    Vault private vault;
    MonkeyToken private monkey;
    ClaimRootsHandler private handler;

    function setUp() public {
        vm.warp(1_800_000_000);
        governance = _deployGovernance();
        usdc = new MockUSDC();
        vault = new Vault(usdc, vm.addr(0x1234), governance, multisig, 1e27, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER);
        monkey = vault.monkey();
        handler = new ClaimRootsHandler(usdc, vault, governance);
        targetContract(address(handler));
        bytes4[] memory selectors = new bytes4[](4);
        selectors[0] = ClaimRootsHandler.post.selector;
        selectors[1] = ClaimRootsHandler.claim.selector;
        selectors[2] = ClaimRootsHandler.rotate.selector;
        selectors[3] = ClaimRootsHandler.retire.selector;
        targetSelector(FuzzSelector(address(handler), selectors));
    }

    /// @dev Whatever root was claimed, nobody holds more than the newest checkpoint proves for it.
    function invariant_NoClaimPassesTheNewestAmount() public view {
        for (uint256 i; i < handler.actorCount(); ++i) {
            address account = handler.actor(i);
            assertLe(vault.emissionsClaimed(account), handler.newestEmission(i));
            assertLe(vault.distributionsClaimed(account), handler.newestDistribution(i));
        }
    }

    /// @dev The records are exactly what left: tokens minted, and USDC paid against what the vault was
    /// funded with, which is what the node's reconciliation reads.
    function invariant_TheRecordsMatchWhatLeftTheVault() public view {
        uint256 paid;
        uint256 minted;
        for (uint256 i; i < handler.actorCount(); ++i) {
            address account = handler.actor(i);
            assertEq(usdc.balanceOf(account), vault.distributionsClaimed(account));
            assertEq(monkey.balanceOf(account), vault.emissionsClaimed(account));
            paid += vault.distributionsClaimed(account);
            minted += vault.emissionsClaimed(account);
        }
        assertEq(vault.distributionsPaid(), paid);
        assertEq(monkey.totalSupply(), minted);
        assertEq(usdc.balanceOf(address(vault)) + vault.distributionsPaid(), handler.funded());
    }
}
