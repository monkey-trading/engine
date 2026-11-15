//! The member link's places: each peer address's share, and the connections that have not asked yet,
//! which give their place up to a newer one once every place is taken, so connections that never ask
//! hold none of the places status questions need.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

/// One peer address's share of the member link's connections, given back on drop.
pub(crate) struct PeerSlot {
    peers: Arc<Mutex<BTreeMap<IpAddr, usize>>>,
    peer: IpAddr,
}

/// The connections that have not sent their question yet, by connection number, oldest first, each with
/// what tells it to give its place up.
#[derive(Clone, Default)]
pub(crate) struct Silent(Arc<Mutex<BTreeMap<u64, Arc<Notify>>>>);

/// Connection `id`'s place among the silent ones, left on drop: once its question has come.
pub(crate) struct SilentPlace {
    silent: Silent,
    id: u64,
    evicted: Arc<Notify>,
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let mut held = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = held.get_mut(&self.peer) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.peer);
            }
        }
    }
}

impl Drop for SilentPlace {
    fn drop(&mut self) {
        self.silent.0.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id);
    }
}

impl PeerSlot {
    /// A slot for `peer`, unless it holds `limit` already.
    pub(crate) fn take(peers: &Arc<Mutex<BTreeMap<IpAddr, usize>>>, peer: IpAddr, limit: usize) -> Option<Self> {
        let mut held = peers.lock().unwrap_or_else(PoisonError::into_inner);
        let count = held.entry(peer).or_default();
        if *count >= limit {
            return None;
        }
        *count += 1;
        Some(Self { peers: peers.clone(), peer })
    }
}

impl Silent {
    /// Enters connection `id` among the silent ones.
    pub(crate) fn enter(&self, id: u64) -> SilentPlace {
        let evicted = Arc::new(Notify::new());
        self.0.lock().unwrap_or_else(PoisonError::into_inner).insert(id, evicted.clone());
        SilentPlace { silent: self.clone(), id, evicted }
    }

    /// Tells the oldest silent connection to give its place up; whether there was one.
    pub(crate) fn evict_oldest(&self) -> bool {
        let oldest = self.0.lock().unwrap_or_else(PoisonError::into_inner).pop_first();
        // `notify_one` stores a permit: a connection told before it waits still gives its place up.
        oldest.map(|(_, evicted)| evicted.notify_one()).is_some()
    }
}

impl SilentPlace {
    /// Resolves once a newer connection needs this one's place.
    pub(crate) async fn evicted(&self) {
        self.evicted.notified().await;
    }
}

/// The address a peer's connections count under: its IPv4 address, or its IPv6 /64, which one host
/// commonly holds whole.
pub(crate) fn peer_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            let mut octets = v6.octets();
            octets[8..].fill(0);
            IpAddr::V6(octets.into())
        }
        v4 => v4,
    }
}
