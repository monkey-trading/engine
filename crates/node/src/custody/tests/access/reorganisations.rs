//! A reorganisation above the finalised frontier through the production watcher, the provider quorum
//! and the durable sequencer, from `V83`: every receipt canonical again moves in one
//! `CustodyReorganisation`, so a deposit included again one vault id later keeps its credit while
//! another deposit takes its old id, and the watcher sequences nothing of a reorganisation until the
//! eligible head pairs or disproves each orphan.

use std::path::PathBuf;

use super::*;
use engine_core::deposits::{DepositStatus, DepositUpdate};

/// Records the declaration of the account `key` signs for, with allowed, clear evidence.
async fn attest_key(node: &Node, key: &SigningKey) -> AccountId {
    let account = eip712::address(key.verifying_key());
    let operator = operator();
    let (nonce, expires_at) = (0, now_ms() + 60_000);
    let hash = eip712::eligibility_hash(
        &eip712::domain(operator.chain_id, operator.vault),
        account,
        EligibilityClass::Professional,
        nonce,
        expires_at,
    );
    let request = EligibilityRequest {
        account,
        class: EligibilityClass::Professional,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash) },
    };
    let query = serde_json::json!({ "request_hash": hash, "account": account, "payer": null });
    let screening = Screening { jurisdiction: Jurisdiction::Allowed, ..signed_answer(&query, SanctionsStatus::Clear) };
    assert_eq!(
        node.submit(Command::AttestEligibility { request, evidence: AccessEvidence::Screened(signed(screening)) })
            .await,
        [Event::EligibilityAttested { account, class: EligibilityClass::Professional }]
    );
    account
}

/// The receipt transitions the log in `dir` holds, in order: the kind, and each subject's vault id.
fn transitions(dir: &Path) -> Vec<(&'static str, Vec<u64>)> {
    log_entries(&dir.join("log"))
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => match command.sequenced.cmd {
                Command::AccessCustodyDeposit { receipt, .. } => Some(("credit", vec![receipt.deposit_id])),
                Command::CustodyReorganisation { moves } => {
                    Some(("reorganise", moves.iter().map(|move_| move_.receipt.deposit_id).collect()))
                }
                Command::AccessCustodyReplacement { update, .. } | Command::CustodyDeposit(update) => {
                    Some(match *update {
                        DepositUpdate::Replace { receipt, .. } => ("replace", vec![receipt.deposit_id]),
                        DepositUpdate::Reverse { receipt, .. } => ("reverse", vec![receipt.deposit_id]),
                        DepositUpdate::Credit { receipt } => ("credit", vec![receipt.deposit_id]),
                        DepositUpdate::Finalise { receipt, .. } => ("finalise", vec![receipt.deposit_id]),
                        DepositUpdate::Reinstate { receipt, .. } => ("reinstate", vec![receipt.deposit_id]),
                    })
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// A node with the access source answering clear, at the latest protocol, with a client declared for
/// each of `seeds`' keys.
async fn latest(
    name: &str,
    seeds: impl IntoIterator<Item = u8>,
) -> (PathBuf, Node, Arc<AccessClient>, JoinHandle<()>, Vec<AccountId>) {
    at_version(name, seeds, ProtocolVersion::LATEST).await
}

/// `latest` at `version`.
async fn at_version(
    name: &str,
    seeds: impl IntoIterator<Item = u8>,
    version: ProtocolVersion,
) -> (PathBuf, Node, Arc<AccessClient>, JoinHandle<()>, Vec<AccountId>) {
    let dir = scratch(name);
    let (config, clear, _calls, source_task) = source(answer).await;
    clear.store(true, Ordering::SeqCst);
    let client = Arc::new(AccessClient::new(&config).unwrap());
    let node = Node::start_with_policy(&dir, Some(config.policy()));
    activate(&node, config.policy()).await;
    for version in [ProtocolVersion::V26, version] {
        assert_eq!(
            node.submit(Command::Admin(AdminCommand::ActivateProtocol { version })).await,
            [Event::AdminApplied]
        );
    }
    let mut accounts = Vec::new();
    for seed in seeds {
        accounts.push(attest_key(&node, &SigningKey::from_slice(&[seed; 32]).unwrap()).await);
    }
    (dir, node, client, source_task, accounts)
}

/// Each account's balance, in order; `None` for an account the engine does not hold.
async fn balances_of(node: &Node, accounts: &[AccountId]) -> Vec<Option<Fixed>> {
    let accounts = accounts.to_vec();
    node.read(move |engine| accounts.iter().map(|account| engine.accounts().get(account).map(|a| a.balance)).collect())
        .await
}

/// The fake chain's hash for `height` on the reorganised chain.
fn fork(height: u64) -> B256 {
    B256::repeat_byte(0x80 + height as u8)
}

/// Polls until the watcher has caught up, failing on any stall, and returns every account's balances
/// as they stood after each poll.
async fn catch_up(
    watch: &mut DepositWatcher<Quorum<FakeChain>>,
    node: &Node,
    accounts: &[AccountId],
) -> Vec<Vec<Option<Fixed>>> {
    let mut seen = Vec::new();
    for _ in 0..60 {
        let polled = watch.poll(&node.tx).await;
        seen.push(balances_of(node, accounts).await);
        match polled {
            Ok(Polled::Read { caught_up: true }) => return seen,
            Ok(_) => {}
            Err(error) => panic!("the reorganisation stalled the watcher: {error:?}"),
        }
    }
    panic!("the watcher did not catch up");
}

#[tokio::test]
async fn a_reorganisation_that_shifts_deposit_ids_keeps_every_canonical_deposit_credited() {
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let (dir, node, client, source_task, accounts) = latest("reorg-id-shift", [1, 2]).await;
    let (first, second) = (accounts[0], accounts[1]);
    // Ids 0 and 1, in blocks 1 and 2, each funded by its own beneficiary.
    let chains = chains(10);
    let (first_deposit, second_deposit) =
        (funding(0, Beneficiary::Trader(first), first), funding(1, Beneficiary::Trader(second), second));
    add(&chains, &first_deposit);
    add(&chains, &second_deposit);
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    let fifty = Some(Fixed::from_int(50));
    assert_eq!(balances_of(&node, &accounts).await, [fifty, fifty]);
    // A third account's deposit takes id 0 in block 1, and the two credited deposits are included
    // again, unchanged, in blocks 2 and 3 at ids 1 and 2.
    let newcomer = AccountId([0xcc; 20]);
    let inserted = VaultDeposit {
        tx_hash: B256::repeat_byte(0x33),
        block_hash: fork(1),
        ..funding(0, Beneficiary::Trader(newcomer), newcomer)
    };
    let first_moved = VaultDeposit { id: 1, block: 2, block_hash: fork(2), ..first_deposit.clone() };
    let second_moved = VaultDeposit { id: 2, block: 3, block_hash: fork(3), ..second_deposit.clone() };
    for chain in &chains {
        chain.remove(&first_deposit);
        chain.remove(&second_deposit);
        for deposit in [&inserted, &first_moved, &second_moved] {
            chain.add(deposit.clone());
        }
        chain.0.lock().unwrap().forks.extend((1..=10).map(|height| (height, fork(height))));
    }
    let seen = catch_up(&mut watch, &node, &accounts).await;
    let reference = format!(
        "{:020}:{}:{}:{}",
        second_deposit.block, second_deposit.block_hash, second_deposit.tx_hash, second_deposit.log_index
    );
    let orphan = node.read(move |engine| engine.recorded_deposit(&reference).map(|d| d.status.clone())).await;
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
    assert!(seen.iter().all(|balances| balances[..] == [fifty, fifty]), "{seen:?}");
    assert_eq!(orphan, Some(DepositStatus::Reversed), "the old receipt left its id");
    assert_eq!(
        transitions(&dir),
        [("credit", vec![0]), ("credit", vec![1]), ("reorganise", vec![0, 1]), ("credit", vec![0])]
    );
    assert_eq!(logs.count("alert=\"custody_chain_unsettled\""), 0);
    assert_eq!(logs.count("alert=\"bad_debt\""), 0);
}

#[tokio::test]
async fn every_deposit_after_an_inserted_one_keeps_its_credit() {
    let (dir, node, client, source_task, accounts) = latest("reorg-id-shift-four", 1..=4).await;
    let chains = chains(10);
    let deposits: Vec<VaultDeposit> = accounts
        .iter()
        .enumerate()
        .map(|(id, account)| funding(id as u64, Beneficiary::Trader(*account), *account))
        .collect();
    for deposit in &deposits {
        add(&chains, deposit);
    }
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    let fifty = Some(Fixed::from_int(50));
    // Another deposit takes id 0; each of the four is included again one block and one id later.
    let newcomer = AccountId([0xcc; 20]);
    let inserted = VaultDeposit {
        tx_hash: B256::repeat_byte(0x33),
        block_hash: fork(1),
        ..funding(0, Beneficiary::Trader(newcomer), newcomer)
    };
    for chain in &chains {
        chain.add(inserted.clone());
        for deposit in &deposits {
            chain.remove(deposit);
            let block = deposit.block + 1;
            chain.add(VaultDeposit { id: deposit.id + 1, block, block_hash: fork(block), ..deposit.clone() });
        }
        chain.0.lock().unwrap().forks.extend((1..=10).map(|height| (height, fork(height))));
    }
    let seen = catch_up(&mut watch, &node, &accounts).await;
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
    assert!(seen.iter().all(|balances| balances[..] == [fifty; 4]), "{seen:?}");
    assert_eq!(transitions(&dir).iter().filter(|(kind, _)| *kind == "reorganise").count(), 1);
}

#[tokio::test]
async fn a_reorganisation_waits_for_the_head_and_reverses_only_the_deposit_the_chain_dropped() {
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let (dir, node, client, source_task, accounts) = latest("reorg-dropped", [1, 2]).await;
    let (first, second) = (accounts[0], accounts[1]);
    let chains = chains(10);
    let (first_deposit, second_deposit) =
        (funding(0, Beneficiary::Trader(first), first), funding(1, Beneficiary::Trader(second), second));
    add(&chains, &first_deposit);
    add(&chains, &second_deposit);
    // A watcher that reads two blocks a poll, so the reorganised scan reaches the head over several.
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 2)).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let mut watch = DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    watch.access = Some(client.clone());
    let fifty = Some(Fixed::from_int(50));
    catch_up(&mut watch, &node, &accounts).await;
    assert_eq!(balances_of(&node, &accounts).await, [fifty, fifty]);
    // The first deposit leaves the chain for good. A new one takes id 0 in block 2, in the scan's first
    // window, while the second deposit is included again at id 1 in block 9, past it: until the scan
    // reaches the head, the first's orphan might still return, and the new receipt would land on its id.
    let newcomer = AccountId([0xcc; 20]);
    let inserted = VaultDeposit {
        tx_hash: B256::repeat_byte(0x33),
        block: 2,
        block_hash: fork(2),
        ..funding(0, Beneficiary::Trader(newcomer), newcomer)
    };
    let second_moved = VaultDeposit { block: 9, block_hash: fork(9), ..second_deposit.clone() };
    for chain in &chains {
        chain.remove(&first_deposit);
        chain.remove(&second_deposit);
        chain.add(inserted.clone());
        chain.add(second_moved.clone());
        chain.0.lock().unwrap().forks.extend((1..=10).map(|height| (height, fork(height))));
    }
    let seen = catch_up(&mut watch, &node, &accounts).await;
    let newcomer_balance = node.balance(newcomer).await;
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
    // Nothing moved until the head: the first client kept its credit until the scan disproved it.
    assert!(seen.len() > 2, "the scan reached the head over several polls: {seen:?}");
    assert!(seen[..seen.len() - 1].iter().all(|balances| balances[..] == [fifty, fifty]), "{seen:?}");
    assert_eq!(seen.last().map(|balances| balances[..].to_vec()), Some(vec![Some(Fixed::ZERO), fifty]));
    assert_eq!(newcomer_balance, None, "the newcomer is not declared, so its deposit waits uncredited");
    let order: Vec<_> = transitions(&dir).into_iter().skip(2).collect();
    assert_eq!(order, [("reverse", vec![0]), ("reorganise", vec![1]), ("credit", vec![0])]);
    assert_eq!(logs.count("alert=\"custody_chain_inconsistent\""), 0);
}

#[tokio::test]
async fn an_activation_racing_a_reorganisation_holds_its_moves_for_the_next_poll() {
    // The poll reads an earlier version and moves receipts one at a time; `V83` activates as the first
    // move is submitted, refuses that move onto the id the second client's receipt holds, and the poll
    // waits as unsettled. The next poll moves both together. The refusal pages once, as the runbook says.
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let (dir, node, client, source_task, accounts) =
        at_version("reorg-activation-race", [1, 2], ProtocolVersion::V80).await;
    let chains = chains(10);
    let deposits: Vec<VaultDeposit> = accounts
        .iter()
        .enumerate()
        .map(|(id, account)| funding(id as u64, Beneficiary::Trader(*account), *account))
        .collect();
    for deposit in &deposits {
        add(&chains, deposit);
    }
    let mut watch = screened_watcher(&chains, &dir, &client);
    assert!(matches!(watch.poll(&node.tx).await, Ok(Polled::Read { caught_up: true })));
    let newcomer = AccountId([0xcc; 20]);
    let inserted = VaultDeposit {
        tx_hash: B256::repeat_byte(0x33),
        block_hash: fork(1),
        ..funding(0, Beneficiary::Trader(newcomer), newcomer)
    };
    for chain in &chains {
        chain.add(inserted.clone());
        for deposit in &deposits {
            chain.remove(deposit);
            let block = deposit.block + 1;
            chain.add(VaultDeposit { id: deposit.id + 1, block, block_hash: fork(block), ..deposit.clone() });
        }
        chain.0.lock().unwrap().forks.extend((1..=10).map(|height| (height, fork(height))));
    }
    let (proxy_tx, mut proxy_rx) = tokio::sync::mpsc::channel::<crate::sequencer::Request>(64);
    let real_tx = node.tx.clone();
    let proxy = tokio::spawn(async move {
        let mut activated = false;
        while let Some(request) = proxy_rx.recv().await {
            let moves = matches!(
                &request,
                crate::sequencer::Request::Submit { cmd: Command::AccessCustodyReplacement { .. }, .. }
            );
            if moves && !activated {
                activated = true;
                let activation = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
                let events = crate::custody::credit::submit(&real_tx, activation).await.unwrap();
                assert_eq!(events, [Event::AdminApplied]);
            }
            real_tx.send(request).await.unwrap();
        }
    });
    let mut raced = watch.poll(&proxy_tx).await;
    for _ in 0..20 {
        if !matches!(raced, Ok(Polled::Read { caught_up: false })) {
            break;
        }
        raced = watch.poll(&proxy_tx).await;
    }
    assert!(
        matches!(raced, Err(crate::custody::stall::PollError::Stalled(crate::custody::stall::Stall::Unsettled(_)))),
        "{raced:?}"
    );
    let fifty = Some(Fixed::from_int(50));
    assert_eq!(balances_of(&node, &accounts).await, [fifty, fifty]);
    drop(proxy_tx);
    proxy.await.unwrap();
    let seen = catch_up(&mut watch, &node, &accounts).await;
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
    assert!(seen.iter().all(|balances| balances[..] == [fifty, fifty]), "{seen:?}");
    // The refused move is logged, with its refusal, and changed nothing.
    let kinds: Vec<_> = transitions(&dir).into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(kinds, ["credit", "credit", "replace", "reorganise", "credit"]);
    assert_eq!(logs.count("alert=\"custody_chain_inconsistent\""), 0);
    assert_eq!(logs.count("alert=\"custody_deposit_rejected\""), 1);
}

#[tokio::test]
async fn an_activation_racing_a_fresh_credit_onto_a_waiting_orphans_id_holds_it_for_the_next_poll() {
    // The poll reads an earlier version, skips the dropped deposit's orphan while the scan has not
    // reached the head, and credits a new deposit at that orphan's id; `V83` activates as the credit is
    // submitted and refuses it, and the poll waits as unsettled rather than paging inconsistent data. The
    // refusal pages once, as the runbook says.
    let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
    let (dir, node, client, source_task, accounts) =
        at_version("reorg-activation-race-credit", [1, 2], ProtocolVersion::V80).await;
    let chains = chains(10);
    let (first_deposit, second_deposit) = (
        funding(0, Beneficiary::Trader(accounts[0]), accounts[0]),
        funding(1, Beneficiary::Trader(accounts[1]), accounts[1]),
    );
    add(&chains, &first_deposit);
    add(&chains, &second_deposit);
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 2)).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    let mut watch = DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    watch.access = Some(client.clone());
    catch_up(&mut watch, &node, &accounts).await;
    // The first deposit leaves the chain and a third account's deposit takes id 0 in block 2; the second
    // deposit is included again at id 1 in block 9, past the scan's first windows.
    let newcomer = accounts[1];
    let inserted = VaultDeposit {
        tx_hash: B256::repeat_byte(0x33),
        block: 2,
        block_hash: fork(2),
        ..funding(0, Beneficiary::Trader(newcomer), newcomer)
    };
    let second_moved = VaultDeposit { block: 9, block_hash: fork(9), ..second_deposit.clone() };
    for chain in &chains {
        chain.remove(&first_deposit);
        chain.remove(&second_deposit);
        chain.add(inserted.clone());
        chain.add(second_moved.clone());
        chain.0.lock().unwrap().forks.extend((1..=10).map(|height| (height, fork(height))));
    }
    let (proxy_tx, mut proxy_rx) = tokio::sync::mpsc::channel::<crate::sequencer::Request>(64);
    let real_tx = node.tx.clone();
    let proxy = tokio::spawn(async move {
        let mut activated = false;
        while let Some(request) = proxy_rx.recv().await {
            let credit =
                matches!(&request, crate::sequencer::Request::Submit { cmd: Command::AccessCustodyDeposit { .. }, .. });
            if credit && !activated {
                activated = true;
                let activation = Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
                let events = crate::custody::credit::submit(&real_tx, activation).await.unwrap();
                assert_eq!(events, [Event::AdminApplied]);
            }
            real_tx.send(request).await.unwrap();
        }
    });
    let mut raced = watch.poll(&proxy_tx).await;
    for _ in 0..20 {
        if !matches!(raced, Ok(Polled::Read { caught_up: false })) {
            break;
        }
        raced = watch.poll(&proxy_tx).await;
    }
    let unsettled =
        matches!(raced, Err(crate::custody::stall::PollError::Stalled(crate::custody::stall::Stall::Unsettled(_))));
    assert!(unsettled, "{raced:?}");
    drop(proxy_tx);
    proxy.await.unwrap();
    let seen = catch_up(&mut watch, &node, &accounts).await;
    node.stop().await;
    source_task.abort();
    let _ = source_task.await;
    assert_eq!(
        seen.last().map(|balances| balances[..].to_vec()),
        Some(vec![Some(Fixed::ZERO), Some(Fixed::from_int(100))])
    );
    assert_eq!(logs.count("alert=\"custody_chain_inconsistent\""), 0);
    assert_eq!(logs.count("alert=\"custody_deposit_rejected\""), 1);
}
