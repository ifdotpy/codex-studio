//! Projections of the native CLI's line-list and log JSON responses.
use serde::Deserialize;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Text {
    String(String),
    Other(IgnoredAny),
    #[default]
    Missing,
}
impl Text {
    fn get(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Time {
    Integer(i64),
    Other(IgnoredAny),
    #[default]
    Missing,
}
impl Time {
    fn get(&self) -> i64 {
        match self {
            Self::Integer(value) => *value,
            _ => 0,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum Caches {
    Object(BTreeMap<String, IgnoredAny>),
    Other(IgnoredAny),
    #[default]
    Missing,
}
impl Caches {
    fn has_entries(&self) -> bool {
        matches!(self, Self::Object(entries) if !entries.is_empty())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(crate) struct Line {
    name: Text,
    id: Text,
    head: Text,
}
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum LineEntry {
    Line(#[serde(deserialize_with = "object")] Line),
    Other(IgnoredAny),
}
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Lines {
    Array(Vec<LineEntry>),
    Other(IgnoredAny),
}
impl Lines {
    fn main(&self) -> Option<&Line> {
        match self {
            Self::Array(lines) => lines.iter().find_map(|entry| match entry {
                LineEntry::Line(line) if line.name.get() == Some("main") => Some(line),
                _ => None,
            }),
            Self::Other(_) => None,
        }
    }
    pub(super) fn main_id(&self) -> Option<&str> {
        self.main()?.id.get()
    }
    pub(crate) fn main_head(&self) -> Option<&str> {
        self.main()?.head.get()
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct State {
    id: Text,
    kind: Text,
    line: Text,
    time: Time,
    layers: Caches,
    caches: Caches,
}
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum StateEntry {
    State(#[serde(deserialize_with = "object")] State),
    Other(IgnoredAny),
}
impl StateEntry {
    pub(super) fn id(&self) -> Option<&str> {
        match self {
            Self::State(state) => state.id.get(),
            Self::Other(_) => None,
        }
    }
    pub(super) fn has_caches(&self) -> bool {
        match self {
            Self::State(state) => match &state.layers {
                Caches::Missing => state.caches.has_entries(),
                layers => layers.has_entries(),
            },
            Self::Other(_) => false,
        }
    }
    pub(super) fn matches_line(&self, line: &str) -> bool {
        matches!(self, Self::State(state) if state.kind.get() == Some("auto") && state.line.get() == Some(line))
    }
    pub(super) fn order(&self) -> (i64, &str) {
        match self {
            Self::State(state) => (state.time.get(), state.id.get().unwrap_or_default()),
            Self::Other(_) => (0, ""),
        }
    }
}

// Serde also accepts sequences for structs by default; the CLI emits objects only.
fn object<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Object<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
        type Value = T;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an object")
        }
        fn visit_map<M: MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_map(Object(std::marker::PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_lines_ignore_nonobjects_and_keep_first_main() {
        let lines: Lines = serde_json::from_str(r#"[["main","wrong","wrong"],null,{"name":"other"},{"name":"main","id":false,"head":"saved"},{"name":"main","id":"later"}]"#).unwrap();
        assert_eq!(lines.main_id(), None);
        assert_eq!(lines.main_head(), Some("saved"));
        let other: Lines = serde_json::from_str("{}").unwrap();
        assert_eq!(other.main_head(), None);
    }
    #[test]
    fn state_cache_field_presence_takes_precedence() {
        for (json, expected) in [
            (r#"{"id":"x","caches":{"target":"x"}}"#, true),
            (r#"{"id":"x","layers":{},"caches":{"target":"x"}}"#, false),
            (r#"{"id":"x","layers":null,"caches":{"target":"x"}}"#, false),
            (r#"{"id":"x","layers":{"target":false}}"#, true),
            (r#"["x","auto","main",1]"#, false),
        ] {
            let state: StateEntry = serde_json::from_str(json).unwrap();
            assert_eq!(state.has_caches(), expected, "{json}");
        }
    }
}
