//! End-to-end tests: the real binary against a local stand-in for REData.
//!
//! Response bodies follow the shape of live REData answers (see `tests/fixtures`).
//! The clock is pinned with `VATIO_NOW` so every run is deterministic.

use std::{
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::Duration,
};

use jiff::{Timestamp, civil::Date, tz::TimeZone};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const PRICES_PATH: &str = "/es/datos/mercados/precios-mercados-tiempo-real";
/// 19:40:27 in Madrid on Thursday 26 November 2026.
const NOW: &str = "2026-11-26T18:40:27Z";

fn madrid() -> TimeZone {
    TimeZone::get("Europe/Madrid").unwrap()
}

/// REData's timestamp format: `2026-11-25T00:00:00.000+01:00`.
fn stamp(at: Timestamp) -> String {
    at.to_zoned_datetime(madrid())
        .strftime("%Y-%m-%dT%H:%M:%S.000%:z")
        .to_string()
}

fn day_bounds(day: &str) -> (i64, i64) {
    let date: Date = day.parse().unwrap();
    let start = date
        .to_zoned_datetime(madrid())
        .unwrap()
        .timestamp()
        .as_second();
    let end = date
        .tomorrow()
        .unwrap()
        .to_zoned_datetime(madrid())
        .unwrap()
        .timestamp()
        .as_second();
    (start, end)
}

/// One series covering the whole day at `step` seconds, priced by `f(slot index)`.
fn values(day: &str, step: i64, f: impl Fn(i64) -> f64) -> Vec<Value> {
    let (start, end) = day_bounds(day);
    (0..(end - start) / step)
        .map(|i| {
            json!({
                "value": f(i),
                "percentage": 0.5,
                "datetime": stamp(Timestamp::from_second(start + i * step).unwrap()),
            })
        })
        .collect()
}

fn envelope(pvpc: Vec<Value>, spot: Vec<Value>) -> Value {
    let series = |kind: &str, id: &str, updated: &str, values: Vec<Value>| {
        json!({
            "type": kind, "id": id, "groupId": null,
            "attributes": {
                "title": kind, "description": null, "color": "#ffcf09", "icon": null, "type": null,
                "magnitude": "price", "composite": false, "last-update": updated, "values": values
            }
        })
    };
    json!({
        "data": {
            "type": "Precios mercado peninsular en tiempo real", "id": "mer13",
            "attributes": {
                "title": "Precios mercado peninsular en tiempo real",
                "last-update": "2026-11-25T20:46:06.000+01:00", "description": null
            },
            "meta": {"cache-control": {"cache": "MISS"}}
        },
        "included": [
            series("PVPC", "1001", "2026-11-25T20:46:06.000+01:00", pvpc),
            series("Precio mercado spot", "600", "2026-11-25T15:03:37.000+01:00", spot),
        ]
    })
}

/// Hourly PVPC from a 24-value profile (repeated on longer days) and flat-ish quarter-hour spot.
fn day_body(day: &str, hourly: &[f64]) -> Value {
    let hourly = hourly.to_vec();
    envelope(
        values(day, 3600, move |i| hourly[i as usize % hourly.len()]),
        values(day, 900, |i| 80.0 + (i % 8) as f64),
    )
}

/// A REData answer from the live API (see tests/fixtures/README.md).
fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))).unwrap()
}

/// What the live API answers, under HTTP 502, for a range it holds nothing for. When
/// only one of the two markets is out, it answers 200 with the other series absent
/// (`unpublished`).
fn no_data_body() -> Value {
    fixture("redata_error_no_data_502.json")
}

/// Thursday's profile: expensive evening, cheap small hours. Index = hour of day.
const THURSDAY: [f64; 24] = [
    120.0, 110.0, 100.0, 100.0, 105.0, 115.0, 150.0, 190.0, 210.0, 200.0, 170.0, 150.0, 140.0,
    130.0, 135.0, 150.0, 180.0, 230.0, 270.0, 280.0, 260.0, 220.0, 170.0, 140.0,
];
/// Friday is cheaper overnight than anything left of Thursday evening.
const FRIDAY: [f64; 24] = [
    90.0, 70.0, 60.0, 65.0, 80.0, 100.0, 140.0, 180.0, 200.0, 190.0, 160.0, 140.0, 120.0, 110.0,
    115.0, 130.0, 170.0, 220.0, 250.0, 260.0, 240.0, 200.0, 160.0, 130.0,
];

struct Env {
    server: MockServer,
    cache: PathBuf,
    now: &'static str,
}

impl Env {
    async fn new(name: &str) -> Self {
        let cache = std::env::temp_dir().join(format!("vatio-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        Self {
            server: MockServer::start().await,
            cache,
            now: NOW,
        }
    }

    /// Serve the same answer for every request to `widget`.
    async fn serve_fixture(&self, widget: &str, status: u16, body: Value) {
        Mock::given(method("GET"))
            .and(path(format!("/es/datos/{widget}")))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    async fn publish(&self, day: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(PRICES_PATH))
            .and(query_param("start_date", format!("{day}T00:00")))
            .and(query_param("end_date", format!("{day}T23:59")))
            .and(query_param("time_trunc", "hour"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&self.server)
            .await;
    }

    /// What REData answers for tomorrow in the evening before PVPC is out: a normal
    /// success document that carries the spot series and simply has no PVPC series.
    async fn unpublished(&self, day: &str) {
        let mut body = envelope(vec![], values(day, 900, |i| 80.0 + (i % 8) as f64));
        body["included"].as_array_mut().unwrap().remove(0);
        self.publish(day, body).await;
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vatio"));
        cmd.args(args)
            .env("VATIO_API_URL", self.server.uri())
            .env("VATIO_CACHE_DIR", &self.cache)
            .env("VATIO_NOW", self.now)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env_remove("VATIO_NO_CACHE")
            .env_remove("NO_COLOR");
        cmd
    }

    async fn run(&self, args: &[&str]) -> Output {
        let mut cmd = self.command(args);
        tokio::task::spawn_blocking(move || cmd.output().unwrap())
            .await
            .unwrap()
    }

    /// Run with `--json` and return (exit code, stdout as JSON or Null, stderr as JSON or Null).
    async fn json(&self, args: &[&str]) -> (i32, Value, Value) {
        let mut all = args.to_vec();
        all.push("--json");
        let out = self.run(&all).await;
        let parse = |bytes: &[u8]| serde_json::from_slice(bytes).unwrap_or(Value::Null);
        (
            out.status.code().unwrap(),
            parse(&out.stdout),
            parse(&out.stderr),
        )
    }

    async fn requests(&self) -> usize {
        self.server.received_requests().await.unwrap().len()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn prices_for_today_follow_the_contract_and_are_cached_once_complete() {
    let env = Env::new("prices").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;

    let (code, doc, err) = env.json(&["prices"]).await;
    assert_eq!((code, &err), (0, &Value::Null));
    assert_eq!(doc["schema"], "vatio.prices/v1");
    assert_eq!(doc["market"], "pvpc");
    assert_eq!(doc["day"], "2026-11-26");
    assert_eq!(doc["timezone"], "Europe/Madrid");
    assert_eq!(doc["resolution_minutes"], 60);
    assert_eq!(doc["complete"], true);
    assert_eq!(doc["area"], "peninsula");
    assert_eq!(doc["points"].as_array().unwrap().len(), 24);
    assert_eq!(
        doc["points"][0],
        json!({
            "start": "2026-11-26T00:00:00+01:00", "end": "2026-11-26T01:00:00+01:00",
            "eur_per_mwh": 120.0, "eur_per_kwh": 0.12
        })
    );
    // Ties (two hours at 100) resolve to the earliest.
    assert_eq!(doc["stats"]["min"]["start"], "2026-11-26T02:00:00+01:00");
    assert_eq!(doc["stats"]["max"]["start"], "2026-11-26T19:00:00+01:00");
    assert_eq!(doc["stats"]["mean_eur_per_mwh"], 167.71);
    assert_eq!(doc["stats"]["mean_eur_per_kwh"], 0.16771);
    // Who published it and when they last updated it travel with every answer.
    assert_eq!(
        doc["sources"][0]["name"],
        "Red Eléctrica de España (REData)"
    );
    assert_eq!(doc["sources"][0]["dataset"], "prices");
    assert_eq!(doc["sources"][0]["updated_at"], "2026-11-25T20:46:06+01:00");
    assert_eq!(doc["sources"][0]["cached"], false);
    assert_eq!(doc["sources"][0]["fetched_at"], "2026-11-26T19:40:27+01:00");
    assert!(doc["sources"][0]["url"].as_str().unwrap().ends_with(
        "/es/datos/mercados/precios-mercados-tiempo-real?start_date=2026-11-26T00:00&end_date=2026-11-26T23:59&time_trunc=hour"
    ));

    // Second call, and the other market from the same response, need no request.
    let (_, again, _) = env.json(&["prices"]).await;
    assert_eq!(again["sources"][0]["cached"], true);
    assert_eq!(
        again["sources"][0]["updated_at"],
        doc["sources"][0]["updated_at"]
    );
    assert_eq!(again["points"], doc["points"]);
    let (_, spot, _) = env.json(&["prices", "--market", "spot"]).await;
    assert_eq!(spot["sources"][0]["cached"], true);
    assert_eq!(
        (
            &spot["resolution_minutes"],
            spot["points"].as_array().unwrap().len()
        ),
        (&json!(15), 96)
    );
    assert_eq!(spot["points"][1]["start"], "2026-11-26T00:15:00+01:00");
    assert_eq!(env.requests().await, 1);

    // --refresh goes back to the network.
    let (_, fresh, _) = env.json(&["prices", "--refresh"]).await;
    assert_eq!(fresh["sources"][0]["cached"], false);
    assert_eq!(env.requests().await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn unpublished_tomorrow_is_exit_3_with_a_structured_error() {
    let env = Env::new("unpublished").await;
    env.unpublished("2026-11-27").await;

    let (code, out, err) = env.json(&["prices", "tomorrow"]).await;
    assert_eq!(
        (code, &out),
        (3, &Value::Null),
        "nothing on stdout when it fails"
    );
    assert_eq!(err["error"]["code"], "not_available");
    assert_eq!(err["error"]["retryable"], true);
    assert_eq!(
        err["error"]["message"],
        "PVPC prices for 2026-11-27 are not published yet"
    );
    assert!(err["error"]["hint"].as_str().unwrap().contains("20:45"));

    let human = env.run(&["prices", "tomorrow"]).await;
    assert_eq!(human.status.code(), Some(3));
    let text = String::from_utf8(human.stderr).unwrap();
    assert!(
        text.starts_with("error: PVPC prices for 2026-11-27 are not published yet\n  hint: "),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_day_with_spot_out_but_no_pvpc_yet_is_not_available_for_pvpc() {
    // What REData answers between about 15:00 and 20:45: a normal success document in
    // which tomorrow's spot series is present and the PVPC series is simply absent.
    let env = Env::new("absent").await;
    let mut body = envelope(vec![], values("2026-11-27", 900, |_| 50.0));
    body["included"].as_array_mut().unwrap().remove(0);
    env.publish("2026-11-27", body).await;

    let (code, _, err) = env.json(&["prices", "tomorrow"]).await;
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_available")));
    let (code, spot, _) = env.json(&["prices", "tomorrow", "-m", "spot"]).await;
    assert_eq!((code, spot["points"].as_array().unwrap().len()), (0, 96));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_published_day_with_an_empty_series_is_also_not_available() {
    let env = Env::new("empty").await;
    env.publish(
        "2026-11-27",
        envelope(vec![], values("2026-11-27", 900, |_| 50.0)),
    )
    .await;

    let (code, _, err) = env.json(&["prices", "tomorrow"]).await;
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_available")));
    let (code, spot, _) = env.json(&["prices", "tomorrow", "-m", "spot"]).await;
    assert_eq!((code, spot["points"].as_array().unwrap().len()), (0, 96));
}

#[tokio::test(flavor = "multi_thread")]
async fn now_reports_the_current_interval_and_its_rank() {
    let env = Env::new("now").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;

    let (code, doc, _) = env.json(&["now"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["schema"], "vatio.now/v1");
    assert_eq!(doc["at"], "2026-11-26T19:40:27+01:00");
    assert_eq!(doc["current"]["start"], "2026-11-26T19:00:00+01:00");
    assert_eq!(doc["current"]["eur_per_kwh"], 0.28);
    assert_eq!(doc["next"]["start"], "2026-11-26T20:00:00+01:00");
    assert_eq!(doc["next"]["eur_per_mwh"], 260.0);
    // 19:00 is the most expensive hour of the day.
    assert_eq!(
        (&doc["rank"], &doc["rank_of"], &doc["level"]),
        (&json!(24), &json!(24), &json!("high"))
    );
    assert_eq!(doc["today"]["min"]["eur_per_mwh"], 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn cheapest_searches_across_midnight_once_tomorrow_is_published() {
    let env = Env::new("cheapest").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.publish("2026-11-27", day_body("2026-11-27", &FRIDAY))
        .await;

    let (code, doc, _) = env.json(&["cheapest", "3h", "--top", "2"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["schema"], "vatio.cheapest/v1");
    assert_eq!(doc["duration_minutes"], 180);
    // Search starts at the current minute and runs to the end of tomorrow.
    assert_eq!(
        doc["searched"],
        json!({"from": "2026-11-26T19:40:00+01:00", "until": "2026-11-28T00:00:00+01:00", "tomorrow": "included"})
    );
    // Friday 01:00-04:00 = (70 + 60 + 65) / 3.
    assert_eq!(
        doc["best"],
        json!({
            "start": "2026-11-27T01:00:00+01:00", "end": "2026-11-27T04:00:00+01:00",
            "mean_eur_per_mwh": 65.0, "mean_eur_per_kwh": 0.065, "starts_in_minutes": 320
        })
    );
    assert_eq!(doc["alternatives"].as_array().unwrap().len(), 1);
    // Starting now: 20 min at 280, 60 at 260, 60 at 220, 40 at 170.
    let now_mean = (280.0 * 20.0 + 260.0 * 60.0 + 220.0 * 60.0 + 170.0 * 40.0) / 180.0;
    assert_eq!(doc["earliest"]["start"], "2026-11-26T19:40:00+01:00");
    assert_eq!(doc["earliest"]["end"], "2026-11-26T22:40:00+01:00");
    assert_eq!(
        doc["earliest"]["mean_eur_per_mwh"],
        (now_mean * 100.0f64).round() / 100.0
    );
    assert_eq!(
        doc["saving_percent"],
        ((now_mean - 65.0) / now_mean * 1000.0).round() / 10.0
    );
    assert_eq!(doc["warnings"], json!([]));
    assert_eq!(doc["sources"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn cheapest_respects_a_deadline_and_says_when_data_runs_out() {
    let env = Env::new("deadline").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.unpublished("2026-11-27").await;

    // Deadline inside today's data: the run must finish by 23:00, so the cheapest
    // remaining 2h block is 21:00-23:00 (220 and 170).
    let (code, doc, _) = env.json(&["cheapest", "2h", "--until", "23:00"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["searched"]["until"], "2026-11-26T23:00:00+01:00");
    assert_eq!(doc["searched"]["tomorrow"], "not_needed");
    assert_eq!(doc["best"]["start"], "2026-11-26T21:00:00+01:00");
    assert_eq!(doc["best"]["mean_eur_per_mwh"], 195.0);
    assert_eq!(doc["warnings"], json!([]));

    // Deadline tomorrow morning while tomorrow is unpublished: answer from what
    // exists and say so.
    let (code, doc, _) = env.json(&["cheapest", "2h", "--until", "08:00"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["searched"]["until"], "2026-11-27T00:00:00+01:00");
    assert_eq!(doc["best"]["start"], "2026-11-26T22:00:00+01:00");
    let warning = doc["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("published only until 2026-11-27T00:00:00+01:00"),
        "{warning}"
    );

    // A run longer than the published remainder cannot be answered yet.
    let (code, _, err) = env.json(&["cheapest", "6h"]).await;
    assert_eq!(
        (code, &err["error"]["code"], &err["error"]["retryable"]),
        (3, &json!("not_available"), &json!(true))
    );

    // A search that starts on a day with nothing published says exactly that.
    let (code, _, err) = env
        .json(&["cheapest", "2h", "--from", "2026-11-27T09:00"])
        .await;
    assert_eq!(code, 3);
    assert_eq!(
        err["error"]["message"],
        "PVPC prices for 2026-11-27 are not published yet"
    );

    // A run that cannot fit before its own deadline is a usage error.
    let (code, _, err) = env.json(&["cheapest", "3h", "--until", "21:00"]).await;
    assert_eq!((code, &err["error"]["code"]), (2, &json!("usage")));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_25_hour_day_keeps_both_0200_hours() {
    let env = Env::new("dst").await;
    let hours: Vec<f64> = (0..25).map(|h| 100.0 + h as f64).collect();
    env.publish(
        "2026-10-25",
        envelope(values("2026-10-25", 3600, |i| hours[i as usize]), vec![]),
    )
    .await;

    let (code, doc, _) = env.json(&["prices", "2026-10-25"]).await;
    assert_eq!(code, 0);
    let points = doc["points"].as_array().unwrap();
    assert_eq!((points.len(), &doc["complete"]), (25, &json!(true)));
    assert_eq!(points[2]["start"], "2026-10-25T02:00:00+02:00");
    assert_eq!(points[3]["start"], "2026-10-25T02:00:00+01:00");
    assert_eq!(points[3]["eur_per_mwh"], 103.0);
    assert_eq!(points[24]["end"], "2026-10-26T00:00:00+01:00");

    let human = String::from_utf8(env.run(&["prices", "2026-10-25"]).await.stdout).unwrap();
    assert!(
        human.contains("02:00 +02:00") && human.contains("02:00 +01:00"),
        "{human}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cached_days_work_offline_and_failures_map_to_exit_4() {
    let env = Env::new("offline").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    assert_eq!(env.json(&["prices"]).await.0, 0);

    // Point the tool at a port nothing listens on: one the system just handed out
    // and we gave straight back.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead = format!("http://127.0.0.1:{port}");
    let offline = |args: &[&str]| {
        let mut cmd = env.command(args);
        cmd.env("VATIO_API_URL", &dead);
        cmd.output().unwrap()
    };
    let out = offline(&["now", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "today is cached, so `now` needs no network"
    );
    let out = offline(&["prices", "2026-11-20", "--json"]);
    assert_eq!(out.status.code(), Some(4));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(
        (&err["error"]["code"], &err["error"]["retryable"]),
        (&json!("network"), &json!(true))
    );

    // cheapest still answers from today when tomorrow cannot be checked, and says so.
    let out = offline(&["cheapest", "2h", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        doc["warnings"][0]
            .as_str()
            .unwrap()
            .starts_with("could not check prices for 2026-11-27")
    );
}

const BOT_WALL: &str = "<html><head><META NAME=\"robots\" CONTENT=\"noindex,nofollow\"></head>\
<body>Request unsuccessful. Incapsula incident ID: 0-0</body></html>";

#[tokio::test(flavor = "multi_thread")]
async fn a_request_refused_by_the_bot_wall_is_retried() {
    let env = Env::new("wall-once").await;
    // The wall turns away the first attempt, then lets the same request through.
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(ResponseTemplate::new(403).set_body_string(BOT_WALL))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&env.server)
        .await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;

    let (code, doc, _) = env.json(&["prices"]).await;
    assert_eq!((code, doc["points"].as_array().unwrap().len()), (0, 24));
    assert_eq!(env.requests().await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_persistent_bot_wall_is_named_for_what_it_is() {
    let env = Env::new("wall").await;
    // Incapsula sometimes serves its page with a 200, so the body decides, not the status.
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(BOT_WALL))
        .mount(&env.server)
        .await;

    let (code, out, err) = env.json(&["prices"]).await;
    assert_eq!((code, &out), (4, &Value::Null));
    assert_eq!(err["error"]["code"], "blocked");
    assert_eq!(err["error"]["retryable"], true);
    assert_eq!(
        err["error"]["message"],
        "Red Eléctrica's bot protection refused the request (HTTP 200)"
    );
    assert_eq!(env.requests().await, 3, "three attempts, then give up");
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_errors_on_past_days_are_reported_as_upstream() {
    let env = Env::new("upstream").await;
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"errors": [{
            "code": 500, "status": "500", "title": "Error", "detail": "Something went wrong upstream."
        }]})))
        .mount(&env.server)
        .await;

    let (code, _, err) = env.json(&["prices", "2026-11-20"]).await;
    assert_eq!(code, 4);
    assert_eq!(err["error"]["code"], "upstream");
    assert_eq!(
        err["error"]["message"],
        "REData error 500: Something went wrong upstream."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn human_output_marks_now_and_carries_no_escape_codes_when_piped() {
    let env = Env::new("human").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.unpublished("2026-11-27").await;

    let prices = String::from_utf8(env.run(&["prices"]).await.stdout).unwrap();
    assert!(
        prices.starts_with("PVPC · Thu 26 Nov 2026 · €/kWh, energy term before taxes\n"),
        "{prices}"
    );
    assert!(
        prices.contains("▸ 19:00    0.28000  ██████████████████████  ← now\n"),
        "{prices}"
    );
    assert!(
        prices.contains("min 0.10000 at 02:00  ·  mean 0.16771  ·  max 0.28000 at 19:00"),
        "{prices}"
    );
    assert!(
        prices.ends_with("  Source: Red Eléctrica de España (REData) · updated Wed 25 Nov 20:46\n"),
        "{prices}"
    );
    assert!(
        !prices.contains('\u{1b}'),
        "no ANSI escapes when stdout is not a terminal"
    );

    let now = String::from_utf8(env.run(&["now"]).await.stdout).unwrap();
    assert!(
        now.contains("0.28000 €/kWh  high for today  (24 of 24, 1 = cheapest; until 20:00)"),
        "{now}"
    );

    let cheapest = String::from_utf8(env.run(&["cheapest", "2h"]).await.stdout).unwrap();
    assert!(
        cheapest.contains("★ Thu 26 Nov 22:00–24:00   0.15500 €/kWh   starts in 2h20m"),
        "{cheapest}"
    );
    assert!(
        cheapest.contains("Starting now (19:40–21:40) would average 0.25000 €/kWh."),
        "{cheapest}"
    );
    assert!(
        cheapest.contains("Waiting saves 0.09500 €/kWh (38.0%)."),
        "{cheapest}"
    );
    assert!(
        cheapest.contains("Tomorrow's prices are not published yet"),
        "{cheapest}"
    );
}

const DEMAND: &str = "demanda/demanda-tiempo-real";
const MIX: &str = "generacion/estructura-generacion";
const CO2: &str = "generacion/no-renovables-detalle-emisiones-CO2";

/// 20:15 in Madrid, just after the latest reading in the demand fixture.
const GRID_NOW: &str = "2026-11-26T19:15:00Z";

/// The answers the live API gave on 2026-11-26, byte for byte, for the four days the
/// grid report asks about. Two things in them broke the first live run: the emissions
/// answer carries a number where a timestamp belongs, and its factor series is out of
/// step with its days.
#[tokio::test(flavor = "multi_thread")]
async fn grid_reads_the_answers_the_live_api_gave() {
    let mut env = Env::new("grid-live").await;
    env.now = GRID_NOW;
    env.serve_fixture(
        DEMAND,
        200,
        fixture("redata_demand_realtime_2026-11-26T2000_2030.json"),
    )
    .await;
    env.serve_fixture(
        MIX,
        200,
        fixture("redata_generation_structure_day_2026-11-22_25.json"),
    )
    .await;
    env.serve_fixture(
        CO2,
        200,
        fixture("redata_co2_emissions_day_2026-11-22_25.json"),
    )
    .await;

    let (code, doc, _) = env.json(&["grid"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["warnings"], json!([]));
    let generation = &doc["generation"];
    assert_eq!(generation["day"], "2026-11-25");
    assert_eq!(generation["total_mwh"], 892181.3);
    assert_eq!(generation["renewable_percent"], 66.3);
    assert_eq!(generation["co2_tonnes"], 67445.0);
    assert_eq!(
        generation["co2_g_per_kwh"], 75.6,
        "67,445 t over 892,181 MWh, not the 64.3 of the factor series"
    );
    assert_eq!(generation["technologies"][0]["id"], "wind");
    assert_eq!(generation["technologies"][0]["percent"], 43.7);

    let human = String::from_utf8(env.run(&["grid"]).await.stdout).unwrap();
    assert!(
        human.contains("66.3% renewable  ·  76 g CO₂/kWh"),
        "{human}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn grid_combines_live_demand_with_the_latest_daily_mix() {
    let mut env = Env::new("grid").await;
    env.now = GRID_NOW;
    env.serve_fixture(
        DEMAND,
        200,
        fixture("redata_demand_realtime_2026-11-26T2000_2030.json"),
    )
    .await;
    env.serve_fixture(
        MIX,
        200,
        fixture("redata_generation_structure_day_2026-11-24.json"),
    )
    .await;
    env.serve_fixture(
        CO2,
        200,
        fixture("redata_co2_emissions_day_2026-11-22_25.json"),
    )
    .await;

    let (code, doc, _) = env.json(&["grid"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["schema"], "vatio.grid/v1");
    assert_eq!(doc["area"], "spain");
    assert_eq!(doc["at"], "2026-11-26T20:15:00+01:00");
    assert_eq!(
        doc["demand"],
        json!({
            "at": "2026-11-26T20:05:00+01:00", "actual_mw": 38261.0, "forecast_mw": 38792.0,
            "scheduled_mw": 38662.0, "resolution_minutes": 5,
            "peak_today": {"at": "2026-11-26T20:00:00+01:00", "mw": 38388.0},
            "low_today": {"at": "2026-11-26T20:05:00+01:00", "mw": 38261.0}
        })
    );
    let generation = &doc["generation"];
    assert_eq!(
        generation["day"], "2026-11-24",
        "the mix is the latest published day, not today"
    );
    assert_eq!(generation["total_mwh"], 808145.3);
    assert_eq!(generation["renewable_percent"], 55.7);
    assert_eq!(generation["co2_g_per_kwh"], 111.0);
    assert_eq!(generation["co2_tonnes"], 89721.2);
    assert_eq!(
        generation["technologies"][0],
        json!({"id": "wind", "name": "Wind", "name_es": "Eólica", "renewable": true, "mwh": 257724.1, "percent": 31.9})
    );
    assert_eq!(doc["warnings"], json!([]));
    let cited: Vec<(&str, &str)> = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["dataset"].as_str().unwrap(),
                s["updated_at"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        cited,
        [
            ("demand", "2026-11-26T20:12:05+01:00"),
            ("generation", "2026-11-26T08:26:36+01:00"),
            ("emissions", "2026-11-26T08:26:36+01:00"),
        ]
    );

    // Daily figures are asked for completed days only, looking back far enough to
    // find the latest one published.
    let requests = env.server.received_requests().await.unwrap();
    let mix = requests
        .iter()
        .find(|r| r.url.path().ends_with(MIX))
        .unwrap();
    assert_eq!(
        mix.url.query(),
        Some("start_date=2026-11-22T00:00&end_date=2026-11-25T23:59&time_trunc=day")
    );
    let demand = requests
        .iter()
        .find(|r| r.url.path().ends_with(DEMAND))
        .unwrap();
    assert_eq!(
        demand.url.query(),
        Some("start_date=2026-11-26T00:00&end_date=2026-11-26T23:59&time_trunc=hour")
    );

    let human = String::from_utf8(env.run(&["grid"]).await.stdout).unwrap();
    assert!(
        human.starts_with("Spain's grid · Thu 26 Nov 20:15\n"),
        "{human}"
    );
    assert!(
        human.contains("Demand  38,261 MW at 20:05  ·  forecast 38,792  ·  scheduled 38,662"),
        "{human}"
    );
    assert!(
        human
            .contains("Generation on Tue 24 Nov (latest daily figures, provisional)  ·  808.1 GWh"),
        "{human}"
    );
    assert!(
        human.contains("55.7% renewable  ·  111 g CO₂/kWh"),
        "{human}"
    );
    assert!(
        human.contains("    Wind                   257.7 GWh   31.9%  ██████████████████████\n"),
        "{human}"
    );
    assert!(
        human.ends_with(
            "  Source: Red Eléctrica de España (REData)\n  Updated: demand Thu 26 Nov 20:12 · \
             generation Thu 26 Nov 08:26 · emissions Thu 26 Nov 08:26\n"
        ),
        "{human}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn grid_degrades_part_by_part_and_fails_only_when_nothing_is_left() {
    let mut env = Env::new("grid-partial").await;
    env.now = GRID_NOW;
    let broken = json!({"errors": [{"code": 500, "status": "500", "title": "Error", "detail": "Internal error."}]});
    env.serve_fixture(
        DEMAND,
        200,
        fixture("redata_demand_realtime_2026-11-26T2000_2030.json"),
    )
    .await;
    env.serve_fixture(
        MIX,
        200,
        fixture("redata_generation_structure_day_2026-11-24.json"),
    )
    .await;
    env.serve_fixture(CO2, 500, broken.clone()).await;

    let (code, doc, _) = env.json(&["grid"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["generation"]["co2_g_per_kwh"], Value::Null);
    assert_eq!(doc["generation"]["renewable_percent"], 55.7);
    assert_eq!(
        doc["warnings"],
        json!(["emissions figures are unavailable: REData error 500: Internal error."])
    );
    assert_eq!(doc["sources"].as_array().unwrap().len(), 2);

    let mut env = Env::new("grid-down").await;
    env.now = GRID_NOW;
    for widget in [DEMAND, MIX, CO2] {
        env.serve_fixture(widget, 500, broken.clone()).await;
    }
    let (code, out, err) = env.json(&["grid"]).await;
    assert_eq!(
        (code, &out, &err["error"]["code"]),
        (4, &Value::Null, &json!("upstream"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn real_clock_change_answer_keeps_both_0200_hours_apart() {
    // Transcribed from the live API for the night of 25 October 2026: REData labels the
    // repeated hour 02:00+02:00 and then 02:00+01:00.
    let env = Env::new("dst-real").await;
    env.publish(
        "2026-10-25",
        fixture("redata_prices_dst_2026-10-25T0100_0359.json"),
    )
    .await;

    let (code, doc, _) = env.json(&["prices", "2026-10-25"]).await;
    assert_eq!(code, 0);
    let starts: Vec<&str> = doc["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["start"].as_str().unwrap())
        .collect();
    assert_eq!(
        starts,
        [
            "2026-10-25T01:00:00+02:00",
            "2026-10-25T02:00:00+02:00",
            "2026-10-25T02:00:00+01:00",
            "2026-10-25T03:00:00+01:00"
        ]
    );
    assert_eq!(doc["points"][2]["eur_per_mwh"], 212.01);
    assert_eq!(
        doc["complete"], false,
        "the capture is an excerpt of the day"
    );

    let (_, spot, _) = env.json(&["prices", "2026-10-25", "-m", "spot"]).await;
    assert_eq!(
        (
            spot["points"].as_array().unwrap().len(),
            &spot["resolution_minutes"]
        ),
        (16, &json!(15))
    );
    assert_eq!(spot["points"][8]["start"], "2026-10-25T02:00:00+01:00");
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_errors_are_exit_2_and_json_when_asked() {
    let env = Env::new("usage").await;
    for args in [
        &["prices", "26/11/2026"][..],
        &["cheapest", "soon"],
        &["cheapest", "3h", "--until", "teatime"],
        &["cheapest"],
        &["frobnicate"],
    ] {
        let (code, out, err) = env.json(args).await;
        assert_eq!(
            (code, &out, &err["error"]["code"]),
            (2, &Value::Null, &json!("usage")),
            "{args:?}"
        );
        assert_eq!(err["error"]["retryable"], false);
    }
    assert_eq!(
        env.requests().await,
        0,
        "usage errors never reach the network"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_command_documents_every_output() {
    let env = Env::new("schema").await;
    let out = env.run(&["schema"]).await;
    let all: Value = serde_json::from_slice(&out.stdout).unwrap();
    for name in ["prices", "now", "cheapest", "outlook", "grid", "error"] {
        assert_eq!(all[name]["type"], "object", "{name}");
    }
    assert!(
        all["prices"]["required"]
            .as_array()
            .unwrap()
            .contains(&json!("schema"))
    );
    let one: Value =
        serde_json::from_slice(&env.run(&["schema", "cheapest"]).await.stdout).unwrap();
    assert_eq!(one, all["cheapest"]);
}

/// The committed schemas under `schema/` are the published contract. A change to an
/// output type must show up there, in review. Regenerate with:
/// `for n in prices now cheapest outlook grid error; do vatio schema $n > schema/$n.json; done`
#[tokio::test(flavor = "multi_thread")]
async fn committed_schemas_match_the_code() {
    let env = Env::new("schema-files").await;
    for name in ["prices", "now", "cheapest", "outlook", "grid", "error"] {
        let path = format!("{}/schema/{name}.json", env!("CARGO_MANIFEST_DIR"));
        let committed: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let current: Value =
            serde_json::from_slice(&env.run(&["schema", name]).await.stdout).unwrap();
        assert_eq!(current, committed, "schema/{name}.json is out of date");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_server_exposes_the_same_documents_as_the_cli() {
    let env = Env::new("mcp").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.unpublished("2026-11-27").await;

    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": "find_cheapest_window", "arguments": {"duration_minutes": 120}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
            "name": "get_prices", "arguments": {"day": "tomorrow"}}}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
            "name": "get_current_price", "arguments": {}}}),
    ];
    let mut cmd = env.command(&["mcp"]);
    let replies = tokio::task::spawn_blocking(move || {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for request in &requests {
            writeln!(stdin, "{request}").unwrap();
        }
        // Read replies on a helper thread so a silent server fails the test instead of hanging it.
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if tx
                    .send(serde_json::from_str::<Value>(&line).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut replies = Vec::new();
        while replies.len() < 5 {
            replies.push(
                rx.recv_timeout(Duration::from_secs(20))
                    .expect("MCP server did not answer in time"),
            );
        }
        drop(stdin); // closing stdin ends the session
        child.wait().unwrap();
        replies
    })
    .await
    .unwrap();
    let reply = |id: i64| {
        replies
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("no reply {id}"))
    };

    assert_eq!(reply(1)["result"]["serverInfo"]["name"], "vatio");
    assert!(
        reply(1)["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("Europe/Madrid")
    );

    let tools = reply(2)["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "find_cheapest_window",
            "get_current_price",
            "get_grid_status",
            "get_price_outlook",
            "get_prices"
        ]
    );
    for tool in tools {
        assert_eq!(
            tool["annotations"]["readOnlyHint"], true,
            "{}",
            tool["name"]
        );
        assert_eq!(tool["outputSchema"]["type"], "object", "{}", tool["name"]);
    }

    // Same document as `vatio cheapest 2h --json`.
    let (_, cli, _) = env.json(&["cheapest", "2h"]).await;
    let structured = &reply(3)["result"]["structuredContent"];
    assert_eq!(structured["best"], cli["best"]);
    assert_eq!(structured["schema"], "vatio.cheapest/v1");
    assert_eq!(reply(3)["result"]["isError"], false);

    // Failures are flagged and carry the same error object.
    assert_eq!(reply(4)["result"]["isError"], true);
    let error: Value =
        serde_json::from_str(reply(4)["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (&error["error"]["code"], &error["error"]["retryable"]),
        (&json!("not_available"), &json!(true))
    );

    assert_eq!(
        reply(5)["result"]["structuredContent"]["current"]["eur_per_kwh"],
        0.28
    );
}

// ---------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_later_start_does_not_pretend_to_be_now() {
    let env = Env::new("from").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.publish("2026-11-27", day_body("2026-11-27", &FRIDAY))
        .await;

    // The clock says 19:40; the run may not start before 22:00.
    let (code, doc, _) = env.json(&["cheapest", "4h", "--from", "22:00"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["searched"]["from"], "2026-11-26T22:00:00+01:00");
    // Friday 01:00-05:00 = (70 + 60 + 65 + 80) / 4, and it is 5h20m away from the clock.
    assert_eq!(doc["best"]["start"], "2026-11-27T01:00:00+01:00");
    assert_eq!(doc["best"]["mean_eur_per_mwh"], 68.75);
    assert_eq!(doc["best"]["starts_in_minutes"], 320);
    // The comparison is with the earliest allowed start, 22:00, which is 2h20m away.
    assert_eq!(doc["earliest"]["start"], "2026-11-26T22:00:00+01:00");
    assert_eq!(doc["earliest"]["starts_in_minutes"], 140);
    assert_eq!(doc["earliest"]["mean_eur_per_mwh"], 117.5);
    assert_eq!(doc["saving_eur_per_kwh"], 0.04875);
    assert_eq!(doc["saving_percent"], 41.5);

    let human =
        String::from_utf8(env.run(&["cheapest", "4h", "--from", "22:00"]).await.stdout).unwrap();
    assert!(
        human.contains("★ Fri 27 Nov 01:00–05:00   0.06875 €/kWh   starts in 5h20m"),
        "{human}"
    );
    assert!(
        human.contains("Starting at 22:00 (22:00–02:00) would average 0.11750 €/kWh."),
        "{human}"
    );
    assert!(
        human.contains("The best window saves 0.04875 €/kWh (41.5%)."),
        "{human}"
    );
    assert!(
        !human.contains("Starting now") && !human.contains("Waiting saves"),
        "{human}"
    );

    // A start in the past is refused rather than answered with a window that is gone.
    let (code, _, err) = env
        .json(&["cheapest", "2h", "--from", "2026-11-26T00:30"])
        .await;
    assert_eq!((code, &err["error"]["code"]), (2, &json!("usage")));
    assert_eq!(
        env.requests().await,
        2,
        "refused before any further request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_midnight_deadline_is_not_mistaken_for_unpublished_prices() {
    let env = Env::new("midnight").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.publish("2026-11-27", day_body("2026-11-27", &FRIDAY))
        .await;

    let (code, doc, _) = env.json(&["cheapest", "1h", "--until", "24:00"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["searched"]["until"], "2026-11-27T00:00:00+01:00");
    assert_eq!(doc["searched"]["tomorrow"], "not_needed");
    assert_eq!(doc["best"]["start"], "2026-11-26T23:00:00+01:00");
    let human = String::from_utf8(
        env.run(&["cheapest", "1h", "--until", "24:00"])
            .await
            .stdout,
    )
    .unwrap();
    assert!(!human.contains("not published yet"), "{human}");
    assert_eq!(env.requests().await, 1, "tomorrow is not even asked for");

    // Without a deadline the same search does reach into tomorrow.
    let (_, doc, _) = env.json(&["cheapest", "1h"]).await;
    assert_eq!(doc["searched"]["tomorrow"], "included");
}

#[tokio::test(flavor = "multi_thread")]
async fn unpublished_tomorrow_is_reported_in_the_search() {
    let env = Env::new("tomorrow-state").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.unpublished("2026-11-27").await;
    let (_, doc, _) = env.json(&["cheapest", "1h"]).await;
    assert_eq!(doc["searched"]["tomorrow"], "not_published");
    assert_eq!(doc["searched"]["until"], "2026-11-27T00:00:00+01:00");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_repeated_hour_is_handled_by_now_and_cheapest() {
    let mut env = Env::new("dst-now").await;
    // Hour index 2 is 02:00+02:00 and index 3 is 02:00+01:00; price = 100 + index.
    let hours: Vec<f64> = (0..25).map(|h| 100.0 + h as f64).collect();
    env.publish(
        "2026-10-25",
        envelope(values("2026-10-25", 3600, |i| hours[i as usize]), vec![]),
    )
    .await;
    env.unpublished("2026-10-26").await;

    env.now = "2026-10-25T00:30:00Z"; // 02:30 summer time: the first 02:00 hour
    let (_, doc, _) = env.json(&["now"]).await;
    assert_eq!(doc["current"]["start"], "2026-10-25T02:00:00+02:00");
    assert_eq!(doc["current"]["eur_per_mwh"], 102.0);
    assert_eq!(doc["next"]["start"], "2026-10-25T02:00:00+01:00");
    assert_eq!(doc["rank_of"], 25);

    // Three real hours from 02:30 summer time end at 04:30 winter time.
    let (_, doc, _) = env.json(&["cheapest", "3h"]).await;
    assert_eq!(doc["earliest"]["start"], "2026-10-25T02:30:00+02:00");
    assert_eq!(doc["earliest"]["end"], "2026-10-25T04:30:00+01:00");
    // 30 min at 102, 60 at 103, 60 at 104, 30 at 105.
    assert_eq!(doc["earliest"]["mean_eur_per_mwh"], 103.5);

    env.now = "2026-10-25T01:30:00Z"; // 02:30 winter time: the second 02:00 hour
    let (_, doc, _) = env.json(&["now"]).await;
    assert_eq!(doc["current"]["start"], "2026-10-25T02:00:00+01:00");
    assert_eq!(doc["current"]["eur_per_mwh"], 103.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_23_hour_day_has_no_0200_hour() {
    let env = Env::new("spring").await;
    env.publish(
        "2026-03-29",
        envelope(values("2026-03-29", 3600, |i| 50.0 + i as f64), vec![]),
    )
    .await;

    let (code, doc, _) = env.json(&["prices", "2026-03-29"]).await;
    assert_eq!(code, 0);
    let points = doc["points"].as_array().unwrap();
    assert_eq!((points.len(), &doc["complete"]), (23, &json!(true)));
    assert_eq!(points[1]["start"], "2026-03-29T01:00:00+01:00");
    assert_eq!(points[2]["start"], "2026-03-29T03:00:00+02:00");
    assert_eq!(points[22]["end"], "2026-03-30T00:00:00+02:00");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_series_with_holes_is_incomplete_and_never_cached() {
    let env = Env::new("sparse").await;
    // A half-loaded publication: only the even hours have arrived.
    let mut pvpc = values("2026-11-26", 3600, |i| 100.0 + i as f64);
    let mut keep = [true, false].into_iter().cycle();
    pvpc.retain(|_| keep.next().unwrap());
    env.publish("2026-11-26", envelope(pvpc, vec![])).await;

    let (code, doc, _) = env.json(&["prices"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["points"].as_array().unwrap().len(), 12);
    assert_eq!(
        (&doc["complete"], &doc["resolution_minutes"]),
        (&json!(false), &json!(60))
    );
    assert_eq!(doc["points"][1]["start"], "2026-11-26T02:00:00+01:00");
    assert_eq!(doc["points"][1]["end"], "2026-11-26T03:00:00+01:00");
    // 19:40 falls in a missing hour: there is no price to report for now.
    let (code, _, err) = env.json(&["now"]).await;
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_available")));
    assert_eq!(
        env.requests().await,
        2,
        "an incomplete day is fetched again, not cached"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn days_still_ahead_are_rechecked_after_an_hour_and_ended_days_are_not() {
    let mut env = Env::new("revalidate").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.publish("2026-11-20", day_body("2026-11-20", &FRIDAY))
        .await;

    assert_eq!(env.json(&["prices"]).await.1["sources"][0]["cached"], false);
    assert_eq!(
        env.json(&["prices", "2026-11-20"]).await.1["sources"][0]["cached"],
        false
    );
    env.now = "2026-11-26T19:10:00Z"; // half an hour later
    assert_eq!(env.json(&["prices"]).await.1["sources"][0]["cached"], true);
    assert_eq!(env.requests().await, 2);

    env.now = "2026-11-26T20:45:00Z"; // more than an hour after the first fetch
    let (_, doc, _) = env.json(&["prices"]).await;
    assert_eq!(
        doc["sources"][0]["cached"], false,
        "today may have been corrected: checked again"
    );
    let (_, doc, _) = env.json(&["prices", "2026-11-20"]).await;
    assert_eq!(
        doc["sources"][0]["cached"], true,
        "a day that is over is kept as it is"
    );
    assert_eq!(env.requests().await, 3);

    // If that check cannot get through, the copy already held is used.
    env.server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string(BOT_WALL))
        .mount(&env.server)
        .await;
    env.now = "2026-11-26T21:50:00Z"; // 65 minutes after the last fetch
    let before = env.requests().await;
    let (code, doc, _) = env.json(&["now"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["sources"][0]["cached"], true);
    assert_eq!(doc["sources"][0]["fetched_at"], "2026-11-26T21:45:00+01:00");
    assert_eq!(
        env.requests().await - before,
        1,
        "a check gets one attempt, not the full retry budget"
    );

    // Likewise if the check comes back with less than is already held.
    env.server.reset().await;
    let mut partial = values("2026-11-26", 3600, |i| THURSDAY[i as usize]);
    partial.truncate(20);
    env.publish("2026-11-26", envelope(partial, vec![])).await;
    let (_, doc, _) = env.json(&["prices"]).await;
    assert_eq!(
        (&doc["complete"], doc["points"].as_array().unwrap().len()),
        (&json!(true), 24)
    );
    assert_eq!(doc["sources"][0]["cached"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_server_never_writes_into_the_default_cache() {
    let env = Env::new("foreign").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    let home = env.cache.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let mut cmd = env.command(&["prices", "--json"]);
    cmd.env_remove("VATIO_CACHE_DIR")
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("LOCALAPPDATA", home.join("AppData"));
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        std::fs::read_dir(&home).unwrap().count(),
        0,
        "nothing was written under the home directory"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_for_tomorrow_are_not_all_called_unpublished() {
    // REData's answer for a range it holds nothing for means "not published yet" when
    // the day is still to come. Any other error for tomorrow is what it says it is.
    let env = Env::new("tomorrow-errors").await;
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(ResponseTemplate::new(502).set_body_json(no_data_body()))
        .mount(&env.server)
        .await;
    let (code, _, err) = env.json(&["prices", "tomorrow"]).await;
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_available")));
    assert_eq!(err["error"]["retryable"], true);
    assert_eq!(
        env.requests().await,
        1,
        "a known answer is not asked for again"
    );

    // Today's prices were published yesterday, so there the same answer is a failure
    // on REData's side, to be tried again. An old day simply has no data.
    let (code, _, err) = env.json(&["prices", "today"]).await;
    assert_eq!((code, &err["error"]["code"]), (4, &json!("upstream")));
    assert_eq!(err["error"]["retryable"], true);
    let (code, _, err) = env.json(&["prices", "2001-01-01"]).await;
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_available")));
    assert_eq!(err["error"]["retryable"], false);

    // The same words under HTTP 400 are a rejected request, not an unpublished day.
    let env = Env::new("tomorrow-errors-400").await;
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(fixture("redata_error_bad_request_400.json")),
        )
        .mount(&env.server)
        .await;
    let (code, _, err) = env.json(&["prices", "tomorrow"]).await;
    assert_eq!((code, &err["error"]["code"]), (4, &json!("upstream")));
    assert_eq!(err["error"]["retryable"], false);

    let env = Env::new("tomorrow-errors-2").await;
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"errors": [{
            "code": 400, "status": "400", "title": "Bad request", "detail": "Error in start date."
        }]})))
        .mount(&env.server)
        .await;
    let (code, _, err) = env.json(&["prices", "tomorrow"]).await;
    assert_eq!(code, 4);
    assert_eq!(err["error"]["code"], "upstream");
    assert_eq!(
        err["error"]["message"],
        "REData error 400: Error in start date."
    );
    assert_eq!(err["error"]["retryable"], false);

    // Nothing can be published beyond tomorrow, so that is answered without asking.
    let before = env.requests().await;
    let (code, _, err) = env.json(&["prices", "2026-11-29"]).await;
    assert_eq!((code, &err["error"]["code"]), (3, &json!("not_available")));
    assert_eq!(env.requests().await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn grid_refused_by_the_bot_wall_says_blocked() {
    let mut env = Env::new("grid-wall").await;
    env.now = GRID_NOW;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string(BOT_WALL))
        .mount(&env.server)
        .await;
    let (code, out, err) = env.json(&["grid"]).await;
    assert_eq!((code, &out), (4, &Value::Null));
    assert_eq!(
        (&err["error"]["code"], &err["error"]["retryable"]),
        (&json!("blocked"), &json!(true))
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_argument_that_is_not_unicode_is_a_usage_error() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new("bytes").await;
    let mut cmd = env.command(&["prices", "--json"]);
    cmd.arg(std::ffi::OsStr::from_bytes(b"\xff"));
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["code"], "usage");
}

#[tokio::test(flavor = "multi_thread")]
async fn durations_must_be_whole_minutes() {
    let env = Env::new("durations").await;
    for bad in ["PT90S", "1.01", "90", "0"] {
        let (code, _, err) = env.json(&["cheapest", bad]).await;
        assert_eq!((code, &err["error"]["code"]), (2, &json!("usage")), "{bad}");
    }
    let (_, _, err) = env.json(&["cheapest", "90"]).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("a bare number means hours")
    );
}

/// Send JSON-RPC requests to `vatio mcp` and collect one reply per request that has an id.
async fn mcp_session(env: &Env, requests: Vec<Value>) -> Vec<Value> {
    let expected = requests.iter().filter(|r| r.get("id").is_some()).count();
    let mut cmd = env.command(&["mcp"]);
    tokio::task::spawn_blocking(move || {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for request in &requests {
            writeln!(stdin, "{request}").unwrap();
        }
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if tx
                    .send(serde_json::from_str::<Value>(&line).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut replies = Vec::new();
        while replies.len() < expected {
            replies.push(
                rx.recv_timeout(Duration::from_secs(20))
                    .expect("MCP server did not answer in time"),
            );
        }
        drop(stdin);
        assert!(child.wait().unwrap().success());
        replies
    })
    .await
    .unwrap()
}

fn mcp_initialize() -> [Value; 2] {
    [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_declares_the_published_schemas_and_rejects_bad_arguments() {
    let env = Env::new("mcp-contract").await;
    let call = |id: i64, name: &str, arguments: Value| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}});
    let mut requests = mcp_initialize().to_vec();
    requests.push(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    requests.push(call(3, "get_prices", json!({"markte": "spot"})));
    requests.push(call(
        4,
        "find_cheapest_window",
        json!({"duration_minutes": 60, "top": 99}),
    ));
    requests.push(call(
        5,
        "find_cheapest_window",
        json!({"duration_minutes": 0}),
    ));
    let replies = mcp_session(&env, requests).await;
    let reply = |id: i64| replies.iter().find(|r| r["id"] == id).unwrap();

    // Each tool declares exactly the schema `vatio schema` prints for its document.
    for (tool, document) in [
        ("get_prices", "prices"),
        ("get_current_price", "now"),
        ("find_cheapest_window", "cheapest"),
        ("get_price_outlook", "outlook"),
        ("get_grid_status", "grid"),
    ] {
        let tools = reply(2)["result"]["tools"].as_array().unwrap();
        let declared = &tools.iter().find(|t| t["name"] == tool).unwrap()["outputSchema"];
        let mut published: Value =
            serde_json::from_slice(&env.run(&["schema", document]).await.stdout).unwrap();
        // The tool listing carries the description on the tool, not on the schema.
        let root = published.as_object_mut().unwrap();
        root.remove("title");
        root.remove("description");
        assert_eq!(declared, &published, "{tool}");
    }

    // A misspelt argument is an error, not a silent default.
    let misspelt = reply(3);
    let refused = misspelt.get("error").is_some() || misspelt["result"]["isError"] == true;
    assert!(refused, "{misspelt}");
    // Out-of-range values are refused rather than quietly adjusted.
    for id in [4, 5] {
        let r = reply(id);
        assert!(
            r.get("error").is_some() || r["result"]["isError"] == true,
            "{r}"
        );
    }
    assert_eq!(env.requests().await, 0);
}

// ---------------------------------------------------------------------------
// Outlook: published days plus vatio's estimate for the days after them
// ---------------------------------------------------------------------------

/// 22:30 in Madrid on Thursday 26 November 2026: Friday's prices are out.
const OUTLOOK_NOW: &str = "2026-11-26T21:30:00Z";
const WEEKDAYS_TO_27: [&str; 4] = ["2026-11-27", "2026-11-26", "2026-11-25", "2026-11-24"];
const WEEKENDS_TO_22: [&str; 4] = ["2026-11-22", "2026-11-21", "2026-11-15", "2026-11-14"];

/// The real hourly PVPC prices for 12 to 27 November 2026, in REData's shape, served
/// whole for every request to the price widget that no other mock answers. The tool
/// has to pick the days it asked for out of the answer.
async fn serve_real_prices(env: &Env) {
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(fixture("redata_prices_pvpc_2026-11-12_27.json")),
        )
        .with_priority(10)
        .mount(&env.server)
        .await;
}

/// The queries of every request received so far, in order.
async fn queries(env: &Env) -> Vec<String> {
    let requests = env.server.received_requests().await.unwrap();
    requests
        .iter()
        .map(|r| r.url.query().unwrap_or_default().to_owned())
        .collect()
}

/// Check a price window against its clock times and a mean worked out independently
/// (in Python, from the rule's definition), allowing for rounding to the cent per MWh.
#[track_caller]
fn assert_window(window: &Value, start: &str, end: &str, eur_per_mwh: f64) {
    assert_eq!(
        (&window["start"], &window["end"]),
        (&json!(start), &json!(end))
    );
    let mwh = window["mean_eur_per_mwh"].as_f64().unwrap();
    let kwh = window["mean_eur_per_kwh"].as_f64().unwrap();
    assert!((mwh - eur_per_mwh).abs() < 0.0051, "{mwh} != {eur_per_mwh}");
    assert!((kwh - eur_per_mwh / 1000.0).abs() < 0.0000051, "{kwh}");
}

#[tokio::test(flavor = "multi_thread")]
async fn outlook_searches_published_and_estimated_days_on_real_prices() {
    let mut env = Env::new("outlook").await;
    env.now = OUTLOOK_NOW;
    serve_real_prices(&env).await;

    let (code, doc, err) = env.json(&["outlook", "3h"]).await;
    assert_eq!((code, &err), (0, &Value::Null));
    assert_eq!(doc["schema"], "vatio.outlook/v1");
    assert_eq!(
        (&doc["market"], &doc["area"], &doc["timezone"]),
        (&json!("pvpc"), &json!("peninsula"), &json!("Europe/Madrid"))
    );
    assert_eq!(doc["at"], "2026-11-26T22:30:00+01:00");
    assert_eq!(doc["duration_minutes"], 180);
    assert_eq!(
        doc["searched"],
        json!({"from": "2026-11-26T22:30:00+01:00", "until": "2026-12-03T00:00:00+01:00"})
    );
    assert_eq!(
        doc["method"],
        json!({"name": "same_kind_days", "days_averaged": 4})
    );
    assert_eq!(doc["warnings"], json!([]));

    // Day, kind, basis, the days averaged, and the cheapest three hours within the
    // day: its start, its end and its mean in €/MWh.
    let (weekdays, weekends, none) = (&WEEKDAYS_TO_27[..], &WEEKENDS_TO_22[..], &[][..]);
    let night = Some(("03:00", "06:00", 166.6467));
    let midday = Some(("12:00", "15:00", 79.695));
    let expected = [
        // 90 minutes are left of Thursday: no room for three hours.
        ("2026-11-26", "weekday", "published", none, None),
        (
            "2026-11-27",
            "weekday",
            "published",
            none,
            Some(("03:00", "06:00", 191.6967)),
        ),
        ("2026-11-28", "weekend", "estimated", weekends, midday),
        ("2026-11-29", "weekend", "estimated", weekends, midday),
        ("2026-11-30", "weekday", "estimated", weekdays, night),
        ("2026-12-01", "weekday", "estimated", weekdays, night),
        ("2026-12-02", "weekday", "estimated", weekdays, night),
    ];
    let days = doc["days"].as_array().unwrap();
    assert_eq!(days.len(), expected.len());
    for (got, (day, kind, basis, from, cheapest)) in days.iter().zip(expected) {
        assert_eq!(
            (&got["day"], &got["kind"], &got["basis"]),
            (&json!(day), &json!(kind), &json!(basis))
        );
        assert_eq!(got["estimated_from"], json!(from), "{day}");
        match cheapest {
            None => assert_eq!(got["cheapest"], Value::Null, "{day}"),
            Some((start, end, mean)) => assert_window(
                &got["cheapest"],
                &format!("{day}T{start}:00+01:00"),
                &format!("{day}T{end}:00+01:00"),
                mean,
            ),
        }
    }

    // Saturday and Sunday are estimated alike; the tie goes to the earlier one.
    let best = &doc["best"];
    assert_window(
        best,
        "2026-11-28T12:00:00+01:00",
        "2026-11-28T15:00:00+01:00",
        79.695,
    );
    assert_eq!(
        (&best["basis"], &best["starts_in_minutes"]),
        (&json!("estimated"), &json!(37 * 60 + 30))
    );

    // A cold cache costs three requests: today, tomorrow, and one for the whole
    // span of past days to estimate from, back to 16 days before today.
    assert_eq!(
        queries(&env).await,
        [
            "start_date=2026-11-26T00:00&end_date=2026-11-26T23:59&time_trunc=hour",
            "start_date=2026-11-27T00:00&end_date=2026-11-27T23:59&time_trunc=hour",
            "start_date=2026-11-10T00:00&end_date=2026-11-25T23:59&time_trunc=hour",
        ]
    );
    let sources = doc["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 3, "one per answer from Red Eléctrica");
    for source in sources {
        assert_eq!(
            (&source["dataset"], &source["cached"], &source["updated_at"]),
            (
                &json!("prices"),
                &json!(false),
                &json!("2026-11-26T20:46:05+01:00")
            )
        );
    }

    // After that every day comes from the cache, the past ones included.
    let (_, again, _) = env.json(&["outlook", "3h"]).await;
    assert_eq!(env.requests().await, 3, "the second run asks for nothing");
    assert_eq!(
        (&again["best"], &again["days"]),
        (&doc["best"], &doc["days"])
    );
    for source in again["sources"].as_array().unwrap() {
        assert_eq!(source["cached"], true);
    }
    // A past day stored by the outlook is an ordinary cached day for `prices`.
    let (_, past, _) = env.json(&["prices", "2026-11-14"]).await;
    assert_eq!(
        (&past["complete"], &past["sources"][0]["cached"]),
        (&json!(true), &json!(true))
    );
    assert_eq!(env.requests().await, 3);
    // --refresh goes back to the network for all of it, again in three requests.
    let (_, fresh, _) = env.json(&["outlook", "3h", "--refresh"]).await;
    assert_eq!(fresh["sources"][2]["cached"], false);
    assert_eq!(env.requests().await, 6);

    let human = String::from_utf8(env.run(&["outlook", "3h"]).await.stdout).unwrap();
    assert!(
        human.starts_with(
            "Outlook for 3h on PVPC · searched Thu 26 Nov 22:30 → Wed 2 Dec 24:00 · \
             €/kWh, energy term before taxes\n"
        ),
        "{human}"
    );
    for line in [
        "    Thu 26 Nov  published  no room left for a 3h run\n",
        "    Fri 27 Nov  published  03:00–06:00   0.19170 €/kWh\n",
        "  ★ Sat 28 Nov  estimated  12:00–15:00   0.079",
        "    Sun 29 Nov  estimated  12:00–15:00   0.079",
        "    Wed 2 Dec   estimated  03:00–06:00   0.16665 €/kWh\n",
        "  Best: Sat 28 Nov 12:00–15:00, estimated 0.079",
        " €/kWh, starts in 37h30m.\n",
        "  Estimated: the average of the last 4 days of the same kind (weekdays or weekend days). \
         It is vatio's own estimate, not Red Eléctrica's; published prices for a day arrive the \
         evening before.\n",
    ] {
        assert!(human.contains(line), "{line:?} is missing from:\n{human}");
    }
    assert!(
        human.ends_with("  Source: Red Eléctrica de España (REData) · updated Thu 26 Nov 20:46\n"),
        "{human}"
    );
    assert!(!human.contains('\u{1b}'), "{human}");
}

#[tokio::test(flavor = "multi_thread")]
async fn outlook_takes_any_duration_and_its_best_window_may_cross_midnight() {
    let mut env = Env::new("outlook-durations").await;
    env.now = OUTLOOK_NOW;
    serve_real_prices(&env).await;

    // One hour lands on the cheapest hour of each estimate, and still fits today.
    let (_, doc, _) = env.json(&["outlook", "1h"]).await;
    let cheapest = |i: usize| &doc["days"][i]["cheapest"];
    assert_window(
        cheapest(0),
        "2026-11-26T23:00:00+01:00",
        "2026-11-27T00:00:00+01:00",
        285.18,
    );
    assert_window(
        cheapest(2),
        "2026-11-28T13:00:00+01:00",
        "2026-11-28T14:00:00+01:00",
        76.3225,
    );
    assert_window(
        cheapest(4),
        "2026-11-30T14:00:00+01:00",
        "2026-11-30T15:00:00+01:00",
        142.8025,
    );

    // 90 minutes over hourly prices: the cheap hour and the cheaper half of a neighbour.
    let (_, doc, _) = env.json(&["outlook", "90m"]).await;
    assert_window(
        &doc["best"],
        "2026-11-28T12:30:00+01:00",
        "2026-11-28T14:00:00+01:00",
        77.7475,
    );
    assert_window(
        &doc["days"][0]["cheapest"],
        "2026-11-26T22:30:00+01:00",
        "2026-11-27T00:00:00+01:00",
        296.27,
    );

    // A whole day: within each day there is only the day itself, but the best 24
    // hours overall run from Sunday 03:00 to Monday 03:00.
    let (_, doc, _) = env.json(&["outlook", "24h"]).await;
    assert_eq!(doc["days"][0]["cheapest"], Value::Null);
    assert_window(
        &doc["days"][3]["cheapest"],
        "2026-11-29T00:00:00+01:00",
        "2026-11-30T00:00:00+01:00",
        165.2419,
    );
    assert_window(
        &doc["best"],
        "2026-11-29T03:00:00+01:00",
        "2026-11-30T03:00:00+01:00",
        164.9258,
    );
    assert_eq!(doc["best"]["basis"], "estimated");
    assert_eq!(env.requests().await, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn outlook_before_tomorrow_is_published_estimates_from_tomorrow_on() {
    // 19:40 on Thursday, an hour before Friday's prices come out.
    let env = Env::new("outlook-early").await;
    env.unpublished("2026-11-27").await;
    serve_real_prices(&env).await;

    let (code, doc, _) = env.json(&["outlook", "3h"]).await;
    assert_eq!(code, 0);
    assert_eq!(doc["at"], "2026-11-26T19:40:27+01:00");
    assert_eq!(
        doc["searched"],
        json!({"from": "2026-11-26T19:40:00+01:00", "until": "2026-12-02T00:00:00+01:00"})
    );
    assert_eq!(doc["warnings"], json!([]));
    let days = doc["days"].as_array().unwrap();
    let column =
        |key: &str| -> Vec<&str> { days.iter().map(|d| d[key].as_str().unwrap()).collect() };
    assert_eq!(
        column("day"),
        [
            "2026-11-26",
            "2026-11-27",
            "2026-11-28",
            "2026-11-29",
            "2026-11-30",
            "2026-12-01"
        ]
    );
    assert_eq!(
        column("basis"),
        [
            "published",
            "estimated",
            "estimated",
            "estimated",
            "estimated",
            "estimated"
        ]
    );
    assert_eq!(
        column("kind"),
        [
            "weekday", "weekday", "weekend", "weekend", "weekday", "weekday"
        ]
    );
    // The days averaged end today: Friday is not a source, Monday the 23rd is.
    let weekdays = json!(["2026-11-26", "2026-11-25", "2026-11-24", "2026-11-23"]);
    assert_eq!(days[0]["estimated_from"], json!([]));
    assert_eq!(days[1]["estimated_from"], weekdays);
    assert_eq!(days[2]["estimated_from"], json!(WEEKENDS_TO_22));
    assert_eq!(days[4]["estimated_from"], weekdays);

    // What is left of today is searched from the current minute.
    assert_window(
        &days[0]["cheapest"],
        "2026-11-26T21:00:00+01:00",
        "2026-11-27T00:00:00+01:00",
        332.6233,
    );
    assert_window(
        &days[1]["cheapest"],
        "2026-11-27T02:00:00+01:00",
        "2026-11-27T05:00:00+01:00",
        162.2142,
    );
    assert_window(
        &days[5]["cheapest"],
        "2026-12-01T02:00:00+01:00",
        "2026-12-01T05:00:00+01:00",
        162.2142,
    );
    assert_window(
        &doc["best"],
        "2026-11-28T12:00:00+01:00",
        "2026-11-28T15:00:00+01:00",
        79.695,
    );
    assert_eq!(doc["best"]["starts_in_minutes"], 40 * 60 + 20);
}

#[tokio::test(flavor = "multi_thread")]
async fn outlook_says_so_when_tomorrow_cannot_be_checked() {
    let mut env = Env::new("outlook-unchecked").await;
    env.now = OUTLOOK_NOW;
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .and(query_param("start_date", "2026-11-27T00:00"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"errors": [{
            "code": 500, "status": "500", "title": "Error", "detail": "Internal error."
        }]})))
        .mount(&env.server)
        .await;
    serve_real_prices(&env).await;

    let (code, doc, _) = env.json(&["outlook", "3h"]).await;
    assert_eq!(code, 0);
    assert_eq!(
        doc["warnings"],
        json!([
            "could not check prices for 2026-11-27, so that day is estimated: \
             REData error 500: Internal error."
        ])
    );
    assert_eq!(
        (&doc["days"][1]["day"], &doc["days"][1]["basis"]),
        (&json!("2026-11-27"), &json!("estimated"))
    );
    let human = String::from_utf8(env.run(&["outlook", "3h"]).await.stdout).unwrap();
    assert!(
        human.contains("  ! could not check prices for 2026-11-27"),
        "{human}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn outlook_needs_four_published_days_of_each_kind() {
    // Today and nine days before it: weekdays enough, but only one weekend.
    let env = Env::new("outlook-short").await;
    env.unpublished("2026-11-27").await;
    let history: Vec<Value> = (17..=26)
        .flat_map(|d| values(&format!("2026-11-{d}"), 3600, |i| THURSDAY[i as usize % 24]))
        .collect();
    env.serve_fixture(
        "mercados/precios-mercados-tiempo-real",
        200,
        envelope(history, vec![]),
    )
    .await;

    let (code, out, err) = env.json(&["outlook", "3h"]).await;
    assert_eq!(
        (code, &out, &err["error"]["code"]),
        (3, &Value::Null, &json!("not_available"))
    );
    assert_eq!(
        err["error"]["message"],
        "too little price history for an outlook: 4 published weekend days are needed to \
         estimate from, and only 2 were found from 2026-11-10 on"
    );
    let human = env.run(&["outlook", "3h"]).await;
    assert_eq!(human.status.code(), Some(3));
    assert!(human.stdout.is_empty());

    // No answer at all for days published long ago is REData failing, which is a
    // different thing from a short history.
    let env = Env::new("outlook-no-history").await;
    env.publish("2026-11-26", day_body("2026-11-26", &THURSDAY))
        .await;
    env.unpublished("2026-11-27").await;
    Mock::given(method("GET"))
        .and(path(PRICES_PATH))
        .respond_with(ResponseTemplate::new(502).set_body_json(no_data_body()))
        .with_priority(10)
        .mount(&env.server)
        .await;
    let (code, _, err) = env.json(&["outlook", "3h"]).await;
    assert_eq!(
        (code, &err["error"]["code"], &err["error"]["retryable"]),
        (4, &json!("upstream"), &json!(true))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn outlook_rejects_a_bad_duration_before_any_request() {
    let env = Env::new("outlook-usage").await;
    for args in [
        &["outlook", "soon"][..],
        &["outlook", "25h"],
        &["outlook", "PT90S"],
        &["outlook"],
        &["outlook", "3h", "--market", "spot"],
    ] {
        let (code, out, err) = env.json(args).await;
        assert_eq!(
            (code, &out, &err["error"]["code"]),
            (2, &Value::Null, &json!("usage")),
            "{args:?}"
        );
    }
    assert_eq!(env.requests().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_outlook_tool_returns_the_same_document_as_the_cli() {
    let mut env = Env::new("outlook-mcp").await;
    env.now = OUTLOOK_NOW;
    serve_real_prices(&env).await;
    let call = |id: i64, arguments: Value| json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": "get_price_outlook", "arguments": arguments}});

    // The command fills the cache, so that the tool and a second run of the command
    // then answer from the same state and must agree to the last field.
    assert_eq!(env.json(&["outlook", "3h"]).await.0, 0);
    let mut requests = mcp_initialize().to_vec();
    requests.push(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    requests.push(call(3, json!({"duration_minutes": 180})));
    requests.push(call(4, json!({"duration_minutes": 0})));
    requests.push(call(5, json!({"duration_minutes": 180, "market": "spot"})));
    let replies = mcp_session(&env, requests).await;
    let reply = |id: i64| replies.iter().find(|r| r["id"] == id).unwrap();
    let (_, cli, _) = env.json(&["outlook", "3h"]).await;

    assert_eq!(reply(3)["result"]["isError"], false);
    let structured = &reply(3)["result"]["structuredContent"];
    assert_eq!(structured["schema"], "vatio.outlook/v1");
    assert_eq!(structured["best"]["basis"], "estimated");
    assert_eq!(structured, &cli);
    assert_eq!(env.requests().await, 3);

    let tools = reply(2)["result"]["tools"].as_array().unwrap();
    let tool = tools
        .iter()
        .find(|t| t["name"] == "get_price_outlook")
        .unwrap();
    assert_eq!(
        (
            &tool["annotations"]["readOnlyHint"],
            &tool["annotations"]["idempotentHint"],
            &tool["annotations"]["openWorldHint"]
        ),
        (&json!(true), &json!(true), &json!(true))
    );
    assert_eq!(tool["inputSchema"]["required"], json!(["duration_minutes"]));
    assert!(
        tool["description"]
            .as_str()
            .unwrap()
            .contains("vatio's own estimate"),
        "{tool}"
    );
    // The same bounds as the cheapest tool, and no other argument.
    for id in [4, 5] {
        let r = reply(id);
        assert!(
            r.get("error").is_some() || r["result"]["isError"] == true,
            "{r}"
        );
    }
}
