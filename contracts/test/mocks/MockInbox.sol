// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IInbox} from "../../src/SeedRelay.sol";

/// @notice A stand-in for Arbitrum's delayed inbox: it checks a retryable ticket's payment as the
/// Inbox does and records the ticket, which a test or the dev scenario's bridge then delivers from
/// the sender's aliased address.
contract MockInbox is IInbox {
    struct Ticket {
        address sender;
        address to;
        uint256 value;
        uint256 maxSubmissionCost;
        address excessFeeRefundAddress;
        address callValueRefundAddress;
        uint256 gasLimit;
        uint256 maxFeePerGas;
        bytes data;
    }

    Ticket[] internal tickets;

    event TicketCreated(uint256 indexed ticket, address indexed sender, address indexed to, bytes data);

    error InsufficientValue(uint256 expected, uint256 actual);
    error InsufficientSubmissionCost(uint256 expected, uint256 actual);

    function createRetryableTicket(
        address to,
        uint256 l2CallValue,
        uint256 maxSubmissionCost,
        address excessFeeRefundAddress,
        address callValueRefundAddress,
        uint256 gasLimit,
        uint256 maxFeePerGas,
        bytes calldata data
    ) external payable returns (uint256 id) {
        uint256 expected = maxSubmissionCost + l2CallValue + gasLimit * maxFeePerGas;
        if (msg.value < expected) revert InsufficientValue(expected, msg.value);
        uint256 fee = calculateRetryableSubmissionFee(data.length, 0);
        if (maxSubmissionCost < fee) revert InsufficientSubmissionCost(fee, maxSubmissionCost);
        id = tickets.length;
        tickets.push(
            Ticket(
                msg.sender,
                to,
                msg.value,
                maxSubmissionCost,
                excessFeeRefundAddress,
                callValueRefundAddress,
                gasLimit,
                maxFeePerGas,
                data
            )
        );
        emit TicketCreated(id, msg.sender, to, data);
    }

    /// @notice The Inbox's submission fee: `(1400 + 6 * dataLength)` times the base fee, the block's
    /// when `baseFee` is zero.
    function calculateRetryableSubmissionFee(uint256 dataLength, uint256 baseFee) public view returns (uint256) {
        return (1400 + 6 * dataLength) * (baseFee == 0 ? block.basefee : baseFee);
    }

    function ticketCount() external view returns (uint256) {
        return tickets.length;
    }

    function ticket(uint256 index) external view returns (Ticket memory) {
        return tickets[index];
    }
}
