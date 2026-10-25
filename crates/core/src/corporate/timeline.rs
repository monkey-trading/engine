//! Corporate cutoffs, ex-date openings and financing share one chronological transition loop.

use std::collections::BTreeMap;

use crate::corporate::Adjustment;
use crate::engine::Engine;
use crate::event::Event;
use crate::types::Millis;

impl Engine {
    /// Advances past boundaries at recorded prices; target-time execution waits for the incoming
    /// quote. Returns whether target-time adjustments require an account-wide margin check.
    pub(crate) fn run_corporate_timeline(&mut self, previous: Millis, target: Millis, events: &mut Vec<Event>) -> bool {
        self.now = previous;
        loop {
            let corporate = self
                .corporate_actions
                .values()
                .filter(|p| p.failure.is_none())
                .map(|p| if p.quantities.is_none() { p.instruction.previous_close } else { p.instruction.ex_open })
                .min();
            let at = corporate.into_iter().chain(self.next_rollover).filter(|at| *at <= target).min();
            let Some(at) = at else { break };
            self.now = at;
            let captures: Vec<_> = self
                .corporate_actions
                .iter()
                .filter(|(_, p)| p.failure.is_none() && p.quantities.is_none() && p.instruction.previous_close == at)
                .map(|(id, p)| (id.clone(), p.instruction.market))
                .collect();
            for (id, market) in captures {
                let quantities = self
                    .hot_indexes
                    .as_ref()
                    .expect("apply builds derived indexes")
                    .holders
                    .get(&market)
                    .into_iter()
                    .flat_map(|holders| holders.iter())
                    .map(|account| (*account, self.accounts[account].positions[&market].qty))
                    .collect::<BTreeMap<_, _>>();
                events.push(Event::CorporateEntitlementsCaptured {
                    id: id.clone(),
                    market,
                    at,
                    accounts: quantities.len(),
                });
                self.corporate_actions.get_mut(&id).expect("selected above").quantities = Some(quantities);
            }
            self.refresh_all_sessions(events);
            // Same-open dividends pay the holdings captured at the previous close, in those share
            // units, before any split. Stable market/id ordering resolves ties across markets.
            let mut openings: Vec<_> = self
                .corporate_actions
                .iter()
                .filter(|(_, p)| p.failure.is_none() && p.quantities.is_some() && p.instruction.ex_open == at)
                .map(|(id, p)| {
                    (matches!(p.instruction.adjustment, Adjustment::Split { .. }), p.instruction.market, id.clone())
                })
                .collect();
            openings.sort();
            let adjusted = !openings.is_empty();
            for (_, market, id) in openings {
                let pending = self.corporate_actions[&id].clone();
                let result = if self.markets[&market].corporate_action_blocked {
                    Err("a failed split has blocked this market".into())
                } else {
                    match pending.instruction.adjustment {
                        Adjustment::Dividend { .. } => self.apply_corporate_dividend(&pending, events),
                        Adjustment::Split { new_units, old_units } => {
                            self.apply_corporate_split(&pending.instruction, new_units, old_units, events)
                        }
                    }
                };
                match result {
                    Ok(()) => {
                        self.corporate_actions.remove(&id);
                        events.push(Event::CorporateActionApplied { id, market, at });
                    }
                    // A failed dividend pays nothing and leaves dealing open; a failed split leaves
                    // share units ambiguous, so its market stops dealing and stays locked.
                    Err(reason) if matches!(pending.instruction.adjustment, Adjustment::Split { .. }) => {
                        self.corporate_actions.get_mut(&id).expect("pending action exists").failure =
                            Some(reason.clone());
                        self.markets.get_mut(&market).expect("pending market exists").corporate_action_blocked = true;
                        events.push(Event::CorporateActionFailed { id, market, at, reason });
                        self.refresh_session(market, events);
                    }
                    Err(reason) => {
                        self.corporate_actions.remove(&id);
                        events.push(Event::CorporateActionFailed { id, market, at, reason });
                    }
                }
            }
            if at == target {
                return adjusted;
            }
            // No opening margin check may see another market's pre-split books at this instant.
            self.advance_markets(None, events);
            if adjusted {
                self.check_all_closeouts(events);
            }
            if self.next_rollover == Some(at) {
                self.run_one_rollover(events);
            }
        }
        self.now = target;
        false
    }
}
