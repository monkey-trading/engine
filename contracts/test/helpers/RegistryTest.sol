// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {GovernanceTest} from "./GovernanceTest.sol";

import {MonkeyToken} from "../../src/MonkeyToken.sol";
import {OperatorRegistry} from "../../src/OperatorRegistry.sol";
import {Staking} from "../../src/Staking.sol";
import {Vault} from "../../src/Vault.sol";
import {MockUSDC} from "../mocks/MockUSDC.sol";

/// @notice A vault, its MONKEY, and an operator registry with the staking contract it deploys,
/// under test parameters that are not proposed production values (#360 decides those).
abstract contract RegistryTest is GovernanceTest {
    uint256 internal constant COOLDOWN = 3 days;
    uint256 internal constant DISTRIBUTION_EPOCH = 30 minutes;
    /// @dev Test values only.
    uint64 internal constant EPOCH = 1 days;
    uint64 internal constant MIN_TERM = 30 days;
    uint64 internal constant FINALITY_MARGIN = 1 hours;
    uint16 internal constant MARGIN_BPS = 1_000;
    uint16 internal constant ELIGIBILITY_BPS = 500;
    uint16 internal constant QUORUM_BPS = 2_000;
    uint16 internal constant MAX_CANDIDATES = 10;
    uint256 internal constant MIN_BOND = 1_000e18;

    bytes32 internal constant CONFIG = keccak256("candidate config");

    Vault internal vault;
    MonkeyToken internal monkey;
    OperatorRegistry internal registry;
    Staking internal staking;
    address internal deployerKey = makeAddr("deployer admin key");

    function _deployRegistry() internal {
        governance = _deployGovernance();
        vault = new Vault(new MockUSDC(), makeAddr("enclave"), governance, multisig, 1e24, 7 days, 3 days, 1 days);
        monkey = vault.monkey();
        registry = new OperatorRegistry(
            monkey,
            COOLDOWN,
            DISTRIBUTION_EPOCH,
            governance,
            OperatorRegistry.InitialOperator(deployerKey, _digest(0), CONFIG),
            _parameters(),
            _minimums(),
            _maximums()
        );
        staking = registry.staking();
    }

    function _parameters() internal pure returns (OperatorRegistry.Parameters memory) {
        return OperatorRegistry.Parameters(
            EPOCH, MIN_TERM, FINALITY_MARGIN, MARGIN_BPS, ELIGIBILITY_BPS, QUORUM_BPS, MAX_CANDIDATES, MIN_BOND
        );
    }

    function _minimums() internal pure returns (OperatorRegistry.Parameters memory) {
        return OperatorRegistry.Parameters(1 hours, 0, 0, 0, 0, 0, 1, 0);
    }

    function _maximums() internal pure returns (OperatorRegistry.Parameters memory) {
        return OperatorRegistry.Parameters(30 days, 365 days, 7 days, 10_000, 10_000, 10_000, 100, 1e27);
    }

    /// @dev A 48-byte launch measurement distinct per `seed`.
    function _digest(uint256 seed) internal pure returns (bytes memory) {
        return abi.encodePacked(keccak256(abi.encode(seed)), bytes16(keccak256(abi.encode(seed, 1))));
    }

    function _release(uint256 seed) internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(_digest(seed), CONFIG));
    }

    /// @dev Mints through the vault in place of a proven claim; the cap is far above test amounts.
    function _stake(address account, uint256 amount) internal {
        vm.prank(address(vault));
        monkey.mint(account, amount);
        vm.startPrank(account);
        monkey.approve(address(staking), amount);
        staking.stake(amount);
        vm.stopPrank();
    }

    function _delegate(address account, address candidate) internal {
        vm.prank(account);
        staking.delegate(candidate);
    }

    function _registration(address adminKey, uint256 bond, uint256 seed)
        internal
        pure
        returns (OperatorRegistry.Registration memory)
    {
        return OperatorRegistry.Registration(
            adminKey, bond, _digest(seed), CONFIG, "api.candidate.example", "handover.candidate.example:9443"
        );
    }

    /// @dev The admin key's private key for `candidate`: deterministic, so the derived address and
    /// its signatures hold run to run.
    function _adminKey(address candidate) internal pure returns (uint256) {
        return uint256(keccak256(abi.encode("admin key", candidate)));
    }

    /// @dev The admin key's EIP-712 authorization of `candidate`'s registration, as `register` checks,
    /// signed over the hand-built digest so computing it consumes no prank.
    function _authorization(address candidate) internal view returns (bytes memory) {
        (uint8 v, bytes32 r, bytes32 s) =
            vm.sign(_adminKey(candidate), _registrationDigest(registry, candidate, _key(candidate)));
        return abi.encodePacked(r, s, v);
    }

    /// @dev The EIP-712 digest a registry's admin key signs, recomputed from the type and domain.
    function _registrationDigest(OperatorRegistry target, address candidate, address adminKey)
        internal
        view
        returns (bytes32)
    {
        bytes32 domain = keccak256(
            abi.encode(
                keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"),
                keccak256("EngineOperatorRegistry"),
                keccak256("1"),
                block.chainid,
                address(target)
            )
        );
        bytes32 structHash = keccak256(
            abi.encode(keccak256("RegistrationAuthorization(address candidate,address adminKey)"), candidate, adminKey)
        );
        return keccak256(abi.encodePacked(hex"1901", domain, structHash));
    }

    /// @dev Stakes `stake` for `candidate`, registers it with a minimum bond and release `seed`, and
    /// has the timelock approve that release. The authorization is read before the prank: the view
    /// call would otherwise consume it.
    function _candidate(address candidate, uint256 stake, uint256 seed) internal {
        _stake(candidate, stake);
        bytes memory authorization = _authorization(candidate);
        vm.prank(candidate);
        registry.register(_registration(_key(candidate), MIN_BOND, seed), authorization);
        _govern(
            address(registry),
            abi.encodeCall(OperatorRegistry.setReleaseApproval, (candidate, _digest(seed), CONFIG, true))
        );
    }

    function _key(address candidate) internal view returns (address) {
        return vm.addr(_adminKey(candidate));
    }

    function _setRotation(bool enabled) internal {
        _govern(address(registry), abi.encodeCall(OperatorRegistry.setRotation, (enabled)));
    }

    /// @dev Warps to the end of the open epoch unless it has already ended, and closes it.
    function _closeNext() internal returns (uint64) {
        (uint64 epochLength,,,,,,,) = registry.parameters();
        uint256 endsAt = uint256(registry.openEpochStart()) + epochLength;
        if (block.timestamp < endsAt) vm.warp(endsAt);
        return registry.closeEpoch();
    }

    function _inEffect() internal view returns (address) {
        return registry.operatorInEffect().candidate;
    }
}
