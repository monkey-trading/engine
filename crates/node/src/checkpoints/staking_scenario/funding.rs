//! Canonical Vault deposits, screened receipts, and finalized custody progress for the scenario.

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use engine_core::Fixed;
use engine_core::access::receipt_access_deposit;
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use engine_core::types::AccountId;

use super::{Scenario, approveCall, mintCall};
use crate::access::ScreeningRequest;

sol! {
    event Deposited(uint256 indexed id, address indexed account, address indexed from, uint256 amount);
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);
    function depositToHouse(uint256 amount) external returns (uint256);
    function depositFor(address account, uint256 amount) external returns (uint256);
}

impl Scenario {
    /// Funds the actual vault and sequences the exact mined event with signed screening evidence.
    pub(super) async fn fund(&mut self, account: Option<AccountId>, amount: Fixed) -> DepositReceipt {
        assert_eq!(amount.raw() % 1_000, 0, "funding uses whole USDC base units");
        let units = U256::from(u128::try_from(amount.raw() / 1_000).unwrap());
        self.send_tx(self.funder, self.usdc, mintCall { to: self.funder, amount: units }.abi_encode()).await;
        self.send_tx(self.funder, self.usdc, approveCall { spender: self.vault, amount: units }.abi_encode()).await;
        let call = match account {
            Some(account) => depositForCall { account: Address::from(account.0), amount: units }.abi_encode(),
            None => depositToHouseCall { amount: units }.abi_encode(),
        };
        let mined = self.transact(self.funder, self.vault, call).await;
        assert!(mined.status(), "the vault deposit succeeds");
        let logs: Vec<_> = mined.logs().iter().filter(|log| log.address() == self.vault).collect();
        assert_eq!(logs.len(), 1, "one vault deposit event per funding transaction");
        let log = logs[0];
        let (id, payer, beneficiary, received) = match account {
            Some(account) => {
                let event = Deposited::decode_log(&log.inner).unwrap().data;
                assert_eq!(event.account, Address::from(account.0));
                (event.id, event.from, DepositBeneficiary::Trader { account }, event.amount)
            }
            None => {
                let event = HouseDeposited::decode_log(&log.inner).unwrap().data;
                (event.id, event.from, DepositBeneficiary::House, event.amount)
            }
        };
        assert_eq!((payer, received), (self.funder, units), "the credited amount and payer come from the vault event");
        let receipt = DepositReceipt {
            block: mined.block_number.unwrap(),
            block_hash: mined.block_hash.unwrap(),
            tx_hash: mined.transaction_hash,
            log_index: log.log_index.unwrap(),
            transaction_deposit_index: 0,
            deposit_id: id.to(),
            beneficiary,
            amount,
        };
        let payer = Some(AccountId(payer.into()));
        let deposit = receipt_access_deposit(&receipt, payer);
        let evidence = self
            .access
            .client
            .screen(&ScreeningRequest { request_hash: deposit_screening_hash(&deposit), account, payer, peer: None })
            .await;
        let events = self.submit(Command::AccessCustodyDeposit { receipt: receipt.clone(), payer, evidence }).await;
        if account.is_some() {
            assert!(events.iter().any(|event| matches!(event, Event::DepositPending { .. })), "{events:?}");
        } else {
            assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == amount)), "{events:?}");
        }
        self.deposits += amount;
        receipt
    }

    /// Releases the receipt's principal and records the actual finalized block and deposit count.
    pub(super) async fn finalize_funding(&self, receipt: &DepositReceipt) {
        let reference = receipt.reference();
        let observed_at_seq =
            self.query(move |engine| engine.recorded_deposit(&reference).unwrap().updated_at_seq).await;
        let events = self
            .submit(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise {
                receipt: receipt.clone(),
                observed_at_seq,
            })))
            .await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::CustodyDepositChanged { status: DepositStatus::Finalised, .. })),
            "{events:?}"
        );
        let checkpoint = CustodyCheckpoint {
            block: receipt.block,
            block_hash: receipt.block_hash,
            deposit_count: receipt.deposit_id + 1,
        };
        let events = self.submit(Command::CustodyFinalised(checkpoint)).await;
        assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
        assert_eq!(self.query(|engine| engine.custody_checkpoint()).await, Some(checkpoint));
    }
}
