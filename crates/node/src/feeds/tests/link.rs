//! Test support: a loopback link between a feed and its venue stand-in that injects the network's
//! faults. It forwards bytes both ways, and on command holds what the venue sends (a stalled
//! connection, its queue growing behind), drops every connection and refuses new ones (a dropped
//! connection, or an outage), or throttles what the venue sends (a slow link, which queues seconds of
//! a large snapshot ahead of the pongs).

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::sleep;

/// The faults the link injects now.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Faults {
    /// Every connection is dropped, and new ones are refused.
    pub down: bool,
    /// Nothing the venue sends is passed on; it queues until the stall ends.
    pub stalled: bool,
    /// What the venue sends is passed on at most this many bytes a second.
    pub rate: Option<u64>,
}

/// A link listening on a loopback port, forwarding each connection to the venue.
pub struct FaultyLink {
    /// The `ws://` URL that reaches the venue through the link.
    pub url: String,
    faults: watch::Sender<Faults>,
    serving: JoinHandle<()>,
}

/// How much of what the venue sends is passed on at a time, so a throttle paces it finely.
const CHUNK: usize = 16 * 1024;

impl FaultyLink {
    /// A link to the venue at `venue`, a `ws://` loopback URL, injecting no fault.
    pub async fn start(venue: &str) -> Self {
        let upstream = venue.strip_prefix("ws://").expect("a ws:// venue").to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let url = format!("ws://{}", listener.local_addr().expect("local addr"));
        let (faults, watching) = watch::channel(Faults::default());
        let serving = tokio::spawn(async move {
            // Dropped with this task, which ends every connection.
            let mut connections = JoinSet::new();
            while let Ok((feed, _)) = listener.accept().await {
                if watching.borrow().down {
                    continue;
                }
                let Ok(venue) = TcpStream::connect(&upstream).await else { continue };
                connections.spawn(forward(feed, venue, watching.clone()));
            }
        });
        FaultyLink { url, faults, serving }
    }

    /// Injects `faults` from now on, on the open connections and new ones.
    pub fn set(&self, faults: Faults) {
        self.faults.send_replace(faults);
    }
}

impl Drop for FaultyLink {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

/// Forwards one connection until either side closes it or the link goes down.
async fn forward(feed: TcpStream, venue: TcpStream, mut faults: watch::Receiver<Faults>) {
    let ((mut feed_read, mut feed_write), (mut venue_read, mut venue_write)) = (feed.into_split(), venue.into_split());
    let mut down = faults.clone();
    let outbound = async { tokio::io::copy(&mut feed_read, &mut venue_write).await.map(drop) };
    let inbound = async {
        let mut buf = vec![0; CHUNK];
        loop {
            let read = venue_read.read(&mut buf).await?;
            if read == 0 {
                return Ok::<(), std::io::Error>(());
            }
            loop {
                let now = *faults.borrow_and_update();
                if now.down {
                    return Ok(());
                }
                if !now.stalled {
                    if let Some(rate) = now.rate {
                        sleep(Duration::from_secs_f64(read as f64 / rate as f64)).await;
                    }
                    break;
                }
                if faults.changed().await.is_err() {
                    return Ok(());
                }
            }
            feed_write.write_all(&buf[..read]).await?;
        }
    };
    // An error is the link itself gone, which ends the connection too.
    let went_down = async { drop(down.wait_for(|now| now.down).await) };
    tokio::select! {
        _ = outbound => {}
        _ = inbound => {}
        () = went_down => {}
    }
}
