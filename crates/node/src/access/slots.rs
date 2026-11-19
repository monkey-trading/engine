//! The API's screening slots: how many requests may wait on the access source at once, in all, for
//! one account and for one client, with a reserve that only requests reducing risk may take, one of
//! its slots at a time for an account, and one slot for a signed request however many copies of it
//! arrive. Each request takes its slot on its own task, so the count is shared rather than owned by
//! one task, as the API's other limits are.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::{Mutex, PoisonError};

use engine_core::eip712::B256;
use engine_core::types::AccountId;
use tracing::debug;

use crate::config::AccessSection;
use crate::limits::{SHARED_CAP_FULL, ThrottledAlert, client_key};

/// What a request takes a slot as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotKind {
    /// A close, a close-all, a position's take profit and stop loss or a withdrawal that the API's engine
    /// copy shows can reduce risk, or a reissue of an unpaid payout the sequencer holds: any free slot,
    /// the reserve included while its account holds none of the reserve's.
    Reducing,
    /// Any other request but a declaration: a free slot outside the reserve.
    Opening,
    /// A declaration: as `Opening`, while declarations hold fewer than half the slots, rounded up.
    Declaration,
}

/// Why a request got no slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotRefusal {
    /// A copy of the same signed request holds one.
    Duplicate,
    /// The request's account, or its client, holds its share.
    Share,
    /// Declarations hold their half.
    Declarations,
    /// Every slot the request may take is held.
    Full,
}

/// A request's claim on a slot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Claim {
    /// The account the request acts for.
    pub(crate) account: AccountId,
    /// The client that sent it, when one did.
    pub(crate) client: Option<IpAddr>,
    /// The digest its signature covers, which every copy of it shares.
    pub(crate) request: B256,
    pub(crate) kind: SlotKind,
}

/// The slots `[access]` sets, and what holds them.
pub(crate) struct ScreeningSlots {
    total: usize,
    per_account: usize,
    per_client: usize,
    reserved: usize,
    declarations: usize,
    /// Locked only to count a request in or out, never across an await.
    held: Mutex<Held>,
    /// Raised while requests are refused for want of a slot.
    full: ThrottledAlert,
    /// Raised while declarations are refused for want of theirs.
    declarations_full: ThrottledAlert,
}

/// The slots held, in all, by declarations, per account, per client, and per request, and the accounts
/// holding a slot taken from the reserve.
#[derive(Default)]
struct Held {
    total: usize,
    declarations: usize,
    accounts: HashMap<AccountId, usize>,
    clients: HashMap<IpAddr, usize>,
    requests: HashSet<B256>,
    reserve: HashSet<AccountId>,
}

/// One request's slot, given back when it is dropped.
pub(crate) struct ScreeningSlot<'a> {
    slots: &'a ScreeningSlots,
    claim: Claim,
    /// Taken while every slot outside the reserve was held.
    reserved: bool,
}

impl Drop for ScreeningSlot<'_> {
    /// Gives the slot back, forgetting an account, client or request that holds none.
    fn drop(&mut self) {
        let mut held = self.slots.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.total -= 1;
        held.declarations -= usize::from(self.claim.kind == SlotKind::Declaration);
        release(&mut held.accounts, self.claim.account);
        if let Some(client) = self.claim.client {
            release(&mut held.clients, client);
        }
        held.requests.remove(&self.claim.request);
        if self.reserved {
            held.reserve.remove(&self.claim.account);
        }
    }
}

impl ScreeningSlots {
    /// The slots of a validated `[access]` section: declarations may hold half of them, rounded up.
    pub(crate) fn new(config: &AccessSection) -> Self {
        Self {
            total: config.max_in_flight.get(),
            per_account: config.max_in_flight_per_account.get(),
            per_client: config.max_in_flight_per_client.get(),
            reserved: config.reserved_in_flight,
            declarations: config.max_in_flight.get().div_ceil(2),
            held: Mutex::default(),
            full: ThrottledAlert::new("api_screening_slots_full", SHARED_CAP_FULL),
            declarations_full: ThrottledAlert::new("api_declaration_slots_full", SHARED_CAP_FULL),
        }
    }

    /// A slot for `claim`, held until the slot is dropped. A refusal for want of a free slot raises
    /// `api_screening_slots_full`, and one for want of a declaration's `api_declaration_slots_full`.
    /// Error cases:
    /// - a copy of the request holds a slot: `Duplicate`
    /// - its account or its client holds its share: `Share`
    /// - it is a declaration, and declarations hold their half: `Declarations`
    /// - every slot it may take is held, the reserve's too once its account holds one of them: `Full`
    pub(crate) fn take(&self, claim: Claim) -> Result<ScreeningSlot<'_>, SlotRefusal> {
        let claim = Claim { client: claim.client.map(client_key), ..claim };
        let mut reserved = false;
        let refused = {
            let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
            let refused = self.refusal(&held, &claim);
            if refused.is_none() {
                // Past every slot outside the reserve, the slot is one of the reserve's.
                reserved = held.total >= self.total - self.reserved;
                held.total += 1;
                held.declarations += usize::from(claim.kind == SlotKind::Declaration);
                *held.accounts.entry(claim.account).or_default() += 1;
                if let Some(client) = claim.client {
                    *held.clients.entry(client).or_default() += 1;
                }
                held.requests.insert(claim.request);
                if reserved {
                    held.reserve.insert(claim.account);
                }
            }
            refused
        };
        let Some(refusal) = refused else { return Ok(ScreeningSlot { slots: self, claim, reserved }) };
        debug!(?refusal, kind = ?claim.kind, "screening slot refused");
        match refusal {
            SlotRefusal::Full => self.full.raise(&"screening slots"),
            SlotRefusal::Declarations => self.declarations_full.raise(&"declaration screening slots"),
            SlotRefusal::Duplicate | SlotRefusal::Share => {}
        }
        Err(refusal)
    }

    /// Why `held` leaves no slot for `claim`, if it does not.
    fn refusal(&self, held: &Held, claim: &Claim) -> Option<SlotRefusal> {
        let holds = |count: Option<&usize>, share: usize| count.is_some_and(|count| *count >= share);
        let room = match claim.kind {
            SlotKind::Reducing if !held.reserve.contains(&claim.account) => self.total,
            SlotKind::Reducing | SlotKind::Opening | SlotKind::Declaration => self.total - self.reserved,
        };
        if held.requests.contains(&claim.request) {
            Some(SlotRefusal::Duplicate)
        } else if holds(held.accounts.get(&claim.account), self.per_account)
            || claim.client.is_some_and(|client| holds(held.clients.get(&client), self.per_client))
        {
            Some(SlotRefusal::Share)
        } else if claim.kind == SlotKind::Declaration && held.declarations >= self.declarations {
            Some(SlotRefusal::Declarations)
        } else if held.total >= room {
            Some(SlotRefusal::Full)
        } else {
            None
        }
    }

    /// Takes every slot, each for its own account and request, as a flood of closes would.
    #[cfg(test)]
    pub(crate) fn fill(&self) -> Vec<ScreeningSlot<'_>> {
        (0..self.total)
            .map(|slot| {
                let byte = u8::try_from(slot).expect("a test takes at most 256 slots");
                let account = AccountId([byte; 20]);
                let claim = Claim { account, client: None, request: B256::repeat_byte(byte), kind: SlotKind::Reducing };
                self.take(claim).expect("a free slot")
            })
            .collect()
    }
}

/// Counts one slot out of `key`'s, forgetting a key that holds none.
fn release<K: Hash + Eq>(counts: &mut HashMap<K, usize>, key: K) {
    if let Some(count) = counts.get_mut(&key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_copy_of_a_request_in_flight_takes_no_second_slot() {
        let slots = pool(4, 2, 4, 1);
        let order = claim(1, 1, 1, SlotKind::Opening);
        let first = slots.take(order).unwrap();
        assert_eq!(slots.take(order).err(), Some(SlotRefusal::Duplicate));
        assert_eq!(slots.take(Claim { kind: SlotKind::Reducing, ..order }).err(), Some(SlotRefusal::Duplicate));
        // The copy held nothing: the account's second slot is still free for another request.
        let other = slots.take(claim(1, 1, 2, SlotKind::Opening)).unwrap();
        drop((first, other));
        let _again = slots.take(order).unwrap();
    }

    #[test]
    fn an_account_and_a_client_hold_at_most_their_share() {
        let slots = pool(8, 2, 4, 2);
        // An account's share counts every client it comes from, and every kind of request.
        let _account = [(1, SlotKind::Opening), (2, SlotKind::Reducing)]
            .map(|(n, kind)| slots.take(claim(1, n, n, kind)).unwrap());
        assert_eq!(slots.take(claim(1, 3, 3, SlotKind::Reducing)).err(), Some(SlotRefusal::Share));
        // A client's share counts every account it acts for: client 1 holds account 1's slot and three more.
        let _client = [2, 3, 4].map(|n| slots.take(claim(n, 1, 10 + n, SlotKind::Declaration)).unwrap());
        assert_eq!(slots.take(claim(5, 1, 15, SlotKind::Reducing)).err(), Some(SlotRefusal::Share));
        // Another client and account still take a slot.
        let _other = slots.take(claim(5, 5, 15, SlotKind::Reducing)).unwrap();

        // An IPv6 client is counted by its /64.
        let slots = pool(8, 1, 2, 0);
        let v6 = |net: u16, host: u16, account: u8| Claim {
            client: Some(IpAddr::from([0x2001, 0xdb8, 0, net, 0, 0, 0, host])),
            ..claim(account, 0, account, SlotKind::Opening)
        };
        let _held = [(1, 1), (2, 2)].map(|(host, account)| slots.take(v6(0, host, account)).unwrap());
        assert_eq!(slots.take(v6(0, 3, 3)).err(), Some(SlotRefusal::Share));
        let _next = slots.take(v6(1, 3, 3)).unwrap();
    }

    #[test]
    fn the_reserve_is_left_to_closes_and_withdrawals() {
        let slots = pool(8, 1, 8, 2);
        let _opening: Vec<_> = (1..=6).map(|n| slots.take(claim(n, n, n, SlotKind::Opening)).unwrap()).collect();
        for kind in [SlotKind::Opening, SlotKind::Declaration] {
            assert_eq!(slots.take(claim(7, 7, 7, kind)).err(), Some(SlotRefusal::Full), "{kind:?}");
        }
        let _reserve: Vec<_> = (7..=8).map(|n| slots.take(claim(n, n, n, SlotKind::Reducing)).unwrap()).collect();
        assert_eq!(slots.take(claim(9, 9, 9, SlotKind::Reducing)).err(), Some(SlotRefusal::Full));
    }

    #[test]
    fn an_account_holds_one_slot_of_the_reserve_at_a_time() {
        let slots = pool(8, 3, 8, 2);
        let _opening: Vec<_> = (1..=6).map(|n| slots.take(claim(n, n, n, SlotKind::Opening)).unwrap()).collect();
        let first = slots.take(claim(7, 7, 7, SlotKind::Reducing)).unwrap();
        // Within its share, the account's second request finds only the slots outside the reserve held.
        assert_eq!(slots.take(claim(7, 7, 17, SlotKind::Reducing)).err(), Some(SlotRefusal::Full));
        let other = slots.take(claim(8, 8, 8, SlotKind::Reducing)).unwrap();
        drop(first);
        let _again = slots.take(claim(7, 7, 17, SlotKind::Reducing)).unwrap();
        // A slot outside the reserve is not one of the reserve's: account 1 still takes one.
        drop(other);
        let _own = slots.take(claim(1, 1, 11, SlotKind::Reducing)).unwrap();
    }

    #[test]
    fn a_reducing_slot_outside_the_reserve_is_not_one_of_the_reserves() {
        let slots = pool(8, 3, 8, 2);
        // While the pool has room, account 7's close takes a slot outside the reserve.
        let ordinary = slots.take(claim(7, 7, 7, SlotKind::Reducing)).unwrap();
        let _opening: Vec<_> = (1..=5).map(|n| slots.take(claim(n, n, n, SlotKind::Opening)).unwrap()).collect();
        // Once the rest are held, its next close takes one of the reserve's, and its third none.
        let _reserved = slots.take(claim(7, 7, 17, SlotKind::Reducing)).unwrap();
        assert_eq!(slots.take(claim(7, 7, 27, SlotKind::Reducing)).err(), Some(SlotRefusal::Full));
        // Giving back the slot outside the reserve leaves the account holding the reserve's.
        drop(ordinary);
        assert_eq!(slots.take(claim(7, 7, 27, SlotKind::Reducing)).err(), Some(SlotRefusal::Full));
    }

    #[test]
    fn declarations_leave_an_order_a_slot_at_every_accepted_capacity() {
        for total in 2..=17 {
            for reserved in 0..total / 2 {
                let slots = pool(total, 1, total, reserved);
                let declaring: Vec<_> =
                    (1..=total as u8).map_while(|n| slots.take(claim(n, n, n, SlotKind::Declaration)).ok()).collect();
                assert!(!declaring.is_empty(), "a declaration takes a slot of {total}");
                let order = slots.take(claim(0xff, 0xff, 0xff, SlotKind::Opening));
                assert!(order.is_ok(), "{} declarations of {total}, {reserved} reserved", declaring.len());
            }
        }
    }

    /// Slots of `total`, `per_account` and `per_client` shares, and `reserved` for closes.
    fn pool(total: usize, per_account: usize, per_client: usize, reserved: usize) -> ScreeningSlots {
        let config: AccessSection = toml::from_str(&format!(
            "source='0x{}'\nendpoint='http://127.0.0.1:9'\ntimeout='100ms'\nmax_age='1s'\nmax_response_bytes=4096\n\
             max_in_flight={total}\nmax_in_flight_per_account={per_account}\nmax_in_flight_per_client={per_client}\n\
             reserved_in_flight={reserved}",
            "11".repeat(20),
        ))
        .unwrap();
        config.validate().unwrap();
        ScreeningSlots::new(&config)
    }

    /// Account `account`'s request `request` from client `203.0.113.<client>`, or no client at 0.
    fn claim(account: u8, client: u8, request: u8, kind: SlotKind) -> Claim {
        Claim {
            account: AccountId([account; 20]),
            client: (client > 0).then(|| IpAddr::from([203, 0, 113, client])),
            request: B256::repeat_byte(request),
            kind,
        }
    }
}
