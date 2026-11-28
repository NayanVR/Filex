//! The internal wire representation of an event: a name and properties.
//!
//! Product code never builds one of these. It tracks a typed event from
//! [`super::catalog`], which is converted here. The constructors are
//! crate-private, so the catalog is the only public way in.

use serde_json::{Map, Value};

use super::schema::AnalyticsEvent;

/// One property value.
#[derive(Debug, Clone, PartialEq)]
pub enum Prop {
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A label from a closed vocabulary or a fixed id.
    Label(&'static str),
    Labels(Vec<&'static str>),
    /// Per-label counts.
    Tally(Vec<(&'static str, u64)>),
}

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

/// An event ready for the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub name: &'static str,
    pub props: Vec<(&'static str, Prop)>,
}

impl Event {
    /// Convert a catalog event.
    pub(crate) fn typed<E: AnalyticsEvent>(event: &E) -> Self {
        Self {
            name: E::NAME,
            props: event.props(),
        }
    }

    /// A propertyless event whose properties travel separately as
    /// pre-scrubbed JSON (`$exception`, `feedback_submitted`).
    pub(crate) fn raw(name: &'static str) -> Self {
        Self {
            name,
            props: Vec::new(),
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

/// Names of the events outside the typed catalog.
pub mod names {
    pub const EXCEPTION: &str = "$exception";
    pub const FEEDBACK_SUBMITTED: &str = "feedback_submitted";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_serialize_by_type() {
        let event = Event {
            name: "x",
            props: vec![
                ("label", Prop::Label("sidebar")),
                ("n", Prop::Int(42)),
                ("flag", Prop::Bool(false)),
                ("ratio", Prop::Float(0.5)),
                ("list", Prop::Labels(vec!["kind", "modified"])),
                ("tally", Prop::Tally(vec![("a", 3)])),
            ],
        };
        assert_eq!(
            Value::Object(event.props_json()),
            serde_json::json!({
                "label": "sidebar",
                "n": 42,
                "flag": false,
                "ratio": 0.5,
                "list": ["kind", "modified"],
                "tally": {"a": 3},
            })
        );
    }

    #[test]
    fn non_finite_floats_become_null_not_a_broken_batch() {
        let event = Event {
            name: "x",
            props: vec![("v", Prop::Float(f64::NAN))],
        };
        assert_eq!(event.props_json()["v"], Value::Null);
    }
}
