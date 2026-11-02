// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";

import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Vault} from "../src/Vault.sol";
import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {TreeBuilder} from "./helpers/TreeBuilder.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";
import {MockWallet} from "./mocks/MockWallet.sol";

/// @notice A contract that relays a claim for someone else.
contract ClaimRelayer {
    function claimEmissions(Vault vault, address account, uint256 entitled, uint64 seq, bytes32[] calldata siblings)
        external
        returns (uint256)
    {
        return vault.claimEmissions(account, entitled, seq, siblings);
    }

    function claimDistribution(Vault vault, address account, uint256 entitled, uint64 seq, bytes32[] calldata siblings)
        external
        returns (uint256)
    {
        return vault.claimDistribution(account, entitled, seq, siblings);
    }
}

/// @notice A token that, on its way out of the vault, claims again against another root and records
/// what the vault's accounting already says.
contract ClaimReentrantToken is MockUSDC {
    Vault public vault;
    address public account;
    uint256 public entitled;
    uint64 public seq;
    bytes4 public refusal;
    uint256 public claimedDuringTransfer;
    uint256 public paidDuringTransfer;

    function arm(Vault vault_, address account_, uint256 entitled_, uint64 seq_) external {
        (vault, account, entitled, seq) = (vault_, account_, entitled_, seq_);
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (address(vault) == address(0) || from != address(vault)) return;
        Vault target = vault;
        vault = Vault(address(0));
        claimedDuringTransfer = target.distributionsClaimed(account);
        paidDuringTransfer = target.distributionsPaid();
        try target.claimDistribution(account, entitled, seq, new bytes32[](0)) {}
        catch (bytes memory reason) {
            refusal = bytes4(reason);
        }
    }
}

/// Claims against any recorded checkpoint of the current signer: an older root pays what was due at
/// it and no more, and a signer's change ends its roots.
contract ClaimRootsTest is GovernanceTest {
    uint256 private constant KEY = 0x1234;
    uint256 private constant NEXT_KEY = 0x5678;
    uint8 private constant ENTITLEMENT_LEAF = 8;
    uint8 private constant DISTRIBUTION_LEAF = 13;
    /// @dev More than any test here allocates.
    uint256 private constant FUNDED = 1e15;

    /// @dev The claim under test: both go through the same checks and accounting.
    enum Kind {
        Emissions,
        Distribution
    }

    MockUSDC private usdc;
    Vault private vault;
    MonkeyToken private monkey;
    address private alice = makeAddr("alice");
    uint256 private signerKey = KEY;
    bool private posted;
    uint64 private parentSeq;
    bytes32 private parentRoot;
    /// @dev Each checkpoint's leaves, so a claim at it can be proven.
    mapping(uint64 seq => bytes32[] keys) private leafKeys;
    mapping(uint64 seq => bytes32[] values) private leafValues;

    function setUp() public {
        vm.warp(1_800_000_000);
        governance = _deployGovernance();
        usdc = new MockUSDC();
        vault = new Vault(usdc, vm.addr(KEY), governance, multisig, 1e27, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER);
        monkey = vault.monkey();
        usdc.mint(address(vault), FUNDED);
    }

    function test_TheVaultStaysUnderTheContractSizeLimit() public view {
        assertLe(address(vault).code.length, 24_576, "EIP-170");
    }

    function test_AnOlderRootThenANewerThenTheOlderAgain() public {
        // Alice holds 100 at both checkpoints 10 and 11, and 250 at 12.
        _post(10, alice, 100);
        _post(11, alice, 100);
        _post(12, alice, 250);
        for (uint256 i; i < 2; ++i) {
            Kind kind = Kind(i);
            _expectClaimed(kind, 100, 100, 10);
            assertEq(_claim(kind, alice, 100, 10), 100, "the older root pays what was due at it");
            assertEq(_held(kind, alice), 100);
            assertEq(_recorded(kind, alice), 100);

            _expectClaimed(kind, 150, 250, 12);
            assertEq(_claim(kind, alice, 250, 12), 150, "the newer root pays the rest");
            assertEq(_held(kind, alice), 250);

            _expectNothing(100, 250);
            _claim(kind, alice, 100, 10);
            _expectNothing(100, 250);
            _claim(kind, alice, 100, 11);
            _expectNothing(250, 250);
            _claim(kind, alice, 250, 12);
            assertEq(_held(kind, alice), 250);
        }
        assertEq(vault.distributionsPaid(), 250);
        assertEq(usdc.balanceOf(address(vault)), FUNDED - 250);
        assertEq(monkey.totalSupply(), 250);
    }

    function test_AfterTheNewestRootAnOlderOnePaysNothing() public {
        _post(10, alice, 100);
        _post(12, alice, 250);
        for (uint256 i; i < 2; ++i) {
            Kind kind = Kind(i);
            assertEq(_claim(kind, alice, 250, 12), 250);
            _expectNothing(100, 250);
            _claim(kind, alice, 100, 10);
            assertEq(_recorded(kind, alice), 250, "the older root leaves the record alone");
        }
    }

    function test_AProofStandsOnlyAgainstTheCheckpointItNames() public {
        _post(10, alice, 100);
        _post(12, alice, 250);
        for (uint256 i; i < 2; ++i) {
            Kind kind = Kind(i);
            bytes32[] memory older = _siblings(kind, 10, alice);
            bytes32[] memory newer = _siblings(kind, 12, alice);
            // The newer amount at the older root, and the other way round.
            vm.expectRevert(Vault.InvalidEntitlementProof.selector);
            _claimWith(kind, alice, 250, 10, older);
            vm.expectRevert(Vault.InvalidEntitlementProof.selector);
            _claimWith(kind, alice, 100, 12, newer);
            vm.expectRevert(Vault.InvalidEntitlementProof.selector);
            _claimWith(kind, alice, 101, 10, older);
            assertEq(_recorded(kind, alice), 0);
        }
    }

    function test_RefusesASeqTheVaultHasNotRecorded() public {
        for (uint256 i; i < 2; ++i) {
            // Before any checkpoint, seq zero included, which reads as a default everywhere.
            _expectUnknown(Kind(i), 0);
            _expectUnknown(Kind(i), 1);
        }
        _post(10, alice, 100);
        _post(12, alice, 250);
        uint64[5] memory unrecorded = [0, 9, 11, 13, type(uint64).max];
        for (uint256 i; i < 2; ++i) {
            for (uint256 n; n < unrecorded.length; ++n) {
                _expectUnknown(Kind(i), unrecorded[n]);
            }
            assertEq(_recorded(Kind(i), alice), 0);
        }
    }

    function test_ASeqOfZeroPaysOnceRecorded() public {
        _post(0, alice, 100);
        assertTrue(vault.hasCheckpointAt(0));
        _post(1, alice, 250);
        for (uint256 i; i < 2; ++i) {
            assertEq(_claim(Kind(i), alice, 100, 0), 100);
            assertEq(_claim(Kind(i), alice, 250, 1), 150);
        }
    }

    function test_ARotationEndsEveryEarlierRootUntilTheNextSignerPosts() public {
        _post(10, alice, 100);
        _post(11, alice, 200);
        for (uint256 i; i < 2; ++i) {
            assertEq(_claim(Kind(i), alice, 100, 10), 100);
        }
        _install(NEXT_KEY);
        assertEq(vault.signerEpoch(), 2);
        for (uint256 i; i < 2; ++i) {
            // The vault's latest root is the old signer's too: nothing pays until the new one posts.
            _expectOldEpoch(Kind(i), 100, 10);
            _expectOldEpoch(Kind(i), 200, 11);
            assertEq(_recorded(Kind(i), alice), 100);
        }
        _post(12, alice, 200);
        assertEq(vault.checkpointEpochAt(12), 2);
        for (uint256 i; i < 2; ++i) {
            assertEq(_claim(Kind(i), alice, 200, 12), 100, "the new signer's root pays what the old ones held");
            _expectOldEpoch(Kind(i), 200, 11);
        }
        _post(13, alice, 300);
        for (uint256 i; i < 2; ++i) {
            _expectNothing(200, 200);
            _claim(Kind(i), alice, 200, 12);
            assertEq(_claim(Kind(i), alice, 300, 13), 100);
        }
    }

    function test_ReinstallingTheSameKeyStillEndsItsEarlierRoots() public {
        _post(10, alice, 100);
        _install(KEY);
        for (uint256 i; i < 2; ++i) {
            _expectOldEpoch(Kind(i), 100, 10);
        }
        _post(11, alice, 100);
        for (uint256 i; i < 2; ++i) {
            assertEq(_claim(Kind(i), alice, 100, 11), 100);
        }
    }

    function test_RetirementEndsTheEpochsRootsUntilANewSignerPosts() public {
        _post(10, alice, 100);
        _post(11, alice, 200);
        for (uint256 i; i < 2; ++i) {
            assertEq(_claim(Kind(i), alice, 100, 10), 100);
        }
        vm.prank(multisig);
        vault.retireSigner();
        assertEq(vault.signerEpoch(), 1, "retirement starts no epoch");
        assertEq(vault.checkpointEpochAt(11), 1);
        for (uint256 i; i < 2; ++i) {
            vm.expectRevert(Vault.NoSigner.selector);
            _claim(Kind(i), alice, 200, 11);
            assertEq(_recorded(Kind(i), alice), 100);
        }
        vm.warp(block.timestamp + 30 days);
        vm.expectRevert(Vault.NoSigner.selector);
        _claim(Kind.Emissions, alice, 200, 11);

        _install(NEXT_KEY);
        for (uint256 i; i < 2; ++i) {
            _expectOldEpoch(Kind(i), 200, 11);
        }
        _post(12, alice, 200);
        for (uint256 i; i < 2; ++i) {
            assertEq(_claim(Kind(i), alice, 200, 12), 100);
        }
    }

    function test_AContractMayRelayAndAContractMayBePaid() public {
        ClaimRelayer relayer = new ClaimRelayer();
        MockWallet wallet = new MockWallet(alice);
        _post(10, address(wallet), 100);
        _post(11, address(wallet), 250);
        assertEq(
            relayer.claimEmissions(vault, address(wallet), 100, 10, _siblings(Kind.Emissions, 10, address(wallet))), 100
        );
        assertEq(
            relayer.claimDistribution(
                vault, address(wallet), 100, 10, _siblings(Kind.Distribution, 10, address(wallet))
            ),
            100
        );
        assertEq(monkey.balanceOf(address(wallet)), 100);
        assertEq(usdc.balanceOf(address(wallet)), 100);
        assertEq(monkey.balanceOf(address(relayer)), 0, "the relayer receives nothing");
        assertEq(usdc.balanceOf(address(relayer)), 0, "the relayer receives nothing");
    }

    function test_ReenteringAClaimAtAnotherRootIsRefusedAfterTheAccountingMoved() public {
        ClaimReentrantToken token = new ClaimReentrantToken();
        Vault target =
            new Vault(token, vm.addr(KEY), governance, multisig, 1e27, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER);
        token.mint(address(target), 1_000);
        bytes32 older = _singleLeaf(alice, 100);
        bytes32 newer = _singleLeaf(alice, 250);
        _postTo(target, 10, older, false, 0, bytes32(0));
        _postTo(target, 11, newer, true, 10, older);
        token.arm(target, alice, 100, 10);

        assertEq(target.claimDistribution(alice, 250, 11, new bytes32[](0)), 250);
        assertEq(token.refusal(), ReentrancyGuardTransient.ReentrancyGuardReentrantCall.selector);
        assertEq(token.claimedDuringTransfer(), 250, "the record moves before the transfer");
        assertEq(token.paidDuringTransfer(), 250, "the total moves before the transfer");
        assertEq(target.distributionsPaid(), 250);
        assertEq(token.balanceOf(alice), 250);
    }

    /// @dev Claims at checkpoints in any order pay exactly what each adds to the highest yet claimed,
    /// whichever root it was, and the two records never leave the vault's own balances.
    function testFuzz_AnyOrderOfRootsPaysTheHighestClaimed(uint40[6] memory additions, uint8[12] memory order) public {
        uint256[6] memory entitled;
        uint256 total;
        for (uint256 i; i < entitled.length; ++i) {
            // An entitlement only grows, here sometimes by nothing.
            total += additions[i] % 3 == 0 ? 0 : additions[i];
            entitled[i] = total;
            _post(uint64(100 + i), alice, total);
        }
        uint256[2] memory highest;
        for (uint256 n; n < order.length; ++n) {
            uint256 at = order[n] % entitled.length;
            uint64 seq = uint64(100 + at);
            Kind kind = Kind(n % 2);
            uint256 before = _recorded(kind, alice);
            if (entitled[at] <= before) {
                _expectNothing(entitled[at], before);
                _claim(kind, alice, entitled[at], seq);
                continue;
            }
            assertEq(_claim(kind, alice, entitled[at], seq), entitled[at] - before);
            highest[n % 2] = entitled[at];
            assertEq(_recorded(kind, alice), highest[n % 2]);
            assertEq(_held(kind, alice), highest[n % 2]);
            assertLe(_recorded(kind, alice), total, "never past the newest root");
        }
        assertEq(vault.distributionsPaid(), highest[1]);
        assertEq(usdc.balanceOf(address(vault)) + vault.distributionsPaid(), FUNDED);
        // What an older root paid counts as held, so booking the funding leaves no surplus behind.
        vault.bookSurplus();
        assertEq(vault.totalDeposited(), FUNDED);
        vm.expectRevert(abi.encodeWithSelector(Vault.NoSurplus.selector, 0));
        vault.bookSurplus();
    }

    function _key(Kind kind, address account) private pure returns (bytes32) {
        return keccak256(abi.encodePacked(kind == Kind.Emissions ? ENTITLEMENT_LEAF : DISTRIBUTION_LEAF, account));
    }

    function _singleLeaf(address account, uint256 entitled) private pure returns (bytes32) {
        return TreeBuilder.leaf(_key(Kind.Distribution, account), keccak256(abi.encode(entitled)));
    }

    /// @dev Posts checkpoint `seq` under the signer in force, holding `entitled` for `account` of both
    /// kinds.
    function _post(uint64 seq, address account, uint256 entitled) private {
        bytes32[] storage keys = leafKeys[seq];
        bytes32[] storage values = leafValues[seq];
        for (uint256 i; i < 2; ++i) {
            keys.push(_key(Kind(i), account));
            values.push(keccak256(abi.encode(entitled)));
        }
        bytes32 root = TreeBuilder.root(keys, values);
        _sign(vault, signerKey, seq, root, posted, parentSeq, parentRoot);
        (posted, parentSeq, parentRoot) = (true, seq, root);
    }

    function _postTo(Vault target, uint64 seq, bytes32 root, bool hasParent, uint64 parent, bytes32 parentHash)
        private
    {
        _sign(target, KEY, seq, root, hasParent, parent, parentHash);
    }

    function _sign(
        Vault target,
        uint256 key,
        uint64 seq,
        bytes32 root,
        bool hasParent,
        uint64 parent,
        bytes32 parentHash
    ) private {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2", uint64(block.chainid), address(target), seq, root, hasParent, parent, parentHash
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        target.postCheckpoint(seq, root, hasParent, parent, parentHash, abi.encodePacked(r, s, v));
    }

    function _install(uint256 key) private {
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(key))));
        signerKey = key;
    }

    function _siblings(Kind kind, uint64 seq, address account) private view returns (bytes32[] memory) {
        bytes32[] memory keys = leafKeys[seq];
        bytes32 key = _key(kind, account);
        for (uint256 i; i < keys.length; ++i) {
            if (keys[i] == key) return TreeBuilder.proof(keys, leafValues[seq], i);
        }
        return new bytes32[](0);
    }

    function _claim(Kind kind, address account, uint256 entitled, uint64 seq) private returns (uint256) {
        return _claimWith(kind, account, entitled, seq, _siblings(kind, seq, account));
    }

    function _claimWith(Kind kind, address account, uint256 entitled, uint64 seq, bytes32[] memory siblings)
        private
        returns (uint256)
    {
        return kind == Kind.Emissions
            ? vault.claimEmissions(account, entitled, seq, siblings)
            : vault.claimDistribution(account, entitled, seq, siblings);
    }

    function _held(Kind kind, address account) private view returns (uint256) {
        return kind == Kind.Emissions ? monkey.balanceOf(account) : usdc.balanceOf(account);
    }

    function _recorded(Kind kind, address account) private view returns (uint256) {
        return kind == Kind.Emissions ? vault.emissionsClaimed(account) : vault.distributionsClaimed(account);
    }

    function _expectClaimed(Kind kind, uint256 amount, uint256 entitled, uint64 seq) private {
        vm.expectEmit(address(vault));
        if (kind == Kind.Emissions) emit Vault.EmissionsClaimed(alice, amount, entitled, seq);
        else emit Vault.DistributionClaimed(alice, amount, entitled, seq);
    }

    function _expectNothing(uint256 entitled, uint256 claimed) private {
        vm.expectRevert(abi.encodeWithSelector(Vault.NothingToClaim.selector, entitled, claimed));
    }

    function _expectUnknown(Kind kind, uint64 seq) private {
        vm.expectRevert(Vault.UnknownCheckpoint.selector);
        _claimWith(kind, alice, 100, seq, new bytes32[](1));
    }

    function _expectOldEpoch(Kind kind, uint256 entitled, uint64 seq) private {
        bytes32[] memory siblings = _siblings(kind, seq, alice);
        vm.expectRevert(Vault.InvalidSignerEpoch.selector);
        _claimWith(kind, alice, entitled, seq, siblings);
    }
}
