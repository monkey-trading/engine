// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ERC20} from "@openzeppelin-contracts/token/ERC20/ERC20.sol";
import {ERC20Capped} from "@openzeppelin-contracts/token/ERC20/extensions/ERC20Capped.sol";

/// @title MonkeyToken
/// @notice MONKEY, the token traders earn from fixed ten-minute emission budgets shared by their
/// realised losses. The vault that deploys it is its only minter, and mints only what a posted engine
/// checkpoint entitles an account to. The cap is the emission schedule's nominal maximum supply.
/// @dev 18 decimals, the base unit the engine's entitlements count in. No owner, no other mint
/// route, and no burn: cancelled emission budgets are simply never minted.
contract MonkeyToken is ERC20Capped {
    /// @notice The only address that may mint: the vault that deployed this token.
    address public immutable minter;

    error NotMinter(address caller);

    /// @param cap_ The nominal maximum supply in base units.
    constructor(uint256 cap_) ERC20("MONKEY", "MONKEY") ERC20Capped(cap_) {
        minter = msg.sender;
    }

    /// @notice Mints `amount` base units to `to`. Only the minter may call it, and never past the cap.
    function mint(address to, uint256 amount) external {
        if (msg.sender != minter) revert NotMinter(msg.sender);
        _mint(to, amount);
    }
}
