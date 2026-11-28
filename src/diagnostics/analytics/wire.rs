//! PostHog capture-v1 wire format, pure and SDK-free.
//!
//! Mirrors what PostHog's own SDKs send (verified against the official
//! `posthog` Rust crate 1.7 and the Python SDK 8.4):
//!
//! ```text
//! POST {host}/i/v1/analytics/events
//! Authorization: Bearer <project api key>
//! { "created_at": RFC3339,
//!   "batch": [ { "event", "uuid" (v7), "distinct_id", "timestamp",
//!                "session_id", "options": {"process_person_profile": false},
//!                "properties": {…} } ] }
//! ```
//!
//! `process_person_profile: false` makes every event anonymous: PostHog
//! keeps no person profile for the install ID. `$geoip_disable` stops
//! GeoIP enrichment from the request IP. (Dropping the IP itself is the
//! project setting "Discard client IP data", see `docs/design-telemetry.md`.)

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};

use super::event::Event;

/// The capture-v1 endpoint path, appended to the configured host.
pub const CAPTURE_PATH: &str = "/i/v1/analytics/events";

/// Version of the event shapes in [`super::catalog`], sent on every event
/// as `schema_version`. Bump it on a breaking change (a renamed or
/// retyped property, a changed meaning) so dashboards can split on it.
pub const SCHEMA_VERSION: u32 = 1;

/// Default ingestion host when the build doesn't name one.
pub const DEFAULT_HOST: &str = "https://us.i.posthog.com";

/// Properties stamped on every event: coarse, non-identifying facts about
/// the build and platform. Built once per process.
#[derive(Debug, Clone)]
pub struct SuperProps {
    pub app: &'static str,
    pub version: &'static str,
}

impl SuperProps {
    pub fn to_json(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("$lib".into(), "filex".into());
        map.insert("$lib_version".into(), self.version.into());
        map.insert("$app_name".into(), self.app.into());
        map.insert("$app_version".into(), self.version.into());
        map.insert("$os".into(), posthog_os_name(std::env::consts::OS).into());
        map.insert("arch".into(), std::env::consts::ARCH.into());
        // Developer runs never mix into real-user numbers.
        map.insert(
            "environment".into(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
        );
        map.insert("$geoip_disable".into(), true.into());
        map.insert("schema_version".into(), SCHEMA_VERSION.into());
        map
    }
}

/// PostHog's display names for operating systems (what its web SDK sends),
/// so the built-in OS breakdowns group filex with everything else.
fn posthog_os_name(os: &str) -> &'static str {
    match os {
        "macos" => "Mac OS X",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => "Other",
    }
}

/// Everything about this process the envelope needs besides the event.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The random install ID (see [`super::identity`]).
    pub distinct_id: String,
    /// A random per-process ID, so PostHog can stitch a run's events.
    pub session_id: String,
}

/// One queued event with the facts captured at `track` time.
#[derive(Debug, Clone)]
pub struct Pending {
    pub event: Event,
    /// Pre-built properties that bypass [`Event`]'s typing. Only
    /// [`super::exception`] and feedback produce these, and both scrub.
    pub raw_props: Option<Map<String, Value>>,
    pub at: DateTime<Utc>,
    pub uuid: String,
}

impl Pending {
    pub fn new(event: Event, rand: [u8; 10]) -> Self {
        let at = Utc::now();
        Self {
            uuid: uuid_v7(at.timestamp_millis().max(0) as u64, rand),
            event,
            raw_props: None,
            at,
        }
    }
}

/// Build one wire event. Event properties override super-properties, so
/// a drained crash can report the version that crashed, not the uploader.
pub fn wire_event(pending: &Pending, identity: &Identity, sup: &SuperProps) -> Value {
    let mut properties = sup.to_json();
    properties.extend(pending.event.props_json());
    if let Some(raw) = &pending.raw_props {
        properties.extend(raw.clone());
    }
    json!({
        "event": pending.event.name,
        "uuid": pending.uuid,
        "distinct_id": identity.distinct_id,
        "timestamp": timestamp(pending.at),
        "session_id": identity.session_id,
        "options": { "process_person_profile": false },
        "properties": Value::Object(properties),
    })
}

/// The batch envelope around already-built wire events.
pub fn batch_body(events: Vec<Value>, created_at: DateTime<Utc>) -> Value {
    json!({ "created_at": timestamp(created_at), "batch": events })
}

/// RFC3339 with millisecond precision and a `Z` suffix.
pub fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A UUIDv7 string: 48-bit Unix-millis timestamp, version and variant
/// bits, the rest random. Time-ordered, which is what PostHog's SDKs use
/// for event IDs (dedup is keyed on it).
pub fn uuid_v7(unix_millis: u64, rand: [u8; 10]) -> String {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&unix_millis.to_be_bytes()[2..]);
    b[6..].copy_from_slice(&rand);
    b[6] = (b[6] & 0x0f) | 0x70; // version 7
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    format_uuid(&b)
}

/// A random UUIDv4 string, for the install and session IDs.
pub fn uuid_v4(rand: [u8; 16]) -> String {
    let mut b = rand;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format_uuid(&b)
}

fn format_uuid(b: &[u8; 16]) -> String {
    let hex = |range: std::ops::Range<usize>| {
        b[range]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    format!(
        "{}-{}-{}-{}-{}",
        hex(0..4),
        hex(4..6),
        hex(6..8),
        hex(8..10),
        hex(10..16)
    )
}

/// OS randomness, or a time-derived fallback if the OS refuses (which
/// only costs uniqueness of an analytics ID, never correctness).
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_le_bytes();
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = nanos[i % nanos.len()] ^ (i as u8).wrapping_mul(31);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v7_has_version_variant_and_timestamp() {
        let id = uuid_v7(0x0189_0000_0000, [0xff; 10]);
        assert_eq!(id.len(), 36);
        assert!(id.starts_with("01890000-0000-7"), "{id}");
        // Variant nibble is 8..b.
        let variant = id.as_bytes()[19];
        assert!(b"89ab".contains(&variant), "{id}");
    }

    #[test]
    fn uuid_v7_sorts_by_time() {
        let a = uuid_v7(1_700_000_000_000, [0xff; 10]);
        let b = uuid_v7(1_700_000_000_001, [0x00; 10]);
        assert!(a < b);
    }

    #[test]
    fn uuid_v4_shape() {
        let id = uuid_v4([0; 16]);
        assert_eq!(id, "00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn wire_event_is_anonymous_and_carries_super_props() {
        let event = Event {
            name: "folder_opened",
            props: vec![("via", super::super::event::Prop::Label("up"))],
        };
        let pending = Pending::new(event, [1; 10]);
        let identity = Identity {
            distinct_id: "install".into(),
            session_id: "session".into(),
        };
        let sup = SuperProps {
            app: "filex",
            version: "9.9.9",
        };
        let wire = wire_event(&pending, &identity, &sup);
        assert_eq!(wire["event"], "folder_opened");
        assert_eq!(wire["distinct_id"], "install");
        assert_eq!(wire["session_id"], "session");
        assert_eq!(wire["options"]["process_person_profile"], false);
        assert_eq!(wire["properties"]["via"], "up");
        assert_eq!(wire["properties"]["$app_version"], "9.9.9");
        assert_eq!(wire["properties"]["$geoip_disable"], true);
        assert!(wire["timestamp"].as_str().unwrap().ends_with('Z'));
        // No identifying property sneaks in via super-properties.
        for key in ["$ip", "$user_id", "email", "$set", "$set_once"] {
            assert!(wire["properties"].get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn event_props_override_super_props() {
        let pending = Pending::new(
            Event {
                name: "$exception",
                props: vec![("$app_version", super::super::event::Prop::Label("0.1.0"))],
            },
            [0; 10],
        );
        let identity = Identity {
            distinct_id: "d".into(),
            session_id: "s".into(),
        };
        let sup = SuperProps {
            app: "filex",
            version: "0.5.0",
        };
        assert_eq!(
            wire_event(&pending, &identity, &sup)["properties"]["$app_version"],
            "0.1.0"
        );
    }

    #[test]
    fn batch_body_shape() {
        let body = batch_body(vec![json!({"event": "x"})], Utc::now());
        assert!(body["created_at"].is_string());
        assert_eq!(body["batch"][0]["event"], "x");
        assert!(
            body.get("api_key").is_none(),
            "v1 auth is the Bearer header"
        );
    }
}
