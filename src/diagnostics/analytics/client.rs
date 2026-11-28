//! The HTTP transport: `ureq` + rustls, the HTTP stack filex already ships
//! for updates. No reqwest or tokio, which is why this isn't the official
//! `posthog` crate (its wire format is what `wire.rs` reproduces).
//! Only compiled with the `analytics` feature, so the daemon and updater
//! builds contain no telemetry transport at all.

use std::time::Duration;

use serde_json::Value;

use super::pipeline::{PostOutcome, Transport};
use super::wire;

/// Environment variable naming the PostHog project API key.
pub const KEY_ENV: &str = "FILEX_POSTHOG_KEY";
/// Environment variable naming the ingestion host (region / reverse proxy).
pub const HOST_ENV: &str = "FILEX_POSTHOG_HOST";

/// The project key and host, or `None` when no key is configured (which
/// disables analytics entirely). A runtime env var wins (developer
/// override). Otherwise the value CI baked in at compile time is used. A
/// project API key is public by design (it can only write events), like
/// the Sentry DSN it replaces.
pub fn config() -> Option<(String, String)> {
    let key = std::env::var(KEY_ENV)
        .ok()
        .or_else(|| option_env!("FILEX_POSTHOG_KEY").map(str::to_string))
        .filter(|key| !key.trim().is_empty())?;
    let host = std::env::var(HOST_ENV)
        .ok()
        .or_else(|| option_env!("FILEX_POSTHOG_HOST").map(str::to_string))
        .filter(|host| !host.trim().is_empty())
        .unwrap_or_else(|| wire::DEFAULT_HOST.to_string());
    Some((
        key.trim().to_string(),
        host.trim().trim_end_matches('/').to_string(),
    ))
}

pub struct Ureq {
    agent: ureq::Agent,
    url: String,
    auth: String,
}

impl Ureq {
    pub fn new(key: &str, host: &str) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(10))
                // A batch never follows a redirect to another origin.
                .redirects(0)
                .user_agent(concat!("filex/", env!("CARGO_PKG_VERSION")))
                .build(),
            url: format!("{host}{}", wire::CAPTURE_PATH),
            auth: format!("Bearer {key}"),
        }
    }
}

impl Transport for Ureq {
    fn post(&self, body: &Value) -> PostOutcome {
        let result = self
            .agent
            .post(&self.url)
            .set("Authorization", &self.auth)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());
        match result {
            Ok(_) => PostOutcome::Delivered,
            Err(ureq::Error::Status(code, _)) => classify(code),
            Err(ureq::Error::Transport(_)) => PostOutcome::Retry,
        }
    }
}

/// 429 and 5xx are worth retrying; any other non-2xx never will succeed.
fn classify(code: u16) -> PostOutcome {
    match code {
        200..=299 => PostOutcome::Delivered,
        408 | 429 | 500..=599 => PostOutcome::Retry,
        _ => PostOutcome::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real transport against a loopback server: path, auth header,
    /// content type and body are what PostHog's capture v1 expects.
    #[test]
    fn posts_the_batch_to_capture_v1_with_bearer_auth() {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = Vec::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                head.push(line.trim_end().to_string());
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let mut stream = stream;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                .unwrap();
            (head, String::from_utf8(body).unwrap())
        });

        let transport = Ureq::new("phc_test", &host);
        let body = serde_json::json!({"batch": [{"event": "x"}]});
        assert_eq!(transport.post(&body), PostOutcome::Delivered);

        let (head, sent) = server.join().unwrap();
        assert_eq!(head[0], "POST /i/v1/analytics/events HTTP/1.1");
        let has = |h: &str| head.iter().any(|l| l.eq_ignore_ascii_case(h));
        assert!(has("authorization: Bearer phc_test"), "{head:?}");
        assert!(has("content-type: application/json"), "{head:?}");
        assert_eq!(serde_json::from_str::<Value>(&sent).unwrap(), body);
    }

    #[test]
    fn unreachable_host_is_retried_not_dropped() {
        // Port 9 on loopback: nothing listens, the connect is refused.
        let transport = Ureq::new("k", "http://127.0.0.1:9");
        assert_eq!(transport.post(&serde_json::json!({})), PostOutcome::Retry);
    }

    #[test]
    fn status_classification() {
        assert_eq!(classify(200), PostOutcome::Delivered);
        assert_eq!(classify(429), PostOutcome::Retry);
        assert_eq!(classify(503), PostOutcome::Retry);
        assert_eq!(classify(400), PostOutcome::Reject);
        assert_eq!(classify(401), PostOutcome::Reject);
    }
}
