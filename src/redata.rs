//! Client for REData, Red Eléctrica's public data API (<https://www.ree.es/en/apidatos>).
//! No credentials are needed.
//!
//! Every widget answers with the same envelope: a `data` header plus an `included`
//! array of series, each holding `attributes.values[] = { value, datetime }`. The
//! documentation describes failures as `{ "errors": [ { status, title, detail } ] }`.

use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;

use crate::{
    error::{Error, Result},
    text,
};

pub const DEFAULT_BASE_URL: &str = "https://apidatos.ree.es";
pub const SOURCE_NAME: &str = "Red Eléctrica de España (REData)";

#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub data: Option<Header>,
    #[serde(default)]
    pub included: Vec<Series>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    #[serde(default)]
    pub attributes: Option<HeaderAttributes>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HeaderAttributes {
    #[serde(rename = "last-update", default)]
    pub last_update: Option<String>,
}

impl Envelope {
    /// When Red Eléctrica last updated the data behind this answer.
    pub fn last_update(&self) -> Option<Timestamp> {
        let attributes = self.data.as_ref()?.attributes.as_ref()?;
        attributes.last_update.as_deref()?.parse().ok()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Series {
    #[serde(deserialize_with = "string_or_number")]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    pub attributes: Attributes,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attributes {
    #[serde(default)]
    pub title: Option<String>,
    /// Classification some widgets attach to a series (for example renewable or not).
    #[serde(rename = "type", default)]
    pub class: Option<String>,
    /// True for totals and other series derived from the rest.
    #[serde(default)]
    pub composite: bool,
    #[serde(rename = "last-update", default)]
    pub last_update: Option<String>,
    #[serde(default)]
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Value {
    /// Null when the operator has no figure for that instant.
    #[serde(default)]
    pub value: Option<f64>,
    pub datetime: String,
}

impl Series {
    pub fn last_update(&self) -> Option<Timestamp> {
        self.attributes.last_update.as_deref()?.parse().ok()
    }

    /// Non-null observations as `(instant, value)`, sorted by time.
    pub fn observations(&self) -> Result<Vec<(Timestamp, f64)>> {
        let mut out = Vec::with_capacity(self.attributes.values.len());
        for v in &self.attributes.values {
            let Some(value) = v.value else { continue };
            let at: Timestamp = v.datetime.parse().map_err(|_| Error::Upstream {
                // The request succeeded; its content is the problem, so retrying will not help.
                status: Some(200),
                message: format!(
                    "REData returned a timestamp this version cannot read: '{}'",
                    text::clip(&v.datetime)
                ),
            })?;
            out.push((at, value));
        }
        out.sort_by_key(|(at, _)| *at);
        Ok(out)
    }
}

/// What an unsuccessful request turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// REData itself reported an error in its answer.
    Api { status: u16, message: String },
    /// Anything else: no connection, timeout, bot wall, unreadable body.
    Other(Error),
}

impl Failure {
    /// Whether REData is saying "nothing published for those filters". Its wording for
    /// that case has not been observed, so this matches the obvious phrasings; an
    /// unmatched message is simply reported as the upstream error it is.
    pub fn is_no_data(&self) -> bool {
        match self {
            Self::Api { message, .. } => {
                let m = message.to_lowercase();
                ["no data", "no values", "no hay datos", "sin datos"]
                    .iter()
                    .any(|p| m.contains(p))
            }
            Self::Other(_) => false,
        }
    }

    /// Worth another attempt straight away: no connection, the bot wall, a server-side
    /// error, or an answer that arrived cut off.
    fn is_transient(&self) -> bool {
        match self {
            Self::Api { status, .. } => *status >= 500 && !self.is_no_data(),
            Self::Other(Error::Network { .. } | Error::Blocked { .. }) => true,
            Self::Other(Error::Upstream { status, .. }) => status.is_none_or(|s| s >= 500),
            Self::Other(_) => false,
        }
    }

    pub fn into_error(self) -> Error {
        match self {
            Self::Api { status, message } => Error::Upstream {
                status: Some(status),
                message: format!("REData error {status}: {message}"),
            },
            Self::Other(err) => err,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    /// Host shown in connection errors.
    host: String,
}

impl Client {
    pub fn new(base_url: Option<&str>) -> Result<Self> {
        // reqwest is built without a bundled crypto provider; install ring once.
        // An Err only means a provider is already installed, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "vatio/",
                env!("CARGO_PKG_VERSION"),
                " (+",
                env!("CARGO_PKG_REPOSITORY"),
                ")"
            ))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(Error::internal)?;
        let base = base_url
            .unwrap_or(DEFAULT_BASE_URL)
            .trim_end_matches('/')
            .to_owned();
        let public = redact(&base);
        let authority = public.split("://").last().unwrap_or(&public);
        let host = authority.split('/').next().unwrap_or(authority).to_owned();
        Ok(Self { http, base, host })
    }

    /// The request URL for a widget, exactly as it will be sent.
    pub fn url(&self, widget: &str, query: &[(&str, &str)]) -> String {
        let mut url = format!("{}/es/datos/{widget}", self.base);
        for (i, (key, value)) in query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(value);
        }
        url
    }

    /// GET a widget.
    ///
    /// REData sits behind a bot wall that refuses a share of perfectly valid requests
    /// at random, so transient failures (no connection, server errors, the bot wall)
    /// are retried twice with a growing pause before giving up.
    pub async fn get(&self, url: &str) -> std::result::Result<Envelope, Failure> {
        const PAUSES_MS: [u64; 2] = [300, 900];
        let mut attempt = 0;
        loop {
            match self.get_once(url, None).await {
                Err(failure) if failure.is_transient() && attempt < PAUSES_MS.len() => {
                    let pause = PAUSES_MS[attempt] + jitter_ms();
                    tokio::time::sleep(Duration::from_millis(pause)).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// GET a widget once, with a short deadline: for callers that already hold an
    /// answer and are only checking whether it has changed.
    pub async fn get_quick(&self, url: &str) -> std::result::Result<Envelope, Failure> {
        self.get_once(url, Some(Duration::from_secs(5))).await
    }

    async fn get_once(
        &self,
        url: &str,
        deadline: Option<Duration>,
    ) -> std::result::Result<Envelope, Failure> {
        let network = |e: reqwest::Error| {
            let what = if e.is_timeout() {
                "request timed out"
            } else if e.is_connect() {
                "connection failed"
            } else {
                "request failed"
            };
            // The innermost cause says why (name lookup, refused, certificate); the
            // outer error only says that it failed, and may quote the URL.
            let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(&e);
            while let Some(next) = cause.and_then(std::error::Error::source) {
                cause = Some(next);
            }
            let message = match cause.map(|c| text::clip(&c.to_string())) {
                Some(why) if !why.is_empty() => format!("{what} ({why})"),
                _ => what.to_owned(),
            };
            Failure::Other(Error::Network {
                host: self.host.clone(),
                message,
            })
        };

        let mut request = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(deadline) = deadline {
            request = request.timeout(deadline);
        }
        let response = request.send().await.map_err(network)?;
        let status = response.status();
        let body = response.bytes().await.map_err(network)?;
        classify(status.as_u16(), &body, &self.host)
    }
}

/// Decide what a response is. The body is inspected whatever the status, because
/// REData can report an error inside a 200 and its bot wall can answer with an HTML
/// page under any status.
fn classify(status: u16, body: &[u8], host: &str) -> std::result::Result<Envelope, Failure> {
    let unreadable = || {
        Failure::Other(Error::Upstream {
            status: Some(status),
            message: format!("REData answered HTTP {status} with a body this version cannot read"),
        })
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Err(Failure::Other(if status >= 500 {
            Error::Network {
                host: host.to_owned(),
                message: format!("server answered HTTP {status}"),
            }
        } else if status == 403 || looks_like_html(body) {
            Error::Blocked { status }
        } else {
            // Not JSON at all, typically an answer cut off on the way. No status is
            // recorded, which marks it as worth asking again.
            Error::Upstream {
                status: None,
                message: format!("REData answered HTTP {status} with a body that is not JSON"),
            }
        }));
    };

    // An `errors` key is the API reporting a failure. Its exact shape is not pinned
    // down by the documentation, so take a message from wherever one can be found.
    let errors = json.get("errors").filter(|e| match e {
        serde_json::Value::Null => false,
        serde_json::Value::Array(items) => !items.is_empty(),
        _ => true,
    });
    if let Some(errors) = errors {
        let first = errors
            .as_array()
            .and_then(|items| items.first())
            .unwrap_or(errors);
        let message = first
            .get("detail")
            .or_else(|| first.get("title"))
            .unwrap_or(first)
            .as_str()
            .map(text::clip)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "unspecified error".to_owned());
        return Err(Failure::Api { status, message });
    }
    if !(200..300).contains(&status) {
        return Err(Failure::Other(Error::Upstream {
            status: Some(status),
            message: format!("REData answered HTTP {status}"),
        }));
    }
    // A real answer always has the header or the series list; JSON with neither is
    // something else, and must not be mistaken for "nothing published".
    if json.get("data").is_none() && json.get("included").is_none() {
        return Err(unreadable());
    }
    serde_json::from_value::<Envelope>(json).map_err(|_| unreadable())
}

fn looks_like_html(body: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&body[..body.len().min(512)]).to_lowercase();
    head.trim_start().starts_with('<') || head.contains("<html") || head.contains("incapsula")
}

/// A little spread so parallel requests do not retry in step.
fn jitter_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) % 120)
}

/// A URL with any `user:password@` removed, for showing in output and errors.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

fn string_or_number<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        Text(String),
        Number(serde_json::Number),
    }
    Ok(match Id::deserialize(d)? {
        Id::Text(s) => s,
        Id::Number(n) => n.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;

    #[test]
    fn parses_the_envelope_and_tolerates_nulls_and_unknown_fields() {
        let body = r##"{
          "data": {"type": "x", "id": "mer13", "attributes": {"title": "x"}, "meta": {}},
          "included": [{
            "type": "PVPC", "id": "1001", "groupId": null,
            "attributes": {
              "title": "PVPC", "description": null, "color": "#ffcf09", "icon": null, "type": null,
              "magnitude": "price", "composite": false,
              "last-update": "2026-11-24T20:46:06.000+01:00",
              "values": [
                {"value": 134.22, "percentage": 0.54, "datetime": "2026-11-25T01:00:00.000+01:00"},
                {"value": 166.11, "percentage": "n/a", "datetime": "2026-11-25T00:00:00.000+01:00"},
                {"value": null, "percentage": null, "datetime": "2026-11-25T02:00:00.000+01:00"},
                {"value": 110, "percentage": 1, "datetime": "2026-11-25T03:00:00.000+01:00"}
              ]
            }
          }, {"type": "n", "id": 600, "attributes": {}}]
        }"##;
        let env = classify(200, body.as_bytes(), "h").unwrap();
        assert_eq!(env.included[1].id, "600");
        let series = &env.included[0];
        assert_eq!(
            series.last_update().unwrap(),
            "2026-11-24T19:46:06Z".parse().unwrap()
        );
        let obs = series.observations().unwrap();
        // Sorted by time, null dropped, integer-looking number accepted, and the
        // unused `percentage` field cannot break the read whatever it holds.
        assert_eq!(obs.len(), 3);
        assert_eq!(obs[0], ("2026-11-24T23:00:00Z".parse().unwrap(), 166.11));
        assert_eq!(obs[2].1, 110.0);
    }

    #[test]
    fn url_is_built_verbatim_and_credentials_never_shown() {
        let c = Client::new(Some("http://user:s3cret@localhost:9/")).unwrap();
        let url = c.url(
            "mercados/precios-mercados-tiempo-real",
            &[("start_date", "2026-11-25T00:00"), ("time_trunc", "hour")],
        );
        assert_eq!(
            url,
            "http://user:s3cret@localhost:9/es/datos/mercados/precios-mercados-tiempo-real?start_date=2026-11-25T00:00&time_trunc=hour"
        );
        assert_eq!(c.host, "localhost:9");
        assert!(redact(&url).starts_with("http://localhost:9/es/datos/"));
        assert_eq!(
            redact("https://apidatos.ree.es/x?a=b@c"),
            "https://apidatos.ree.es/x?a=b@c"
        );
    }

    fn failure(status: u16, body: &str) -> Failure {
        classify(status, body.as_bytes(), "apidatos.ree.es").unwrap_err()
    }

    fn other(status: u16, body: &str) -> Error {
        match failure(status, body) {
            Failure::Other(err) => err,
            api => panic!("expected a non-API failure, got {api:?}"),
        }
    }

    #[test]
    fn only_html_or_403_is_called_the_bot_wall() {
        let wall = "<html><head><META NAME=\"robots\"></head><body>Request unsuccessful. \
                    Incapsula incident ID: 1</body></html>";
        assert_eq!(other(403, wall), Error::Blocked { status: 403 });
        assert_eq!(other(200, wall), Error::Blocked { status: 200 });
        assert_eq!(other(403, ""), Error::Blocked { status: 403 });
        // An answer that is not JSON at all is an upstream problem, not the wall, and
        // since it is usually a response cut short it is worth asking again.
        for body in ["", "{\"data\": {\"attr", "\u{feff}{}", "not json"] {
            let failure = failure(200, body);
            assert!(failure.is_transient(), "{body:?}");
            let err = failure.into_error();
            assert_eq!(
                (err.code(), err.retryable()),
                (ErrorCode::Upstream, true),
                "{body:?}"
            );
        }
        // A server error is a server error, whatever its body looks like.
        assert!(matches!(
            other(503, "upstream connect error"),
            Error::Network { .. }
        ));
        assert!(matches!(other(500, wall), Error::Network { .. }));
        assert!(
            Failure::Other(other(503, "{}")).is_transient(),
            "a JSON 503 is retried too"
        );
        assert!(matches!(
            other(404, "{}"),
            Error::Upstream {
                status: Some(404),
                ..
            }
        ));
    }

    #[test]
    fn an_errors_key_is_an_api_error_in_any_shape_and_under_any_status() {
        let api = r#"{"errors":[{"code":400,"status":"400","title":"Bad request","detail":"time_trunc not allowed"}]}"#;
        let expected = |status| Failure::Api {
            status,
            message: "time_trunc not allowed".into(),
        };
        assert_eq!(failure(400, api), expected(400));
        assert_eq!(failure(200, api), expected(200));
        let message = |body| match failure(200, body) {
            Failure::Api { message, .. } => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(message(r#"{"errors":"boom"}"#), "boom");
        assert_eq!(message(r#"{"errors":{"detail":"x"}}"#), "x");
        assert_eq!(
            message(r#"{"errors":[{"title":"only a title"}]}"#),
            "only a title"
        );
        assert_eq!(message(r#"{"errors":[{}]}"#), "unspecified error");
        // Control characters in upstream text never reach the terminal.
        assert_eq!(
            message("{\"errors\":[{\"detail\":\"\\u001b[2Jwiped\"}]}"),
            "[2Jwiped"
        );
    }

    #[test]
    fn json_that_is_not_an_answer_is_not_mistaken_for_an_empty_one() {
        for body in [
            "{}",
            "[]",
            "null",
            "42",
            r#"{"errors":[]}"#,
            r#"{"included":"x"}"#,
        ] {
            assert!(
                matches!(
                    other(200, body),
                    Error::Upstream {
                        status: Some(200),
                        ..
                    }
                ),
                "{body}"
            );
        }
        // A genuine answer with no series is fine: that is what "not published" looks like.
        let empty = classify(200, br#"{"data":{"type":"x"},"included":[]}"#, "h").unwrap();
        assert!(empty.included.is_empty());
    }

    #[test]
    fn server_side_api_errors_are_retried_but_no_data_is_not() {
        let transient = |status, message: &str| {
            Failure::Api {
                status,
                message: message.into(),
            }
            .is_transient()
        };
        assert!(transient(503, "Service unavailable"));
        assert!(!transient(
            502,
            "There are no data for the selected filters."
        ));
        assert!(!transient(400, "Error in start date"));
        assert!(Failure::Other(Error::Blocked { status: 403 }).is_transient());
        assert!(!Failure::Other(Error::usage("x")).is_transient());
    }

    #[test]
    fn a_success_document_carries_its_update_time() {
        let body = r#"{"data":{"type":"x","id":"dem15","attributes":{"title":"x","last-update":"2026-11-26T20:12:05.000+01:00"}},"included":[]}"#;
        let env = classify(200, body.as_bytes(), "h").unwrap();
        assert_eq!(
            env.last_update().unwrap(),
            "2026-11-26T19:12:05Z".parse().unwrap()
        );
        assert!(
            classify(200, br#"{"included":[]}"#, "h")
                .unwrap()
                .last_update()
                .is_none()
        );
    }

    #[test]
    fn an_unreadable_timestamp_is_reported_without_its_control_characters() {
        let body = "{\"included\":[{\"id\":\"1001\",\"attributes\":{\"values\":[{\"value\":1,\"datetime\":\"\\u001b]0;pwned\\u0007soon\"}]}}]}";
        let env = classify(200, body.as_bytes(), "h").unwrap();
        let err = env.included[0].observations().unwrap_err();
        assert_eq!(
            err.to_string(),
            "REData returned a timestamp this version cannot read: ']0;pwnedsoon'"
        );
        assert!(!err.retryable());
    }
}
