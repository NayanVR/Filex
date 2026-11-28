//! The machinery behind the typed event catalog ([`super::catalog`]).
//!
//! - [`Field`] is implemented only for types that cannot carry user data:
//!   integers, bools, floats, durations, `&'static str` ids, label enums
//!   made with [`labels!`], lists and tallies of those, and `Option`s of
//!   them. There is no impl for `String`, `PathBuf` or `&str` with a
//!   non-static lifetime, so a struct field holding a path or query does
//!   not compile.
//! - [`labels!`] declares a closed vocabulary (an enum whose variants map
//!   to wire strings). Typos become compile errors, and the allowed values
//!   appear in the generated catalog.
//! - [`events!`] declares event structs. Each gets an [`AnalyticsEvent`]
//!   impl (wire name + properties) and a [`EventSchema`] entry, which the
//!   docs are generated from (`docs/analytics-events.md`).

use std::time::Duration;

use super::event::Prop;

/// What a property's values look like, for the generated catalog.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Int,
    Millis,
    Bool,
    Float,
    /// One label; `Some` lists the closed set, `None` is an id from a
    /// source enum (e.g. `AppCommand::id`), documented on the field.
    Label(Option<&'static [&'static str]>),
    Labels(Option<&'static [&'static str]>),
    /// `{label: count}`.
    Tally(Option<&'static [&'static str]>),
    /// One of several shapes (a setting's new value).
    Variant(&'static str),
    Optional(Box<FieldType>),
}

impl FieldType {
    /// A compact rendering for the catalog table.
    pub fn render(&self) -> String {
        let set = |values: &Option<&'static [&'static str]>| match values {
            Some(all) => all.join(" \\| "),
            None => "id".to_string(),
        };
        match self {
            Self::Int => "int".into(),
            Self::Millis => "ms".into(),
            Self::Bool => "bool".into(),
            Self::Float => "float".into(),
            Self::Label(values) => set(values),
            Self::Labels(values) => format!("[{}]", set(values)),
            Self::Tally(values) => format!("{{{}: int}}", set(values)),
            Self::Variant(text) => (*text).to_string(),
            Self::Optional(inner) => format!("{}?", inner.render()),
        }
    }
}

/// A value that may be an event property. See the module docs for why the
/// set of implementors is closed.
///
/// An event with only allowed field types compiles:
///
/// ```
/// filex::diagnostics::analytics::schema::events! {
///     /// Fine.
///     Ok => "ok" { count: u64, elapsed_ms: std::time::Duration, flag: bool }
/// }
/// ```
///
/// A field that could carry user text does not (the PII guard):
///
/// ```compile_fail
/// filex::diagnostics::analytics::schema::events! {
///     /// Rejected: `String` is not a `Field`.
///     Leak => "leak" { path: String }
/// }
/// ```
pub trait Field {
    /// The wire value, or `None` to omit the property.
    fn prop(&self) -> Option<Prop>;
    fn schema() -> FieldType;
}

macro_rules! int_field {
    ($($t:ty),*) => {$(
        impl Field for $t {
            fn prop(&self) -> Option<Prop> {
                Some(Prop::Int(i64::try_from(*self).unwrap_or(i64::MAX)))
            }
            fn schema() -> FieldType {
                FieldType::Int
            }
        }
    )*};
}
int_field!(u8, u16, u32, u64, usize, i32, i64);

impl Field for bool {
    fn prop(&self) -> Option<Prop> {
        Some(Prop::Bool(*self))
    }
    fn schema() -> FieldType {
        FieldType::Bool
    }
}

impl Field for f64 {
    fn prop(&self) -> Option<Prop> {
        Some(Prop::Float(*self))
    }
    fn schema() -> FieldType {
        FieldType::Float
    }
}

/// Durations go out as whole milliseconds.
impl Field for Duration {
    fn prop(&self) -> Option<Prop> {
        Some(Prop::Int(
            i64::try_from(self.as_millis()).unwrap_or(i64::MAX),
        ))
    }
    fn schema() -> FieldType {
        FieldType::Millis
    }
}

/// An id from a source enum (`AppCommand::id`, `PlanError::id`, …). Only
/// literals have the `'static` lifetime, so this can't carry user text.
impl Field for &'static str {
    fn prop(&self) -> Option<Prop> {
        Some(Prop::Label(self))
    }
    fn schema() -> FieldType {
        FieldType::Label(None)
    }
}

impl<T: Field> Field for Option<T> {
    fn prop(&self) -> Option<Prop> {
        self.as_ref().and_then(Field::prop)
    }
    fn schema() -> FieldType {
        FieldType::Optional(Box::new(T::schema()))
    }
}

/// A type that renders as one label: `&'static str` ids and [`labels!`]
/// enums. Lists and tallies are built from these.
pub trait Label: Copy {
    fn as_label(self) -> &'static str;
    fn values() -> Option<&'static [&'static str]>;
}

impl Label for &'static str {
    fn as_label(self) -> &'static str {
        self
    }
    fn values() -> Option<&'static [&'static str]> {
        None
    }
}

impl<L: Label> Field for Vec<L> {
    fn prop(&self) -> Option<Prop> {
        Some(Prop::Labels(self.iter().map(|l| l.as_label()).collect()))
    }
    fn schema() -> FieldType {
        FieldType::Labels(L::values())
    }
}

/// Per-label counts, e.g. `{move: 3, create: 1}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally<L>(pub Vec<(L, u64)>);

impl<L> Default for Tally<L> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<L: Label + PartialEq> Tally<L> {
    /// Count one occurrence of `label`.
    pub fn bump(&mut self, label: L) {
        match self.0.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += 1,
            None => self.0.push((label, 1)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<L: Label + PartialEq> FromIterator<L> for Tally<L> {
    fn from_iter<I: IntoIterator<Item = L>>(iter: I) -> Self {
        let mut tally = Self(Vec::new());
        for label in iter {
            tally.bump(label);
        }
        tally
    }
}

impl<L: Label> Field for Tally<L> {
    fn prop(&self) -> Option<Prop> {
        Some(Prop::Tally(
            self.0.iter().map(|(l, n)| (l.as_label(), *n)).collect(),
        ))
    }
    fn schema() -> FieldType {
        FieldType::Tally(L::values())
    }
}

/// Implemented by every [`events!`] struct.
pub trait AnalyticsEvent {
    /// The wire event name.
    const NAME: &'static str;
    /// The properties, in declaration order, `None` fields omitted.
    fn props(&self) -> Vec<(&'static str, Prop)>;
}

/// One field of an event, for the generated catalog.
#[derive(Debug, Clone)]
pub struct FieldSchema {
    pub name: &'static str,
    pub ty: FieldType,
    pub doc: &'static str,
}

/// One event, for the generated catalog.
#[derive(Debug, Clone)]
pub struct EventSchema {
    pub name: &'static str,
    pub doc: &'static str,
    pub fields: Vec<FieldSchema>,
}

/// Declare a closed vocabulary:
///
/// ```ignore
/// labels! {
///     /// How a folder was reached.
///     pub enum NavVia { Up = "up", Back = "back" }
/// }
/// ```
#[macro_export]
#[doc(hidden)]
macro_rules! __analytics_labels {
    ($(
        $(#[doc = $doc:literal])*
        pub enum $name:ident { $( $(#[doc = $vdoc:literal])* $variant:ident = $wire:literal ),* $(,)? }
    )*) => {$(
        $(#[doc = $doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $( $(#[doc = $vdoc])* $variant ),* }

        impl $name {
            /// Every wire value, in declaration order.
            pub const ALL: &'static [&'static str] = &[$($wire),*];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire),* }
            }
        }

        impl $crate::diagnostics::analytics::schema::Label for $name {
            fn as_label(self) -> &'static str {
                self.as_str()
            }
            fn values() -> Option<&'static [&'static str]> {
                Some(Self::ALL)
            }
        }

        impl $crate::diagnostics::analytics::schema::Field for $name {
            fn prop(&self) -> Option<$crate::diagnostics::analytics::event::Prop> {
                Some($crate::diagnostics::analytics::event::Prop::Label(self.as_str()))
            }
            fn schema() -> $crate::diagnostics::analytics::schema::FieldType {
                $crate::diagnostics::analytics::schema::FieldType::Label(Some(Self::ALL))
            }
        }
    )*};
}
pub use crate::__analytics_labels as labels;

/// Declare the event structs and the `catalog()` listing them. See
/// [`super::catalog`].
#[macro_export]
#[doc(hidden)]
macro_rules! __analytics_events {
    ($(
        $(#[doc = $doc:literal])*
        $name:ident => $wire:literal {
            $( $(#[doc = $fdoc:literal])* $field:ident : $ty:ty ),* $(,)?
        }
    )*) => {
        $(
            $(#[doc = $doc])*
            #[derive(Debug, Clone, PartialEq)]
            pub struct $name { $( $(#[doc = $fdoc])* pub $field: $ty, )* }

            impl $crate::diagnostics::analytics::schema::AnalyticsEvent for $name {
                const NAME: &'static str = $wire;

                #[allow(unused_mut)]
                fn props(&self) -> Vec<(&'static str, $crate::diagnostics::analytics::event::Prop)> {
                    let mut props = Vec::new();
                    $(
                        if let Some(prop) =
                            $crate::diagnostics::analytics::schema::Field::prop(&self.$field)
                        {
                            props.push((stringify!($field), prop));
                        }
                    )*
                    props
                }
            }
        )*

        /// Every catalog event, for the generated docs and schema tests.
        pub fn catalog() -> Vec<$crate::diagnostics::analytics::schema::EventSchema> {
            vec![$(
                $crate::diagnostics::analytics::schema::EventSchema {
                    name: $wire,
                    doc: concat!($($doc, "\n",)*),
                    fields: vec![$(
                        $crate::diagnostics::analytics::schema::FieldSchema {
                            name: stringify!($field),
                            ty: <$ty as $crate::diagnostics::analytics::schema::Field>::schema(),
                            doc: concat!($($fdoc, " ",)*),
                        }
                    ),*],
                }
            ),*]
        }
    };
}
pub use crate::__analytics_events as events;

/// Render the catalog as Markdown (`docs/analytics-events.md`).
pub fn render_catalog(events: &[EventSchema], raw: &[(&str, &str)]) -> String {
    let clean = |text: &str| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .replace('|', "\\|")
    };
    let mut out = String::from(
        "<!-- Generated from src/diagnostics/analytics/catalog.rs by the\n     \
         `catalog_doc_is_current` test. Do not edit: change the catalog, then run\n     \
         FILEX_BLESS=1 cargo test --lib catalog_doc_is_current -->\n\n\
         # Analytics event catalog\n\n\
         Every product event filex can send. Types: `int`, `ms` (milliseconds), \
         `bool`, a list of allowed labels, `id` (a fixed id from a source enum), \
         `[…]` a list, `{…: int}` a tally, `?` optional. Design and privacy rules: \
         [design-telemetry.md](design-telemetry.md).\n\n",
    );
    for event in events {
        out.push_str(&format!("## `{}`\n\n{}\n\n", event.name, clean(event.doc)));
        if event.fields.is_empty() {
            out.push_str("No properties.\n\n");
            continue;
        }
        out.push_str("| Property | Type | Meaning |\n|---|---|---|\n");
        for field in &event.fields {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                field.name,
                field.ty.render(),
                clean(field.doc)
            ));
        }
        out.push('\n');
    }
    out.push_str("## Events outside the typed catalog\n\n");
    for (name, doc) in raw {
        out.push_str(&format!("- `{name}`: {doc}\n"));
    }
    out
}
