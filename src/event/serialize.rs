//! Forward-compatible serde glue for [`Event`].
//!
//! `serde`'s derived impls for the closed `KnownEvent` enum give us clean,
//! fast code for the types we know. The open `Event` enum needs three things
//! on top of that:
//!
//! 1. **Serialize** an `Unknown` as the exact bytes it was parsed from.
//! 2. **Deserialize** without knowing the tag up front: capture the raw JSON
//!    first, read `type`, then dispatch.
//! 3. **Never mislabel.** If a *known* tag fails to parse we surface the error
//!    rather than silently downgrading to `Unknown`, because that means either
//!    corruption or a breaking change that should have bumped `version`.

use super::model::{Event, KnownEvent};
use serde::{Deserialize, Serialize};

/// Every `type` tag this build implements.
pub const KNOWN_TAGS: &[&str] = &[
    "meta",
    "session_start",
    "output",
    "input",
    "resize",
    "command",
    "exit",
    "mark",
    "signal",
    "mouse",
    "session_end",
];

impl Serialize for Event {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // `RawValue` writes its bytes verbatim: byte-exact round-trip.
            Event::Unknown(raw) => raw.serialize(s),
            known => KnownEvent::from(known.clone()).serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Event, D::Error> {
        let raw: Box<serde_json::value::RawValue> = Deserialize::deserialize(d)?;
        from_raw(&raw).map_err(serde::de::Error::custom)
    }
}

/// Turn raw JSON into an [`Event`], preserving unknown tags verbatim.
pub fn from_raw(raw: &serde_json::value::RawValue) -> Result<Event, UnknownEventError> {
    match serde_json::from_str::<KnownEvent>(raw.get()) {
        Ok(known) => Ok(Event::from(known)),
        // A tag we do not implement: not an error, just not ours to interpret.
        Err(_) if !is_known_tag(raw.get()) => Ok(Event::unknown(raw.get()).expect("raw is valid JSON")),        // A tag we *do* implement that will not parse is a real problem.
        Err(e) => Err(UnknownEventError { tag: tag_of(raw.get()), source: e }),
    }
}

fn is_known_tag(json: &str) -> bool {
    match tag_of(json) {
        Some(tag) => KNOWN_TAGS.contains(&tag.as_str()),
        None => false,
    }
}

/// Extract the `type` field without decoding the whole object.
fn tag_of(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get("type")?.as_str().map(str::to_owned)
}

/// A known-tagged event that failed to decode.
#[derive(Debug, thiserror::Error)]
#[error("malformed `{}` event: {source}", tag.as_deref().unwrap_or("<untagged>"))]
pub struct UnknownEventError {
    pub tag: Option<String>,
    #[source]
    pub source: serde_json::Error,
}

/// Serialise one event as a single line of JSON.
pub fn to_json_line(event: &Event) -> Result<String, serde_json::Error> {
    serde_json::to_string(event)
}

/// Parse one line of JSON into an [`Event`].
pub fn from_json_line(line: &str) -> Result<Event, UnknownEventError> {
    let raw: Box<serde_json::value::RawValue> = serde_json::from_str(line)
        .map_err(|source| UnknownEventError { tag: None, source })?;
    from_raw(&raw)
}

#[cfg(test)]
mod tests {
    use super::super::model::*;
    use super::*;

    #[test]
    fn unknown_tags_round_trip_byte_exactly() {
        // A hypothetical eye 2.0 event. Whitespace and key order must survive.
        let line = r#"{ "type": "hyperlink", "t": 1.5, "url": "https://example.com" }"#;
        let ev = from_json_line(line).unwrap();
        assert!(ev.is_unknown());
        assert_eq!(ev.raw(), Some(line));
        assert_eq!(ev.kind(), "unknown");
        assert_eq!(to_json_line(&ev).unwrap(), line);
    }

    #[test]
    fn unknown_tags_are_readable_without_serde_fields() {
        let line = r#"{"type":"gpu_frame","t":0.25,"frame":9001}"#;
        let ev = from_json_line(line).unwrap();
        assert!(ev.is_unknown());
        // ...and still carry whatever fields a future consumer wants.
        let v: serde_json::Value = serde_json::from_str(ev.raw().unwrap()).unwrap();
        assert_eq!(v["frame"], 9001);
    }

    #[test]
    fn known_events_round_trip() {
        let ev = Event::Command {
            t: 10.231,
            id: 3,
            text: "cargo build".into(),
            source: CommandSource::Integration,
        };
        let line = to_json_line(&ev).unwrap();
        assert_eq!(
            line,
            r#"{"type":"command","t":10.231,"id":3,"text":"cargo build","source":"integration"}"#
        );
        assert_eq!(from_json_line(&line).unwrap(), ev);
    }

    #[test]
    fn corrupt_payload_in_a_known_tag_is_an_error_not_silent_unknown() {
        let line = r#"{"type":"output","t":1.0,"data":"!!!not base64!!!"}"#;
        let err = from_json_line(line).unwrap_err();
        assert_eq!(err.tag.as_deref(), Some("output"));
    }

    #[test]
    fn missing_type_tag_is_treated_as_unknown() {
        let ev = from_json_line(r#"{"t":1.0}"#).unwrap();
        assert!(ev.is_unknown());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(from_json_line("{not json").is_err());
    }
}
