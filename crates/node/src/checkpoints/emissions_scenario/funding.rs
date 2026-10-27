//! Canonical Vault funding and view calls used by the legacy emissions scenario.

use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositStatus, DepositUpdate};
use engine_core::event::Event;
use engine_core::types::AccountId;

use super::Scenario;
use crate::checkpoints::scenario::{send, transact};

sol! {
    event Deposited(uint256 indexed id, address indexed account, address indexed from, uint256 amount);
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function depositToHouse(uint256 amount) external returns (uint256);
    function depositFor(address account, uint256 amount) external returns (uint256);
}

impl Scenario {
    /// Deposits actual Vault USDC and records its exact canonical receipt and finalized frontier.
    pub(super) async fn fund(&self, account: Option<AccountId>, amount: Fixed) {
        assert_eq!(amount.raw() % 1_000, 0, "funding uses whole USDC base units");
        let units = U256::from(u128::try_from(amount.raw() / 1_000).unwrap());
        send(&self.provider, self.funder, self.usdc, mintCall { to: self.funder, amount: units }.abi_encode()).await;
        send(&self.provider, self.funder, self.usdc, approveCall { spender: self.vault, amount: units }.abi_encode())
            .await;
        let call = match account {
            Some(account) => depositForCall { account: Address::from(account.0), amount: units }.abi_encode(),
            None => depositToHouseCall { amount: units }.abi_encode(),
        };
        let mined =
            transact(&self.provider, TransactionRequest::default().from(self.funder).to(self.vault).input(call.into()))
                .await;
        assert!(mined.status(), "the Vault deposit succeeds");
        let logs: Vec<_> = mined.logs().iter().filter(|log| log.address() == self.vault).collect();
        assert_eq!(logs.len(), 1, "one Vault deposit event per funding transaction");
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
        assert_eq!((payer, received), (self.funder, units));
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
        let events =
            self.submit(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() }))).await;
        assert!(events.iter().any(|event| matches!(event, Event::CustodyDepositChanged { principal_delta, .. } if *principal_delta == amount)), "{events:?}");
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

    /// Calls a view function at `to`.
    pub(super) async fn view<C: SolCall>(&self, to: Address, call: C) -> C::Return {
        let request = TransactionRequest::default().to(to).input(call.abi_encode().into());
        let out: Bytes = self.provider.request("eth_call", (request, "latest")).await.unwrap();
        C::abi_decode_returns(&out).unwrap()
    }
}
