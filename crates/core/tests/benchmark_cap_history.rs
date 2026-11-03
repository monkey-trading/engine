//! The `V46` cap against each launch benchmark's published history: the rates in
//! `research/benchmark-cap/fixtures/` replayed through `split_by_cap` in date order must refuse
//! exactly those `research/benchmark-cap/results/refused.csv` lists, which the study writes on its own
//! arithmetic. A change to the cap, or to the data, shows here as moves to report.

use chrono::NaiveDate;
use engine_core::Fixed;
use engine_core::benchmark::{BenchmarkHistory, BenchmarkRate, split_by_cap};

const HISTORIES: [(&str, &str); 5] = [
    ("SOFR", include_str!("../../../research/benchmark-cap/fixtures/sofr.csv")),
    ("ESTR", include_str!("../../../research/benchmark-cap/fixtures/estr.csv")),
    ("SONIA", include_str!("../../../research/benchmark-cap/fixtures/sonia.csv")),
    ("AONIA", include_str!("../../../research/benchmark-cap/fixtures/aonia.csv")),
    ("TONA", include_str!("../../../research/benchmark-cap/fixtures/tona.csv")),
];
const REFUSED: &str = include_str!("../../../research/benchmark-cap/results/refused.csv");

/// A rate in percent as the annual fraction the worker signs.
fn annual(percent: &str) -> Fixed {
    percent.parse::<Fixed>().expect("a published percent").div_int(100)
}

fn date(day: &str) -> NaiveDate {
    day.parse().expect("an ISO date")
}

#[test]
fn the_cap_refuses_exactly_the_published_moves_the_study_lists() {
    let mut refused: Vec<[String; 5]> = Vec::new();
    let (history, undated) = (BenchmarkHistory::new(), Default::default());
    for (benchmark, published) in HISTORIES {
        let rates: Vec<BenchmarkRate> = published
            .lines()
            .map(|line| {
                let (day, percent) = line.split_once(',').expect("date,percent");
                BenchmarkRate { benchmark: benchmark.into(), effective_date: date(day), annual_rate: annual(percent) }
            })
            .collect();
        let count = rates.len();
        let (admitted, found) = split_by_cap(&history, &undated, &rates);
        assert_eq!(admitted.len() + found.len(), count, "{benchmark}");
        refused.extend(found.into_iter().map(|refusal| {
            [
                benchmark.to_owned(),
                refusal.rate.effective_date.to_string(),
                refusal.rate.annual_rate.to_string(),
                refusal.from_date.expect("every rate here is dated").to_string(),
                refusal.from_rate.to_string(),
            ]
        }));
    }
    let listed: Vec<[String; 5]> = REFUSED
        .lines()
        .skip(1)
        .map(|line| {
            let cells: Vec<&str> = line.split(',').collect();
            [
                cells[0].to_owned(),
                cells[1].to_owned(),
                annual(cells[2]).to_string(),
                cells[3].to_owned(),
                annual(cells[4]).to_string(),
            ]
        })
        .collect();
    assert_eq!(refused, listed);
}

#[test]
fn no_launch_benchmark_has_a_refused_move_since_june_1988() {
    let last = REFUSED.lines().skip(1).filter_map(|line| line.split(',').nth(1)).map(date).max();
    assert_eq!(last, Some(date("1988-06-10")), "a later refusal is a finding to report, not to absorb");
    assert!(REFUSED.lines().skip(1).all(|line| line.starts_with("AONIA,")), "only AONIA's 1976-1988 rates");
}
