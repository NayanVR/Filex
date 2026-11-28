//! The product-analytics event model: a name plus typed properties.
//!
//! **The PII guard is the type system.** A [`Prop`] can only be a number,
//! a bool, or a `&'static str` (or a list/tally of `&'static str`). There
//! is deliberately no `From<String>`: a file name, folder path, search
//! query, tag name or anything else the user typed is a runtime `String`
//! and simply cannot become a property. Labels have to be literals written
//! in this codebase, which is what makes "strictly no PII" reviewable:
//! grep the call sites, every value is visible in the source.
//!
//! The only free-text that ever leaves the machine is the exception text
//! (scrubbed, see [`super::exception`]) and feedback the user typed and
//! chose to send (scrubbed the same way).

use std::time::Duration;

use serde_json::{Map, Value};

/// One property value. See the module docs for why there is no string
/// variant that takes a runtime value.
#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A label from a closed vocabulary written in the source.
    Label(&'static str),
    /// Several labels (e.g. the filter kinds a query used).
    Labels(Vec<&'static str>),
    /// Per-label counts (e.g. keyboard-shortcut usage).
    Tally(Vec<(&'static str, u64)>),
}

impl From<bool> for Prop {
    fn from(v: bool) -> Self {
        Prop::Bool(v)
    }
}
impl From<&'static str> for Prop {
    fn from(v: &'static str) -> Self {
        Prop::Label(v)
    }
}
impl From<Vec<&'static str>> for Prop {
    fn from(v: Vec<&'static str>) -> Self {
        Prop::Labels(v)
    }
}
impl From<f64> for Prop {
    fn from(v: f64) -> Self {
        Prop::Float(v)
    }
}
/// Durations go out as whole milliseconds.
impl From<Duration> for Prop {
    fn from(v: Duration) -> Self {
        Prop::Int(i64::try_from(v.as_millis()).unwrap_or(i64::MAX))
    }
}
macro_rules! int_prop {
    ($($t:ty),*) => {$(
        impl From<$t> for Prop {
            fn from(v: $t) -> Self {
                Prop::Int(i64::try_from(v).unwrap_or(i64::MAX))
            }
        }
    )*};
}
int_prop!(u8, u16, u32, u64, usize, i32, i64);

impl Prop {
    fn to_json(&self) -> Value {
        match self {
            Prop::Bool(v) => Value::Bool(*v),
            Prop::Int(v) => Value::from(*v),
            Prop::Float(v) => serde_json::Number::from_f64(*v)
                .map(Value::Number)
                .unwrap_or(Value::Null),
            Prop::Label(v) => Value::from(*v),
            Prop::Labels(v) => Value::from(v.clone()),
            Prop::Tally(v) => Value::Object(
                v.iter()
                    .map(|(k, n)| ((*k).to_string(), Value::from(*n)))
                    .collect(),
            ),
        }
    }
}

/// A product event: a catalog name (see [`names`]) and its properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub props: Vec<(&'static str, Prop)>,
}

impl Event {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            props: Vec::new(),
        }
    }

    /// Add a property. Later keys overwrite earlier ones on the wire.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<Prop>) -> Self {
        self.props.push((key, value.into()));
        self
    }

    /// Add a property only when `value` is `Some`.
    #[must_use]
    pub fn with_opt(self, key: &'static str, value: Option<impl Into<Prop>>) -> Self {
        match value {
            Some(value) => self.with(key, value),
            None => self,
        }
    }

    /// The properties as a JSON object (no super-properties yet).
    pub fn props_json(&self) -> Map<String, Value> {
        self.props
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.to_json()))
            .collect()
    }
}

/// The event catalog. Every name PostHog will ever see from filex is here,
/// so the catalog in `docs/design-telemetry.md` can be checked against it.
pub mod names {
    // Lifecycle
    pub const APP_LAUNCHED: &str = "app_launched";
    pub const APP_SESSION_ENDED: &str = "app_session_ended";
    pub const USAGE_SUMMARY: &str = "usage_summary";
    pub const RESOURCE_SAMPLE: &str = "resource_sample";
    pub const INDEX_STATUS: &str = "index_status";
    // Navigation
    pub const FOLDER_OPENED: &str = "folder_opened";
    pub const FOLDER_LOAD_FAILED: &str = "folder_load_failed";
    // Search and Magic
    pub const SEARCH_SESSION: &str = "search_session";
    pub const MAGIC_PLAN_SHOWN: &str = "magic_plan_shown";
    pub const MAGIC_PLAN_APPLIED: &str = "magic_plan_applied";
    pub const MAGIC_INSTANT: &str = "magic_instant";
    // Files
    pub const FILE_OP: &str = "file_op";
    pub const FILE_OPENED: &str = "file_opened";
    pub const UNDO: &str = "undo";
    // Everything else the user can reach: views, tabs, panes, menus.
    pub const FEATURE_USED: &str = "feature_used";
    pub const SETTING_CHANGED: &str = "setting_changed";
    // Onboarding, updates, feedback
    pub const ONBOARDING_STEP: &str = "onboarding_step";
    pub const ONBOARDING_FINISHED: &str = "onboarding_finished";
    pub const UPDATE: &str = "update";
    pub const FEEDBACK_SUBMITTED: &str = "feedback_submitted";
    // Error tracking (PostHog's reserved name)
    pub const EXCEPTION: &str = "$exception";
}

/// Round a count into a coarse bucket label, for counts where the exact
/// number adds nothing but noise (and a little fingerprinting surface).
pub fn bucket(n: u64) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2..=5 => "2-5",
        6..=20 => "6-20",
        21..=100 => "21-100",
        101..=1_000 => "101-1k",
        1_001..=10_000 => "1k-10k",
        10_001..=100_000 => "10k-100k",
        _ => "100k+",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_serialize_by_type() {
        let event = Event::new(names::FOLDER_OPENED)
            .with("via", "sidebar")
            .with("load_ms", Duration::from_millis(42))
            .with("entries", 1200usize)
            .with("cached", false)
            .with("ratio", 0.5)
            .with("filters", vec!["kind", "modified"])
            .with_opt("rank", None::<u32>)
            .with_opt("tabs", Some(3u32));
        let json = Value::Object(event.props_json());
        assert_eq!(
            json,
            serde_json::json!({
                "via": "sidebar",
                "load_ms": 42,
                "entries": 1200,
                "cached": false,
                "ratio": 0.5,
                "filters": ["kind", "modified"],
                "tabs": 3,
            })
        );
    }

    #[test]
    fn tally_serializes_as_an_object() {
        let event = Event::new(names::USAGE_SUMMARY)
            .with("shortcuts", Prop::Tally(vec![("GoUp", 3), ("NewTab", 1)]));
        assert_eq!(
            Value::Object(event.props_json()),
            serde_json::json!({"shortcuts": {"GoUp": 3, "NewTab": 1}})
        );
    }

    #[test]
    fn non_finite_floats_become_null_not_a_broken_batch() {
        let event = Event::new("x").with("v", f64::NAN);
        assert_eq!(event.props_json()["v"], Value::Null);
    }

    #[test]
    fn buckets_are_monotonic_and_cover_the_edges() {
        assert_eq!(bucket(0), "0");
        assert_eq!(bucket(1), "1");
        assert_eq!(bucket(5), "2-5");
        assert_eq!(bucket(6), "6-20");
        assert_eq!(bucket(1_000), "101-1k");
        assert_eq!(bucket(1_001), "1k-10k");
        assert_eq!(bucket(u64::MAX), "100k+");
    }
}
