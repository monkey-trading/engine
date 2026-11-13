//! Reading one venue message into the books a connection holds for sequencing.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use tokio::time::timeout;

use crate::clock::now_ms;
use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::transport::Link;
use crate::feeds::venue::{Frame, Pending, Refusal, VenueFeed, to_quote};

impl VenueFeed {
    /// Reads one text message into `buffered`, refusing what does not parse or check, and sends
    /// any reply the venue's protocol asks for.
    /// Error cases:
    /// - the venue asked for a reconnect, or the adapter's state no longer matches the venue's,
    ///   including a refused book from an adapter that keeps state
    /// - a reply cannot be sent
    pub(super) async fn receive(
        &mut self,
        adapter: &mut Adapter,
        link: &mut Link,
        text: &str,
        buffered: &mut BTreeMap<String, Pending>,
    ) -> anyhow::Result<()> {
        let now = now_ms();
        let books = match adapter.read(text, now) {
            Ok(Frame::Books(books)) => books,
            Ok(Frame::Control | Frame::Subscribed) => return Ok(()),
            Ok(Frame::Reply(messages)) => {
                return timeout(self.timing.timeout, link.send_all(messages)).await.context("reply timed out")?;
            }
            Ok(Frame::Reconnect) => bail!("the venue asked for a reconnect"),
            Err(Refusal::Message(reason)) => {
                self.refuse(&reason, text);
                return Ok(());
            }
            Err(Refusal::Connection(reason)) => {
                self.refuse(&reason, text);
                bail!("resyncing: {reason}");
            }
        };
        // The adapter has kept what the message changes; only the quotes wait for the stream.
        if !self.stream_current() {
            books.iter().for_each(|book| self.standing.forget(&book.symbol));
            return Ok(());
        }
        for book in books {
            match to_quote(self.conn.venue, &self.instruments, book, now) {
                Ok((symbol, quote)) => match self.take_book(&symbol, quote, now, buffered) {
                    Ok(ts) => {
                        self.delay.note_read(&symbol, ts, self.timing.max_delay);
                    }
                    Err(lead_ms) => self.clock_ahead(self.instruments[&symbol].market, lead_ms),
                },
                Err(reason) if adapter.keeps_state() => {
                    self.refuse(&reason, text);
                    bail!("resyncing: {reason}");
                }
                Err(reason) => self.refuse(&reason, text),
            }
        }
        Ok(())
    }
}
