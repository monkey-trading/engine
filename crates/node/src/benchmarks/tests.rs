use super::*;
use engine_core::Fixed;

use crate::benchmarks::parse::parse;

fn date(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

#[test]
fn official_csv_shapes_preserve_dates_series_and_sub_basis_point_precision() {
    let cases = [
        (Publisher::Sofr, "Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,3.90\n", "0.039"),
        (Publisher::Estr, "KEY,TIME_PERIOD,OBS_VALUE\nEST.B.EU000A2X2A25.WT,2026-10-22,2.442\n", "0.02442"),
        (Publisher::Sonia, "DATE,IUDSOIA\n22 Oct 2026,3.9234\n", "0.039234"),
        (Publisher::Aonia, "F1 INTEREST RATES\nSeries ID,FIRMMCRTD,FIRMMCRID\n22-Oct-2026,4.60,4.59\n", "0.0459"),
        (
            Publisher::Tona,
            "BOJ's Main Time-series Statistics\n\"Series code\",FM01'STRDCLUCON\n\"Unit\",percent per annum\n2026/10/22,-0.012\n2026/10/23,NA\n",
            "-0.00012",
        ),
    ];
    for (publisher, csv, expected) in cases {
        let rates = parse(publisher, csv.as_bytes(), date("2026-10-20"), date("2026-10-23")).unwrap();
        assert_eq!(rates.len(), 1);
        assert_eq!(rates[0].benchmark, publisher.benchmark());
        assert_eq!(rates[0].effective_date, date("2026-10-22"));
        assert_eq!(rates[0].annual_rate, expected.parse::<Fixed>().unwrap());
    }
}

#[test]
fn invalid_publisher_input_is_refused_as_a_whole() {
    for csv in [
        "<html>blocked</html>",
        "Effective Date,Rate Type,Rate (%)\n10/22/2026,EFFR,3.90\n",
        "Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,3.90\n10/22/2026,SOFR,4.00\n",
        "Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,NaN\n",
        "Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,100.001\n",
        "Effective Date,Rate Type,Rate (%)\ninvalid,SOFR,3.90\n",
        "Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,0.123456789\n",
    ] {
        assert!(parse(Publisher::Sofr, csv.as_bytes(), date("2026-10-20"), date("2026-10-23")).is_err(), "{csv}");
    }
}

#[test]
fn catch_up_history_keeps_a_predecessor_and_excludes_future_observations() {
    let csv = b"Effective Date,Rate Type,Rate (%)\n10/16/2026,SOFR,3.80\n10/19/2026,SOFR,3.90\n10/20/2026,SOFR,4.00\n10/23/2026,SOFR,4.10\n";
    let rates = parse(Publisher::Sofr, csv, date("2026-10-20"), date("2026-10-22")).unwrap();
    assert_eq!(rates.iter().map(|r| r.effective_date).collect::<Vec<_>>(), [date("2026-10-19"), date("2026-10-20")]);
}

#[test]
fn request_bounds_use_each_publishers_effective_date_api() {
    for (publisher, expected) in [
        (Publisher::Sofr, "startDate=1970-01-01"),
        (Publisher::Estr, "startPeriod=1970-01-01"),
        (Publisher::Sonia, "Datefrom=01%2FJan%2F1970"),
    ] {
        let source = BenchmarkSource { publisher, endpoint: "https://example.test/history".into() };
        let url = publisher_url(&source, date("2026-10-23")).unwrap();
        assert!(url.as_str().contains(expected), "{url}");
    }
}

#[tokio::test]
async fn publisher_errors_and_timeouts_retain_the_previous_snapshot_policy() {
    use crate::config::BenchmarkSource;
    use axum::{Router, http::StatusCode, routing::get};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/bad", get(|| async { "<html>unavailable</html>" }))
        .route(
            "/http-error",
            get(|| async {
                (StatusCode::INTERNAL_SERVER_ERROR, "Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,3.90\n")
            }),
        )
        .route(
            "/oversized",
            get(|| async {
                let mut body = b"Effective Date,Rate Type,Rate (%)\n10/22/2026,SOFR,3.90\n".to_vec();
                body.resize(8 * 1024 * 1024 + 1, b'\n');
                body
            }),
        )
        .route("/timeout", get(|| async { std::future::pending::<&str>().await }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let state = BenchmarkState {
        next_rollover: None,
        rollover: None,
        required: ["SOFR".into()].into(),
        history: Default::default(),
        undated: Default::default(),
    };
    let client = client().unwrap();
    for (path, expected_error) in [
        ("bad", "no matching observations"),
        ("timeout", "timed out"),
        ("http-error", "500 Internal Server Error"),
        ("oversized", "publisher response exceeds byte bound"),
    ] {
        let cfg = BenchmarkSection {
            interval: std::time::Duration::from_secs(3600),
            timeout: if path == "timeout" {
                std::time::Duration::from_millis(100)
            } else {
                std::time::Duration::from_secs(10)
            },
            sources: vec![BenchmarkSource { publisher: Publisher::Sofr, endpoint: format!("{url}/{path}") }],
        };
        let err =
            fetch(&cfg.sources[0], &client, cfg.timeout, date("2026-10-20"), date("2026-10-23")).await.unwrap_err();
        assert!(err.to_string().contains(expected_error), "{path}: {err:#}");
        let batch = snapshot(&cfg, &client, &state, date("2026-10-23")).await.unwrap();
        assert_eq!(batch.checked, state.required);
        assert!(batch.rates.is_empty(), "invalid or unavailable data cannot supply a rate");
    }
    server.abort();
}

#[test]
fn an_observation_older_than_a_month_remains_available_as_a_stale_predecessor() {
    let csv = b"Effective Date,Rate Type,Rate (%)\n08/10/2026,SOFR,3.90\n";
    let rates = parse(Publisher::Sofr, csv, date("2026-10-22"), date("2026-10-23")).unwrap();
    assert_eq!(rates.len(), 1);
    assert_eq!(rates[0].effective_date, date("2026-08-10"));
    assert_eq!(rates[0].annual_rate, "0.039".parse().unwrap());
}
