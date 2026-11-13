//! A connection replaced on a fresh bearer token without a gap in its quotes. Once the next token is
//! in hand (see `auth`), the replacement opens on it while the current connection streams on: it is
//! pinged, and sends its opening messages and any reply the venue's protocol asks for. Once the
//! venue has accepted its subscription or sent a book, it is pinged again, so its pings bound what it
//! sends however long it took to open (see `delay`). Once that ping is answered it is ready: the
//! first book it then sends, bounded by that ping, is read once the feed switches, and if none comes
//! within a ping interval, as through a closure, it is ready without one. A book read, or a ping
//! interval ending, while that ping is older than `timing.max_delay` finds the connection behind:
//! it is pinged again, and readies on that ping's answer instead. A message the connection
//! would refuse and carry on past is passed over, and refused as the feed would once it switches. The
//! feed then switches to it and drops the old one, keeping the bounds noted for the old one's quotes
//! not yet sequenced. A replacement that fails, or is not ready within `timing.timeout` and before
//! its own token expires, is dropped and the current connection streams on; the feed tries again on a new token after a backoff, until
//! the current connection's token expires.

use std::future::Future;
use std::mem;
use std::pin::Pin;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use tokio::time::{Instant, timeout_at};
use tracing::info;

use crate::clock::now_ms;
use crate::feeds::venue::adapter::Adapter;
use crate::feeds::venue::auth::Token;
use crate::feeds::venue::delay::DelayProbe;
use crate::feeds::venue::standing::redates;
use crate::feeds::venue::transport::{self, Incoming, Link};
use crate::feeds::venue::{Frame, Refusal, VenueFeed};

/// The connection being opened to replace the current one.
pub(super) type Replacing = Pin<Box<dyn Future<Output = anyhow::Result<Replacement>> + Send>>;

/// A connection ready to take over: its link, its adapter, its pings, the messages it passed over
/// with why, and the message carrying the first book it sent once ready, if one came.
pub(super) struct Replacement {
    link: Link,
    adapter: Adapter,
    delay: DelayProbe,
    refused: Vec<(String, String)>,
    books: Vec<String>,
}

impl VenueFeed {
    /// Opens the connection that replaces the current one on `token`, its handshake carrying it (see
    /// the module doc).
    pub(super) fn replace(&self, token: &Token) -> Replacing {
        let (conn, tls, bearer, expires_at) =
            (self.conn.clone(), self.tls.clone(), token.bearer().clone(), token.expires_at());
        let (wait, max_delay, ping_interval) = (self.timing.timeout, self.timing.max_delay, self.timing.ping_interval);
        let symbols: Vec<String> = self.instruments.keys().cloned().collect();
        // Whether a frame read now is within the delay bound, as `VenueFeed::stream_current` judges.
        let current = move |delay: &DelayProbe| delay.behind(Instant::now()).is_some_and(|behind| behind <= max_delay);
        Box::pin(async move {
            let opened = async {
                let mut link = transport::connect(&conn, Some(bearer.expose()), tls).await.context("connect")?;
                let mut delay = DelayProbe::default();
                link.ping(delay.sent(Instant::now(), now_ms())).await.context("ping")?;
                let symbols: Vec<&str> = symbols.iter().map(String::as_str).collect();
                let mut adapter = Adapter::new(&conn, None, &symbols);
                link.send_all(adapter.opening(&symbols)).await.context("subscribe")?;
                let (mut subscribed, mut ready_from, mut book_until) = (false, None, None);
                let (mut refused, mut books) = (Vec::new(), Vec::new());
                while books.is_empty() {
                    if subscribed && ready_from.is_none() {
                        let now = Instant::now();
                        link.ping(delay.sent(now, now_ms())).await.context("ping")?;
                        ready_from = Some(now);
                    }
                    // Read after the ready ping's answer, a frame was sent after that ping.
                    let bounded = ready_from.is_some_and(|from| delay.newest_answered().is_some_and(|at| at >= from));
                    let next = link.stream.next();
                    let message = if bounded {
                        let until = *book_until.get_or_insert_with(|| Instant::now() + ping_interval);
                        match timeout_at(until, next).await {
                            Ok(message) => message,
                            Err(_) if current(&delay) => break,
                            Err(_) => {
                                (ready_from, book_until) = (None, None);
                                continue;
                            }
                        }
                    } else {
                        next.await
                    };
                    match message.context("connection closed")?.context("read")? {
                        Incoming::Pong(payload) => {
                            delay.answered(&payload);
                        }
                        Incoming::Text(text) => match adapter.read(&text, now_ms()) {
                            Ok(Frame::Reply(messages)) => link.send_all(messages).await.context("reply")?,
                            Ok(Frame::Control) => {}
                            Err(Refusal::Message(reason)) => refused.push((reason, text)),
                            Ok(Frame::Subscribed) => subscribed = true,
                            Ok(Frame::Books(read)) => {
                                subscribed = true;
                                if bounded && current(&delay) {
                                    read.iter().for_each(|book| delay.note_read(&book.symbol, book.ts, max_delay));
                                    books.push(text);
                                } else if bounded {
                                    (ready_from, book_until) = (None, None);
                                }
                            }
                            Ok(Frame::Reconnect) => bail!("the venue asked for a reconnect"),
                            Err(Refusal::Connection(reason)) => bail!(reason),
                        },
                        Incoming::Binary => refused.push(("binary message".to_owned(), String::new())),
                        Incoming::Closed(reason) => bail!("closed by the venue: {reason}"),
                        Incoming::Other => {}
                    }
                }
                Ok(Replacement { link, adapter, delay, refused, books })
            };
            // A replacement its token outlives by nothing would end as soon as the feed switched.
            timeout_at((Instant::now() + wait).min(expires_at), opened).await.context("not ready in time")?
        })
    }

    /// Switches the feed to `replaced` in place of `link` and `adapter`, on the token fetched for it,
    /// and returns the message carrying the book it sent once ready, if any, for the caller to read.
    /// A replacement that failed is logged and dropped with its token, and the feed streams on:
    /// `None`.
    pub(super) fn switch(
        &mut self,
        replaced: anyhow::Result<Replacement>,
        link: &mut Link,
        adapter: &mut Adapter,
    ) -> Option<Vec<String>> {
        let replacement = match replaced {
            Ok(replacement) => replacement,
            Err(error) => {
                let (venue, feed, error) =
                    (self.conn.venue.name(), self.conn.url.as_str(), self.redact(&format!("{error:#}")));
                info!(venue, feed, "replacing the connection failed, streaming on: {error}");
                self.next_token = None;
                return None;
            }
        };
        replacement.refused.iter().for_each(|(reason, text)| self.refuse(reason, text));
        (*link, *adapter) = (replacement.link, replacement.adapter);
        let earlier = mem::replace(&mut self.delay, replacement.delay);
        self.delay.keep_bounds(earlier);
        self.standing.connected(redates(self.conn.venue));
        // The engine's quote from the old connection says nothing of what this one sequences.
        self.sent.clear();
        self.token = self.next_token.take();
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        info!(venue, feed, "venue switched to a connection on a fresh bearer token");
        Some(replacement.books)
    }
}

#[cfg(test)]
mod tests;
