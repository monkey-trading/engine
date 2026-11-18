//! A command's derived rows are indexed after its logged events, under the cursor indices past them,
//! and leave the command's identity in the history head as the logged entry alone gives it.

use std::sync::Arc;

use engine_core::financing::FinancingRow;

use super::*;
use crate::history::derived::Taken;

#[test]
fn derived_rows_follow_the_logged_events_and_leave_the_prefix_the_log_gives() {
    let rows: Vec<FinancingRow> =
        [A, B, A].iter().zip(1..).map(|(account, market)| financing(*account, market)).collect();
    let logged = command(1, 100, Some(vec![deposit(A, 1), Event::AdminApplied]));
    let (mut index, _) = fixture("derived-rows", 1_000);
    let taken = Taken { command: logged.clone(), derived: Some(Arc::new(rows.clone())) };
    index.ingest_taken(&[Taken { command: command(0, 50, None), derived: None }, taken]).expect("ingest");
    assert_eq!(index.head().command_prefix, prefix(&[command(0, 50, None), logged]), "the log's prefix");
    let page = index.page(PageQuery { kind: Some("financing".into()), ..query(A) }).expect("proved page");
    let cursors: Vec<String> = page.events.iter().map(|event| event.cursor.to_string()).collect();
    assert_eq!(cursors, ["1:4", "1:2"], "past the two logged events, newest first");
    assert_eq!(page.events[1].event, rows[0].event());
    let all = index.page(query(A)).expect("proved page");
    assert_eq!(all.events.len(), 3, "the deposit and both of A's rows");
    let page = index.page(PageQuery { kind: Some("financing".into()), ..query(B) }).expect("proved page");
    assert_eq!(page.events.iter().map(|event| event.cursor.to_string()).collect::<Vec<_>>(), ["1:3"]);
}

fn financing(account: AccountId, market: u32) -> FinancingRow {
    FinancingRow { account, market, amount: Fixed::from_int(i64::from(market)) }
}
