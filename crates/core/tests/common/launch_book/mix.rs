//! The launch book's account mix: the shares #905 proposes, in one place, so the owner's changes to
//! them are edits to `Mix::proposal`, and the deterministic draws that deal them out.

use engine_core::types::MarketId;

const MARKET_MIX: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../research/cap-calibration/data/derived/market_mix.csv");

/// How the accounts of a book are dealt out. Shares are in per cent of accounts.
#[derive(Clone, Debug)]
pub struct Mix {
    /// Accounts in all, the named ones included.
    pub accounts: usize,
    /// The share of accounts holding each count of markets, as `(share, fewest, most)`; an account's
    /// count is drawn from its range, and its markets from the cap study's market mix.
    pub holdings: Vec<(u64, usize, usize)>,
    /// The share of accounts at each volume tier, from tier 0.
    pub tiers: Vec<u64>,
    /// The share of accounts resting orders, each at its tier's limit.
    pub resting: u64,
    /// The share of an account's resting orders inside its markets' first spread band, where a
    /// price could reach them; the rest rest away from the price, buys at half the mark and sells at
    /// twice it.
    pub in_band: u64,
    /// Session keys each account holds.
    pub session_keys: usize,
    /// Each position's size in USD at the book's mark of 100, and each account's cash.
    pub position_usd: i64,
    pub balance: i64,
}

impl Mix {
    /// #905's proposal.
    pub fn proposal(accounts: usize) -> Self {
        Mix {
            accounts,
            holdings: vec![(50, 1, 1), (30, 2, 4), (15, 5, 12), (5, 34, 34)],
            tiers: vec![70, 20, 8, 2],
            resting: 20,
            in_band: 50,
            session_keys: 2,
            position_usd: 5_000,
            balance: 100_000,
        }
    }
}

/// A deterministic stream of draws (splitmix64), so a book is the same on every run.
pub struct Rng(u64);

impl Rng {
    /// The stream for `seed`, one per account.
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xd1b5_4a32_d192_ed03)
    }

    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A draw in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// The index `draw` (in `0..100`) falls on in `shares`, which sum to 100.
    pub fn bucket(shares: impl IntoIterator<Item = u64>, draw: u64) -> usize {
        let mut seen = 0;
        for (index, share) in shares.into_iter().enumerate() {
            seen += share;
            if draw < seen {
                return index;
            }
        }
        panic!("the shares sum to less than 100");
    }

    /// `count` distinct markets drawn without replacement, each draw in proportion to the weights.
    pub fn markets(&mut self, count: usize, weights: &[(MarketId, u64)]) -> Vec<MarketId> {
        let mut left = weights.to_vec();
        let mut held = Vec::with_capacity(count);
        while held.len() < count {
            let total: u64 = left.iter().map(|(_, weight)| weight).sum();
            let mut at = self.below(total);
            let index = left.iter().position(|(_, weight)| match at.checked_sub(*weight) {
                Some(rest) => {
                    at = rest;
                    false
                }
                None => true,
            });
            held.push(left.remove(index.expect("a draw below the total lands on a market")).0);
        }
        held
    }
}

/// Each listed market's weight in the cap study's market mix (its share of trading volume, in
/// millionths, at least 1), by market id, for the markets `symbols` names.
pub fn market_weights(symbols: &[(MarketId, String)]) -> Vec<(MarketId, u64)> {
    let text = std::fs::read_to_string(MARKET_MIX).expect("the cap study's market mix");
    let shares: Vec<(&str, f64)> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let cells: Vec<&str> = line.split(',').collect();
            Some((*cells.first()?, cells.last()?.parse().ok()?))
        })
        .collect();
    symbols
        .iter()
        .map(|(id, symbol)| {
            let (_, share) = shares.iter().find(|(name, _)| name == symbol).unwrap_or_else(|| panic!("{symbol}"));
            (*id, ((share * 1e6).round() as u64).max(1))
        })
        .collect()
}
