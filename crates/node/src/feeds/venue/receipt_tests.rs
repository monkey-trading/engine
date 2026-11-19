//! A venue quote's receipt: the time its book was read, which its wait to its stamp is timed from,
//! carried to the sequencer with it; a repeat sent to refresh a held quote is timed from its send.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::clock::now_ms;
use crate::feeds::venue::quiet_tests::{SYMBOL, at, feed_with_rules};
use crate::feeds::venue::quotes::Pending;
use crate::sequencer::Request;

#[tokio::test]
async fn a_sequenced_quote_carries_its_books_receipt_and_a_refreshed_repeat_its_send() {
    let (mut feed, mut rx) = feed_with_rules(true);
    // A repeat refreshes 20 ms after the quote it repeats, so the test waits briefly.
    feed.max_age.insert(1, 40);
    let mut buffered = BTreeMap::from([(SYMBOL.to_owned(), Pending::default())]);
    // A new mid whose book was read 7 ms ago goes with that receipt.
    let read = std::time::Instant::now() - Duration::from_millis(7);
    pending(&mut buffered).add(at(100, now_ms() - 10), now_ms()).unwrap();
    pending(&mut buffered).note_received(read);
    assert_eq!(feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap(), None);
    assert_eq!(received(&mut rx), Some(read));
    // A repeat of that mid waits for its refresh, and goes timed from its send, not its read.
    pending(&mut buffered).add(at(100, now_ms()), now_ms()).unwrap();
    pending(&mut buffered).note_received(read);
    let wake = feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap().expect("a timer for the repeat");
    assert_eq!(received(&mut rx), None, "the repeat waits");
    tokio::time::sleep_until(wake).await;
    let released = std::time::Instant::now();
    assert!(feed.emit(&mut buffered, &mut BTreeMap::new()).await.unwrap().is_none());
    assert!(received(&mut rx).expect("the repeat goes") >= released, "timed from its send");
}

/// The one symbol's pending quote.
fn pending(buffered: &mut BTreeMap<String, Pending>) -> &mut Pending {
    buffered.get_mut(SYMBOL).expect("the symbol")
}

/// The receipt the next sequenced quote carries, if one went.
fn received(rx: &mut mpsc::Receiver<Request>) -> Option<std::time::Instant> {
    match rx.try_recv() {
        Ok(Request::Quote { received, .. }) => Some(received),
        Ok(_) => panic!("expected a price command"),
        Err(_) => None,
    }
}
