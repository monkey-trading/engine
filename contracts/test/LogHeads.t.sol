// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;
import {GovernanceTest} from "./helpers/GovernanceTest.sol";
import {Vault} from "../src/Vault.sol";
import {LogHeads} from "../src/LogHeads.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";

contract LogHeadsTest is GovernanceTest {
    Vault private vault;
    LogHeads private heads;
    uint256 private constant KEY = 0x1234;

    function setUp() public {
        governance = _deployGovernance();
        vault = new Vault(
            IERC20(address(1)), vm.addr(KEY), governance, multisig, 1e18, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER
        );
        heads = new LogHeads(vault);
    }

    /// The `engine.log-head.v1` digest the enclave signs, byte-for-byte as the node builds it.
    function sign(uint64 seq, bytes32 link, uint64 parentSeq, bytes32 parentLink, uint256 key)
        internal
        view
        returns (bytes memory)
    {
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.log-head.v1",
                uint64(block.chainid),
                address(vault),
                uint64(vault.signerEpoch()),
                seq,
                link,
                parentSeq,
                parentLink
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(key, digest);
        return abi.encodePacked(r, s, v);
    }

    /// The fixed known-answer vector the node's `tee` tests replay: key 0x1234, chain 31337, vault
    /// 0x1111…11, epoch 7, seq 42, link 0x…2a, parent (41, 0x…29). Both sides must produce this
    /// signature.
    function testLogHeadSignatureVector() public {
        vm.chainId(31337);
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.log-head.v1",
                uint64(31337),
                address(0x1111111111111111111111111111111111111111),
                uint64(7),
                uint64(42),
                bytes32(uint256(0x2a)),
                uint64(41),
                bytes32(uint256(0x29))
            )
        );
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(KEY, digest);
        assertEq(r, bytes32(0xe6fb2b7d4ce26a25aa0997e2e550785451f8aa52a66a0b3f5f81092acd9f1162));
        assertEq(s, bytes32(0x753bcfa3ee2e0626ab9261097d77d213d96d3498c5dc6d99ddfbd4d6d1865dfa));
        assertEq(v, 27);
        assertEq(vm.addr(KEY), 0xCf03Dd0a894Ef79CB5b601A43C4b25E3Ae4c67eD);
    }

    function testFirstPostOfTheEpochNeedsNoParent() public {
        // The empty epoch head accepts any parent fields on its first post.
        vm.warp(1_800_000_000);
        heads.post(10, bytes32(uint256(10)), 0, bytes32(0), sign(10, bytes32(uint256(10)), 0, bytes32(0), KEY));
        (uint64 seq, bytes32 link, uint64 signedAt, uint64 boundBlock, uint64 postedAt) =
            heads.heads(vault.signerEpoch());
        assertEq(seq, 10);
        assertEq(link, bytes32(uint256(10)));
        assertEq(signedAt, 0, "a v1 head carries no signing time");
        assertEq(boundBlock, 0);
        assertEq(postedAt, 1_800_000_000);
    }

    function testAVaultWithNoRegistryTakesNoV2Head() public {
        LogHeads.HeadV2 memory head;
        (head.seq, head.link, head.signedAt) = (10, bytes32(uint256(10)), uint64(block.timestamp * 1000));
        vm.expectRevert(LogHeads.UseV1.selector);
        heads.postV2(head, new bytes(65));
    }

    function testPostsExtendTheStoredHeadAsAParentChain() public {
        heads.post(10, bytes32(uint256(10)), 0, bytes32(0), sign(10, bytes32(uint256(10)), 0, bytes32(0), KEY));
        heads.post(
            12,
            bytes32(uint256(12)),
            10,
            bytes32(uint256(10)),
            sign(12, bytes32(uint256(12)), 10, bytes32(uint256(10)), KEY)
        );
        (uint64 seq, bytes32 link,,,) = heads.heads(vault.signerEpoch());
        assertEq(seq, 12);
        assertEq(link, bytes32(uint256(12)));
    }

    function testRefusesAPostThatDoesNotExtendTheStoredHead() public {
        heads.post(10, bytes32(uint256(10)), 0, bytes32(0), sign(10, bytes32(uint256(10)), 0, bytes32(0), KEY));
        // A longer divergent copy cannot overwrite the anchor: the parent must be the stored head.
        bytes memory wrongParentLink = sign(12, bytes32(uint256(12)), 10, bytes32(uint256(11)), KEY);
        vm.expectRevert(
            abi.encodeWithSelector(
                LogHeads.NotParent.selector, vault.signerEpoch(), 10, bytes32(uint256(11)), 10, bytes32(uint256(10))
            )
        );
        heads.post(12, bytes32(uint256(12)), 10, bytes32(uint256(11)), wrongParentLink);
        // Neither can it claim a nonexistent parent.
        bytes memory absentParent = sign(12, bytes32(uint256(12)), 11, bytes32(uint256(11)), KEY);
        vm.expectRevert(
            abi.encodeWithSelector(
                LogHeads.NotParent.selector, vault.signerEpoch(), 11, bytes32(uint256(11)), 10, bytes32(uint256(10))
            )
        );
        heads.post(12, bytes32(uint256(12)), 11, bytes32(uint256(11)), absentParent);
        // The extending post still lands.
        heads.post(
            12,
            bytes32(uint256(12)),
            10,
            bytes32(uint256(10)),
            sign(12, bytes32(uint256(12)), 10, bytes32(uint256(10)), KEY)
        );
    }

    function testFuzzRefusesStaleOrReplayedSeq(uint64 first, uint64 second, bytes32 link) public {
        vm.assume(first > 0); // seq 0 is the unattested empty log: nothing ever posts it
        heads.post(first, link, 0, bytes32(0), sign(first, link, 0, bytes32(0), KEY));
        bytes memory again = sign(second, link, first, link, KEY);
        if (second <= first) {
            vm.expectRevert(abi.encodeWithSelector(LogHeads.NotNewer.selector, vault.signerEpoch(), second, first));
            heads.post(second, link, first, link, again);
        } else {
            heads.post(second, link, first, link, again);
        }
        (uint64 seq,,,,) = heads.heads(vault.signerEpoch());
        assertEq(seq, second <= first ? first : second);
    }

    function testRefusesForgedSignerAndModifiedSeqOrLink() public {
        bytes memory signature = sign(2, bytes32(uint256(7)), 0, bytes32(0), KEY);
        // Signatures are computed before the expectRevert: the helper's signerEpoch read is itself
        // a call, and it must not be the one expectRevert waits on.
        bytes memory forged3 = sign(3, bytes32(uint256(7)), 0, bytes32(0), KEY + 1);
        bytes memory forged2 = sign(2, bytes32(uint256(7)), 0, bytes32(0), KEY + 1);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        heads.post(3, bytes32(uint256(7)), 0, bytes32(0), forged3);
        // The signature for link 7 presented against link 8, and the forged key.
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        heads.post(2, bytes32(uint256(8)), 0, bytes32(0), signature);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        heads.post(2, bytes32(uint256(7)), 0, bytes32(0), forged2);
        (uint64 seq,,,,) = heads.heads(vault.signerEpoch());
        assertEq(seq, 0);
        heads.post(2, bytes32(uint256(7)), 0, bytes32(0), signature);
    }

    function testRefusesAnotherVaultOrChain() public {
        bytes memory signature = sign(2, bytes32(uint256(7)), 0, bytes32(0), KEY);
        Vault other = new Vault(
            IERC20(address(1)), vm.addr(KEY), governance, multisig, 1e18, EXIT_PERIOD, CLAIM_WINDOW, STALE_AFTER
        );
        LogHeads otherHeads = new LogHeads(other);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        otherHeads.post(2, bytes32(uint256(7)), 0, bytes32(0), signature);
        vm.chainId(block.chainid + 1);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        heads.post(2, bytes32(uint256(7)), 0, bytes32(0), signature);
    }

    function testRotationMovesPostsToANewEpoch() public {
        heads.post(5, bytes32(uint256(5)), 0, bytes32(0), sign(5, bytes32(uint256(5)), 0, bytes32(0), KEY));
        uint64 firstEpoch = vault.signerEpoch();
        _govern(address(vault), abi.encodeCall(Vault.setSigner, (vm.addr(KEY + 7))));
        // A stale-seq post under the new signer lands in the new epoch: epochs isolate histories,
        // and the new epoch's first post needs no parent.
        bytes memory rotated = sign(3, bytes32(uint256(3)), 0, bytes32(0), KEY + 7);
        heads.post(3, bytes32(uint256(3)), 0, bytes32(0), rotated);
        (uint64 oldSeq,,,,) = heads.heads(firstEpoch);
        (uint64 newSeq,,,,) = heads.heads(vault.signerEpoch());
        assertEq(oldSeq, 5);
        assertEq(newSeq, 3);
        // The old key no longer posts at all.
        bytes memory stale = sign(9, bytes32(uint256(9)), 5, bytes32(uint256(5)), KEY);
        vm.expectRevert(LogHeads.InvalidSigner.selector);
        heads.post(9, bytes32(uint256(9)), 5, bytes32(uint256(5)), stale);
    }
}
