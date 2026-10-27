//! Canonical payer screening keeps one fixed command format across startup activation.

use super::*;
use crate::custody::stall::{PollError, Stall};
use crate::sequencer::Request;
use engine_core::deposits::DepositUpdate;
use tokio::sync::mpsc;

#[tokio::test]
async fn a_reorg_screens_the_canonical_payer_even_when_activation_races_submission() {
    for activate_during_submit in [true, false] {
        let dir = scratch("access-replacement-activation");
        let (config, clear, calls, source_task) = source(answer).await;
        clear.store(true, Ordering::SeqCst);
        let client = Arc::new(AccessClient::new(&config).unwrap());
        let node = Node::start_with_policy(&dir, Some(config.policy()));
        activate(&node, config.policy()).await;
        let trader = eip712::address(client_key().verifying_key());
        attest(&node, trader).await;
        let chains = chains(10);
        let original = funding(0, Beneficiary::Trader(trader), AccountId([0xaa; 20]));
        add(&chains, &original);
        let mut watch = screened_watcher(&chains, &dir, &client);
        assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
        assert_eq!(node.balance(trader).await, Some(Fixed::from_int(50)));
        clear.store(false, Ordering::SeqCst);
        let moved = VaultDeposit {
            block: 4,
            block_hash: B256::repeat_byte(0x84),
            payer: Some(AccountId([0xbb; 20])),
            ..original.clone()
        };
        for chain in &chains {
            chain.remove(&original);
            chain.add(moved.clone());
            chain
                .0
                .lock()
                .unwrap()
                .forks
                .extend((1..=10).map(|height| (height, B256::repeat_byte(0x80 + height as u8))));
        }
        let (proxy_tx, mut proxy_rx) = mpsc::channel::<Request>(64);
        let real_tx = node.tx.clone();
        let activated = Arc::new(AtomicBool::new(false));
        let switched = activated.clone();
        let proxy = tokio::spawn(async move {
            while let Some(request) = proxy_rx.recv().await {
                let replacement = match &request {
                    Request::Submit { cmd: Command::AccessCustodyReplacement { .. }, .. } => true,
                    Request::Submit { cmd: Command::CustodyDeposit(update), .. } => {
                        matches!(update.as_ref(), DepositUpdate::Replace { .. })
                    }
                    _ => false,
                };
                if activate_during_submit && replacement && !switched.swap(true, Ordering::SeqCst) {
                    let events = crate::custody::credit::submit(
                        &real_tx,
                        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 }),
                    )
                    .await
                    .unwrap();
                    assert_eq!(events, [Event::AdminApplied]);
                }
                real_tx.send(request).await.unwrap();
            }
        });
        let mut result = Ok(Polled::Waiting);
        for _ in 0..20 {
            result = watch.poll(&proxy_tx).await;
            if matches!(result, Ok(Polled::Read { caught_up: true }) | Err(_)) {
                break;
            }
        }
        if !activate_during_submit {
            assert!(matches!(result, Err(PollError::Stalled(Stall::Unsettled(_)))), "{result:?}");
            assert_eq!(
                node.balance(trader).await,
                Some(Fixed::from_int(50)),
                "preactivation refusal moves no principal"
            );
            assert!(held_reasons(&node).await.is_empty());
            assert_eq!(node.read(|engine| engine.protocol_version()).await, ProtocolVersion::V24);
            assert_eq!(
                node.submit(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V26 })).await,
                [Event::AdminApplied]
            );
            for _ in 0..20 {
                result = watch.poll(&proxy_tx).await;
                assert!(result.is_ok(), "{result:?}");
                if matches!(result, Ok(Polled::Read { caught_up: true })) {
                    break;
                }
            }
        } else {
            assert!(activated.load(Ordering::SeqCst));
        }
        assert!(matches!(result, Ok(Polled::Read { caught_up: true })), "{result:?}");
        assert_eq!(node.balance(trader).await, Some(Fixed::ZERO));
        assert_eq!(held_reasons(&node).await, [AccessReject::PayerSanctioned]);
        assert_eq!(calls.load(Ordering::SeqCst), if activate_during_submit { 2 } else { 3 });
        drop(proxy_tx);
        proxy.await.unwrap();
        node.stop().await;
        assert!(log_entries(&dir.join("log")).iter().any(|entry| matches!(entry, LogEntry::Command(command) if matches!(command.sequenced.cmd, Command::AccessCustodyReplacement { payer: Some(payer), .. } if payer == AccountId([0xbb; 20])))));
        source_task.abort();
        let _ = source_task.await;
    }
}
