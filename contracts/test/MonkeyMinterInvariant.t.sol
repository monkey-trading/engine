// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {TimelockController} from "@openzeppelin-contracts/governance/TimelockController.sol";

import {MonkeyMinter} from "../src/MonkeyMinter.sol";
import {MonkeyToken} from "../src/MonkeyToken.sol";
import {Vault} from "../src/Vault.sol";
import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {TreeBuilder} from "./helpers/TreeBuilder.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// @dev Two vaults, each posting roots whose entitlements grow, signers rotated and retired, and the
/// minter moved between them whenever the timelock asks; a launch vault's growth starts from what
/// the other last proved, as a carried log's does. Every claim names a root of either vault at any of
/// its recorded seqs, so a proof the followed vault does not hold is tried too; its outcome is
/// predicted from the vault the minter follows, and a refused claim must mint nothing.
contract MonkeyMinterHandler is GovernanceTest {
    uint256 private constant FIRST_KEY = 0x1234;
    uint8 private constant ENTITLEMENT_LEAF = 8;

    MonkeyMinter private minter;
    MonkeyToken private monkey;
    Vault[2] private vaults;
    uint256[2] private signerKeys;
    address[3] private actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("carol")];

    /// @dev Each vault's newest entitlement for each actor, and every root it posted with its leaves.
    mapping(uint256 vaultIndex => uint256[3] amounts) private newest;
    mapping(uint256 vaultIndex => uint64[] seqs) private seqsOf;
    mapping(uint256 vaultIndex => mapping(uint64 seq => uint256[3] amounts)) private amountsAt;
    mapping(uint256 vaultIndex => mapping(uint64 seq => bytes32 root)) private rootAt;

    /// @notice The highest entitlement any root of either vault proved for each actor.
    uint256[3] public highestProven;
    uint256 public moves;
    uint256 public mints;
    /// @notice Mints against a root of the vault a move named.
    uint256 public mintsAfterMove;

    constructor(MonkeyMinter minter_, Vault[2] memory vaults_, TimelockController timelock) {
        minter = minter_;
        monkey = minter_.monkey();
        vaults = vaults_;
        signerKeys = [FIRST_KEY, FIRST_KEY + 10];
        governance = timelock;
    }

    function post(uint256 seed, bool onOther) external {
        uint256 index = _index(onOther);
        Vault vault = vaults[index];
        if (vault.checkpointSigner() == address(0)) return;
        uint256[3] memory amounts = newest[index];
        for (uint256 i; i < actors.length; ++i) {
            uint256 draw = uint256(keccak256(abi.encode(seed, i)));
            // An entitlement only grows, sometimes by nothing.
            amounts[i] += draw % 3 == 0 ? 0 : bound(draw >> 8, 1, 1e24);
            if (amounts[i] > highestProven[i]) highestProven[i] = amounts[i];
        }
        newest[index] = amounts;
        uint64 seq = vault.hasCheckpoint() ? vault.latestSeq() + 1 : uint64(bound(seed, 0, 1));
        amountsAt[index][seq] = amounts;
        seqsOf[index].push(seq);
        (bytes32[] memory keys, bytes32[] memory values) = _leaves(amounts);
        bytes32 root = TreeBuilder.root(keys, values);
        rootAt[index][seq] = root;
        _sign(vault, signerKeys[index], seq, root);
    }

    /// @dev Claims with a proof from either vault's root at one of its seqs, relayed by anyone.
    function claim(uint256 actorSeed, uint256 seqSeed, bool fromOther) external {
        uint256 index = _index(fromOther);
        uint64[] storage seqs = seqsOf[index];
        if (seqs.length == 0) return;
        _claim(index, actorSeed % actors.length, seqs[seqSeed % seqs.length]);
    }

    /// @dev Claims at the followed vault's newest root, the one most likely to mint.
    function claimNewest(uint256 actorSeed) external {
        uint256 index = _index(false);
        uint64[] storage seqs = seqsOf[index];
        if (seqs.length == 0) return;
        _claim(index, actorSeed % actors.length, seqs[seqs.length - 1]);
    }

    /// @dev Claims for `actor` with the proof from vault `index`'s root at `seq`, predicting the outcome
    /// from the vault the minter follows.
    function _claim(uint256 index, uint256 actor, uint64 seq) private {
        address account = actors[actor];
        uint256 entitled = amountsAt[index][seq][actor];
        (bytes32[] memory keys, bytes32[] memory values) = _leaves(amountsAt[index][seq]);
        bytes32[] memory siblings = TreeBuilder.proof(keys, values, actor);
        uint256 claimed = minter.emissionsClaimed(account);
        uint256 supplyBefore = monkey.totalSupply();

        bool refused = true;
        Vault followed = minter.vault();
        try followed.claimRoot(seq) returns (bytes32 root) {
            if (root != rootAt[index][seq]) {
                vm.expectRevert(MonkeyMinter.InvalidEntitlementProof.selector);
            } else if (entitled <= claimed) {
                vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.NothingToClaim.selector, entitled, claimed));
            } else {
                refused = false;
            }
        } catch (bytes memory reason) {
            vm.expectRevert(reason);
        }
        uint256 amount = minter.claimEmissions(account, entitled, seq, siblings);
        if (refused) {
            assertEq(monkey.totalSupply(), supplyBefore, "a refused claim mints nothing");
            assertEq(minter.emissionsClaimed(account), claimed, "a refused claim moves no record");
            return;
        }
        assertEq(amount, entitled - claimed, "a claim mints what its root adds to the record");
        assertEq(monkey.totalSupply(), supplyBefore + amount);
        mints++;
        if (moves != 0) mintsAfterMove++;
    }

    /// @dev Rotates one call in three, so roots live long enough to claim.
    function rotate(uint256 keySeed, bool onOther) external {
        if (keySeed % 3 != 0) return;
        uint256 index = _index(onOther);
        uint256 next = bound(keySeed, FIRST_KEY + 20 * index, FIRST_KEY + 20 * index + 4);
        _govern(address(vaults[index]), abi.encodeCall(Vault.setSigner, (vm.addr(next))));
        signerKeys[index] = next;
    }

    function retire(bool onOther) external {
        Vault vault = vaults[_index(onOther)];
        if (vault.checkpointSigner() == address(0)) return;
        vm.prank(multisig);
        vault.retireSigner();
    }

    /// @dev Asks the timelock to move the minter to the other vault: refused while the vault it follows
    /// has a signer. A move lifts the next vault's newest entitlements to the last ones proven, as its
    /// carried log's would be.
    function move() external {
        Vault current = minter.vault();
        uint256 from = address(current) == address(vaults[0]) ? 0 : 1;
        uint256 to = 1 - from;
        address signer = current.checkpointSigner();
        if (signer != address(0)) {
            vm.expectRevert(abi.encodeWithSelector(MonkeyMinter.SignerLive.selector, signer));
            vm.prank(address(governance));
            minter.setVault(vaults[to]);
            return;
        }
        _govern(address(minter), abi.encodeCall(MonkeyMinter.setVault, (vaults[to])));
        for (uint256 i; i < actors.length; ++i) {
            if (newest[from][i] > newest[to][i]) newest[to][i] = newest[from][i];
        }
        moves++;
    }

    function actorCount() external view returns (uint256) {
        return actors.length;
    }

    function actor(uint256 index) external view returns (address) {
        return actors[index];
    }

    /// @dev Posts `root` at `seq` on `vault`, extending its latest checkpoint, signed with `key`.
    function _sign(Vault vault, uint256 key, uint64 seq, bytes32 root) private {
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
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        vault.postCheckpoint(seq, root, hasParent, parentSeq, parentRoot, abi.encodePacked(r, s, v));
    }

    /// @dev The vault the minter follows for `other` false, the other one for true.
    function _index(bool other) private view returns (uint256 index) {
        index = address(minter.vault()) == address(vaults[0]) ? 0 : 1;
        if (other) index = 1 - index;
    }

    /// @dev Each actor's entitlement leaf, in actor order.
    function _leaves(uint256[3] memory amounts) private view returns (bytes32[] memory keys, bytes32[] memory values) {
        keys = new bytes32[](actors.length);
        values = new bytes32[](actors.length);
        for (uint256 i; i < actors.length; ++i) {
            keys[i] = keccak256(abi.encodePacked(ENTITLEMENT_LEAF, actors[i]));
            values[i] = keccak256(abi.encode(amounts[i]));
        }
    }
}

contract MonkeyMinterInvariantTest is GovernanceTest {
    MonkeyMinter private minter;
    MonkeyToken private monkey;
    MonkeyMinterHandler private handler;

    function setUp() public {
        vm.warp(1_800_000_000);
        governance = _deployGovernance();
        Vault alpha =
            new Vault(new MockUSDC(), vm.addr(0x1234), governance, multisig, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER);
        Vault launch = new Vault(
            new MockUSDC(), vm.addr(0x1234 + 10), governance, multisig, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER
        );
        minter = new MonkeyMinter(governance, 1e27);
        monkey = minter.monkey();
        _govern(address(minter), abi.encodeCall(MonkeyMinter.setVault, (alpha)));
        handler = new MonkeyMinterHandler(minter, [alpha, launch], governance);
        targetContract(address(handler));
        bytes4[] memory selectors = new bytes4[](6);
        selectors[0] = MonkeyMinterHandler.post.selector;
        selectors[1] = MonkeyMinterHandler.claim.selector;
        selectors[2] = MonkeyMinterHandler.claimNewest.selector;
        selectors[3] = MonkeyMinterHandler.rotate.selector;
        selectors[4] = MonkeyMinterHandler.retire.selector;
        selectors[5] = MonkeyMinterHandler.move.selector;
        targetSelector(FuzzSelector(address(handler), selectors));
    }

    /// @dev Across every move, nobody holds more than the highest entitlement any root proved for it,
    /// and the total minted is no more than the sum of those.
    function invariant_NothingMintsPastTheHighestProvenEntitlement() public view {
        uint256 minted;
        uint256 proven;
        for (uint256 i; i < handler.actorCount(); ++i) {
            address account = handler.actor(i);
            assertLe(minter.emissionsClaimed(account), handler.highestProven(i));
            minted += minter.emissionsClaimed(account);
            proven += handler.highestProven(i);
        }
        assertLe(monkey.totalSupply(), proven);
        assertEq(monkey.totalSupply(), minted);
    }

    /// @dev The one record is exactly what each account holds, whichever vault's root minted it.
    function invariant_TheRecordIsWhatEachAccountHolds() public view {
        for (uint256 i; i < handler.actorCount(); ++i) {
            address account = handler.actor(i);
            assertEq(monkey.balanceOf(account), minter.emissionsClaimed(account));
        }
    }
}
