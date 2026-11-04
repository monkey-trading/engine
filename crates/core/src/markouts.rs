//! Markouts: each fill queued, marked against the index once its horizon has passed, and folded
//! into its account's toxicity score.

use std::collections::BTreeMap;

use crate::Fixed;
use crate::engine::{Engine, Markout};
use crate::fixed::Rounding;
use crate::params::ProtocolParams;
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

/// Clamp on a single markout, so one wild print cannot dominate an account's toxicity score.
const MAX_MARKOUT_BPS: i64 = 1_000;

impl Engine {
    /// From `V16`, clamps every account's toxicity history to the memory `params` weighs markouts
    /// over. A lowered memory reweighs old history at once: without the clamp, the account's next
    /// markout still weighs the history it built under the earlier memory, and caps it only then.
    /// Raising the memory clamps nothing, since history never exceeds the memory it built under.
    /// `SetToxicityNotional` lands only where no history exists to clamp: parameters without
    /// weighting predate every notional-weighted markout. Read to select first: a touched account
    /// re-encodes its leaf at the next root, so touch only what the clamp changes.
    pub(crate) fn clamp_toxicity_history(&mut self, params: &ProtocolParams) {
        if self.protocol_version >= ProtocolVersion::V16
            && let Some(memory) = params.toxicity.notional.map(|weighting| weighting.memory)
        {
            let over: Vec<_> = self
                .accounts
                .iter()
                .filter(|(_, acct)| acct.toxicity_history > memory)
                .map(|(account, _)| *account)
                .collect();
            for account in over {
                self.accounts.get_mut(&account).expect("selected above").toxicity_history = memory;
            }
        }
    }

    /// Queues a fill's markout, with the fill's notional in USD. From `V11` the notional weighs it,
    /// and it joins the queue in due order, so a shortened horizon's markouts wait behind none;
    /// before, it carries no notional and joins the back, as it always did.
    pub(crate) fn record_markout(&mut self, mut markout: Markout) {
        if self.protocol_version >= ProtocolVersion::V11 {
            return self.insert_markout(markout);
        }
        markout.notional = Fixed::ZERO;
        self.queue_markout(markout);
    }

    /// Marks every fill whose horizon has passed and folds the markout into its account's score:
    /// from `V11` as `score_markouts` says, and before it at the market's last mark, in whole bps.
    pub(crate) fn process_markouts(&mut self) {
        let Some(params) = &self.params else { return };
        if self.protocol_version >= ProtocolVersion::V11 {
            return self.score_markouts();
        }
        let weight = params.toxicity.ewma_weight_bps;
        while self.markouts.front().is_some_and(|markout| markout.due <= self.now) {
            let markout = self.pop_markout().expect("front exists");
            let mark = self.markets.get(&markout.market).and_then(|market| market.mark);
            let (Some(mark), Some(acct)) = (mark, self.accounts.get_mut(&markout.account)) else {
                continue;
            };
            let bps = (mark - markout.price)
                .mul_int(markout.side.sign())
                .bps_of(markout.price)
                .clamp(-MAX_MARKOUT_BPS, MAX_MARKOUT_BPS);
            acct.toxicity_bps += (bps - acct.toxicity_bps) * weight / 10_000;
        }
    }

    /// From `V11`, scores each markout that has fallen due against the index a fill in its market
    /// would take now (`tradeable_index`), and averages it into its account's score by the fill's
    /// notional (`NotionalWeighting::averaged`). A markout with no such index waits for one, and one
    /// still waiting a horizon after it fell due is dropped unscored.
    fn score_markouts(&mut self) {
        let now = self.now;
        if self.markouts.front().is_none_or(|markout| markout.due > now) {
            return;
        }
        let params = self.params.as_ref().expect("callers check params are set");
        let horizon_ms = params.toxicity.horizon_ms;
        // `V11` activates, and takes parameters, only with notional weighting.
        let weighting = params.toxicity.notional.expect("V11 parameters weigh markouts by notional");
        let mut waiting = Vec::new();
        // Each market's index, worked out once for the pass.
        let mut indexes: BTreeMap<MarketId, Option<Fixed>> = BTreeMap::new();
        while self.markouts.front().is_some_and(|markout| markout.due <= now) {
            let markout = self.pop_markout().expect("front exists");
            // An index this long after the horizon no longer measures the fill's edge.
            if now - markout.due > horizon_ms {
                continue;
            }
            let index = *indexes
                .entry(markout.market)
                .or_insert_with(|| self.tradeable_index(markout.market).map(|(index, _)| index));
            let Some(index) = index else {
                waiting.push(markout);
                continue;
            };
            if let Some(acct) = self.accounts.get_mut(&markout.account) {
                let bps = markout_bps(&markout, index);
                let history = acct.toxicity_history_under(&weighting);
                let (score, history) = weighting.averaged(acct.toxicity_score(), history, bps, markout.notional);
                acct.set_toxicity_score(score);
                acct.toxicity_history = history;
            }
        }
        // Back at the front in due order, to try again at the next command; none holds back a
        // markout behind it whose market has a price.
        waiting.into_iter().rev().for_each(|markout| self.requeue_markout(markout));
    }
}

/// A fill's markout against `index`, in bps of its price: positive when the index has moved in the
/// trader's favour. Rounds up, in the house's favour, and clamps to `MAX_MARKOUT_BPS` either way.
fn markout_bps(markout: &Markout, index: Fixed) -> Fixed {
    let max = Fixed::from_int(MAX_MARKOUT_BPS);
    (index - markout.price)
        .mul_int(markout.side.sign())
        .mul_div(Fixed::from_int(10_000), markout.price, Rounding::Up)
        .clamp(-max, max)
}
