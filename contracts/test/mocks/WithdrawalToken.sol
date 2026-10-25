// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ERC20} from "@openzeppelin-contracts/token/ERC20/ERC20.sol";

/// @notice Exercises outgoing sender fees, blocked transfers and callbacks against the Vault.
contract WithdrawalToken is ERC20 {
    bool private senderFee;
    bool private blocked;
    address private callback;
    bytes private callbackData;
    error TransferBlocked();

    constructor() ERC20("Withdrawal Test Token", "WTT") {}

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function configure(bool senderFee_, bool blocked_, address callback_, bytes calldata data) external {
        senderFee = senderFee_;
        blocked = blocked_;
        callback = callback_;
        callbackData = data;
    }

    function _update(address from, address to, uint256 amount) internal override {
        if (from != address(0) && to != address(0)) {
            if (blocked) revert TransferBlocked();
            if (senderFee) super._update(from, address(0), 1);
            if (callback != address(0)) {
                (bool ok, bytes memory reason) = callback.call(callbackData);
                if (!ok) {
                    assembly ("memory-safe") { revert(add(reason, 32), mload(reason)) }
                }
            }
        }
        super._update(from, to, amount);
    }
}
