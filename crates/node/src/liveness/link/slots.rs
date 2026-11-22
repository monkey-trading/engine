//! The places of the listeners on a member's endpoints: each peer address's share, and the connections
//! that have not asked yet, which give their place up to a newer one once every place, or every place
//! of their address's share, is taken, so connections that never ask hold none of the places a
//! question needs. Behind address translation every peer shares one address, and its share then
//! churns as the whole listener does.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

/// A silent connection: the address it counts under, and what tells it to give its place up.
type Waiting = (IpAddr, Arc<Notify>);

/// One peer address's share of a listener's connections, given back on drop.
pub(crate) struct PeerSlot {
    peers: Arc<Mutex<BTreeMap<IpAddr, usize>>>,
    peer: IpAddr,
}

/// The connections that have not sent their question yet, by connection number, oldest first.
#[derive(Clone, Default)]
pub(crate) struct Silent(Arc<Mutex<BTreeMap<u64, Waiting>>>);

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
    /// A slot for `peer`: one of its `limit`, or, once it holds them all, the slot of its oldest
    /// connection among the `silent` ones, which is told to give it up; none while every connection it
    /// holds has asked. Whether one was told.
    pub(crate) fn take_or_evict(
        peers: &Arc<Mutex<BTreeMap<IpAddr, usize>>>,
        peer: IpAddr,
        limit: usize,
        silent: &Silent,
    ) -> Option<(Self, bool)> {
        let mut held = peers.lock().unwrap_or_else(PoisonError::into_inner);
        let count = held.entry(peer).or_default();
        let evicted = *count >= limit;
        if evicted && !silent.evict_oldest_of(peer) {
            return None;
        }
        // Past the limit only until the connection told to leave gives its slot back.
        *count += 1;
        Some((Self { peers: peers.clone(), peer }, evicted))
    }
}

impl Silent {
    /// Enters connection `id`, counted under `peer`, among the silent ones.
    pub(crate) fn enter(&self, id: u64, peer: IpAddr) -> SilentPlace {
        let evicted = Arc::new(Notify::new());
        self.0.lock().unwrap_or_else(PoisonError::into_inner).insert(id, (peer, evicted.clone()));
        SilentPlace { silent: self.clone(), id, evicted }
    }

    /// Tells the oldest silent connection to give its place up; whether there was one.
    pub(crate) fn evict_oldest(&self) -> bool {
        let oldest = self.0.lock().unwrap_or_else(PoisonError::into_inner).pop_first();
        // `notify_one` stores a permit: a connection told before it waits still gives its place up.
        oldest.map(|(_, (_, evicted))| evicted.notify_one()).is_some()
    }

    /// Makes room in a full listener for a new connection: whether a place is coming free. When the
    /// new connection's address had its own oldest silent connection told to leave (`evicted`), that
    /// place is the room, and no other is touched; otherwise the oldest silent connection is told.
    pub(crate) fn make_room(&self, evicted: bool) -> bool {
        evicted || self.evict_oldest()
    }

    /// Tells the oldest silent connection counted under `peer` to give its place up; whether there was
    /// one.
    pub(crate) fn evict_oldest_of(&self, peer: IpAddr) -> bool {
        let mut silent = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let oldest = silent.iter().find(|(_, (counted, _))| *counted == peer).map(|(id, _)| *id);
        let evicted = oldest.and_then(|id| silent.remove(&id));
        evicted.map(|(_, evicted)| evicted.notify_one()).is_some()
    }
}

impl SilentPlace {
    /// Resolves once a newer connection needs this one's place.
    pub(crate) async fn evicted(&self) {
        self.evicted.notified().await;
    }

    /// Leaves the silent ones as its question arrives: whether it still held its place, which a newer
    /// connection may have taken first, and then holds.
    pub(crate) fn leave(self) -> bool {
        self.silent.0.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.id).is_some()
    }
}

/// The address a peer's connections count under, as the API counts a client: its IPv4 address, also
/// when a dual-stack listener reports it mapped into IPv6, or its IPv6 /64, which one host commonly
/// holds whole.
pub(crate) fn peer_key(ip: IpAddr) -> IpAddr {
    crate::limits::client_key(ip)
}
