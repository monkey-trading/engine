//! An exit read whose account holds more exit items than a step proves: the worker lists them across
//! steps from the last one proved, and answers what the inline engine proves.

use super::*;

#[test]
fn an_exit_read_of_more_items_than_a_step_proves_lists_them_across_steps_as_the_inline_proof_does() {
    let (mut h, account) = (Harness::zero_house_operated(ProtocolVersion::V22), trader(1));
    // Room for the house to lose with no capital of its own, so a client's win leaves the vault holding
    // less than the client's balance, and requests past what it holds queue.
    let mut params = h.engine.params().expect("params").clone();
    params.house_total_cap_floor = f("1000000");
    h.operator_admin(AdminCommand::SetParams(params));
    let mut spec = h.engine.markets()[&BTC].spec.clone();
    spec.house_cap_floor = f("1000000");
    h.operator_admin(AdminCommand::UpsertMarket(spec));
    h.deposit(account, "100");
    h.price(BTC, "100");
    _ = h.market_order(account, BTC, Side::Buy, "1", "100");
    h.price(BTC, "200");
    _ = h.market_order(account, BTC, Side::Sell, "1", "200");
    let withdraw = |amount| Command::Withdraw { account, amount: f(amount), auth: None };
    _ = h.cmd(withdraw("100"));
    (0..2 * PROOF_STEP).for_each(|_| _ = h.cmd(withdraw("1")));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V38 });
    let root = B256::from(h.engine.state_root());
    let inline = h.engine.exit_proof(account).expect("an exit proof under V38");
    assert_eq!(inline.queued.len(), 2 * PROOF_STEP, "every request past the vault's cash queues");

    let tree = h.engine.detach_tree().expect("a root under V9 leaves a tree");
    let seq = tree.newest().expect("committed").seq();
    let (mut state, progress): (_, Progress) = (WorkerState::new(StateTree::default()), Progress::default());
    state.take(Message::Work(Work::Install(Box::new(tree))));
    state.take(Message::Work(Work::Release(seq, root)));
    let (reply, mut answer) = oneshot::channel();
    state.take(Message::Read(Read::Exit(account, reply)));
    let mut steps = 0;
    let exit = loop {
        if let Ok(exit) = answer.try_recv() {
            break exit;
        }
        state.step(&progress).expect("the read is answered");
        steps += 1;
    };
    assert!(steps > 3, "the queued requests take more than one step: {steps}");
    assert_eq!(exit, Some(inline), "the worker proves every item once, in the inline proof's order");
}
