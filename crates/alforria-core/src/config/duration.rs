//! The one duration type of the config (alforria extension).
//!
//! Two notations exist, because two families of settings were written at
//! different times and both are in people's files:
//!
//! * [`Millis`] — the host settings (`server.*`, `hook.*`, `triggers.*`):
//!   `"500ms"`, `"30s"`, `"1.5s"`, `"5m"`, `"2h"`, `"1d"`, or a bare number
//!   of milliseconds (upstream's convention for timeouts). Written back as
//!   a number of milliseconds.
//! * [`Calendar`] — `storage.retention.*` and `alforria db prune`: a whole
//!   number and a unit, `45s`, `90m`, `12h`, `30d`, `2w`. Written back as
//!   such a string.
//!
//! Each field keeps exactly the notation it had: what a config file may say
//! and what the server writes are unchanged. What is shared is the type,
//! the parser and the value.

use std::marker::PhantomData;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// How a duration setting is written.
pub trait Notation: Copy + Eq + std::fmt::Debug + 'static {
    /// Unit names and their length in milliseconds.
    const UNITS: &'static [(&'static str, u64)];
    /// Whether the amount may have a fraction (`1.5s`); otherwise it is a
    /// whole number and the result must fit whole seconds without overflow.
    const FRACTIONS: bool;
    /// Whether a bare JSON number (of milliseconds) is accepted.
    const BARE_MILLIS: bool;
    /// The error for a string that is not a duration.
    fn invalid(text: &str) -> String;
    /// The error for a value that is neither an accepted string nor number.
    const EXPECTED: &'static str;
    fn write<S: Serializer>(duration: Duration, serializer: S) -> Result<S::Ok, S::Error>;
}

/// A duration in the config, in notation `N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DurationOf<N: Notation>(pub Duration, PhantomData<N>);

impl<N: Notation> DurationOf<N> {
    pub fn new(duration: Duration) -> Self {
        DurationOf(duration, PhantomData)
    }

    pub fn get(&self) -> Duration {
        self.0
    }

    pub fn as_millis(self) -> u64 {
        self.0.as_millis() as u64
    }

    /// Parse the string form; `None` when the text is not a duration in
    /// this notation.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let split = text
            .find(|c: char| !(c.is_ascii_digit() || (N::FRACTIONS && c == '.')))
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(split);
        let unit = unit.trim();
        let per_unit = N::UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, millis)| *millis)?;
        let millis = if N::FRACTIONS {
            let number: f64 = number.parse().ok()?;
            if !number.is_finite() || number < 0.0 {
                return None;
            }
            (number * per_unit as f64).round() as u64
        } else {
            // Whole seconds, as these were always kept: an amount whose
            // seconds overflow is not a duration.
            let amount: u64 = number.parse().ok()?;
            return Some(Self::new(Duration::from_secs(
                amount.checked_mul(per_unit / 1000)?,
            )));
        };
        Some(Self::new(Duration::from_millis(millis)))
    }
}

impl<N: Notation> Serialize for DurationOf<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        N::write(self.0, serializer)
    }
}

impl<'de, N: Notation> Deserialize<'de> for DurationOf<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        match &value {
            serde_json::Value::Number(number) if N::BARE_MILLIS => number
                .as_u64()
                .map(|millis| Self::new(Duration::from_millis(millis)))
                .ok_or_else(|| {
                    serde::de::Error::custom("expected a non-negative number of milliseconds")
                }),
            serde_json::Value::String(text) => {
                Self::parse(text).ok_or_else(|| serde::de::Error::custom(N::invalid(text)))
            }
            _ => Err(serde::de::Error::custom(N::EXPECTED)),
        }
    }
}

/// `"500ms"`, `"30s"`, `"5m"`, `"2h"`, `"1d"`, fractions allowed, or a bare
/// number of milliseconds; written as a number of milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Millis;

impl Notation for Millis {
    const UNITS: &'static [(&'static str, u64)] = &[
        ("ms", 1),
        ("s", 1_000),
        ("sec", 1_000),
        ("secs", 1_000),
        ("m", 60_000),
        ("min", 60_000),
        ("mins", 60_000),
        ("h", 3_600_000),
        ("hr", 3_600_000),
        ("hrs", 3_600_000),
        ("d", 86_400_000),
        ("day", 86_400_000),
        ("days", 86_400_000),
    ];
    const FRACTIONS: bool = true;
    const BARE_MILLIS: bool = true;
    const EXPECTED: &'static str =
        "expected a duration string such as \"30s\" or a number of milliseconds";

    fn invalid(text: &str) -> String {
        format!("invalid duration {text:?}: expected a number followed by ms, s, m, h or d")
    }

    fn write<S: Serializer>(duration: Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(duration.as_millis() as u64)
    }
}

/// A whole number and a unit — `45s`, `90m`, `12h`, `30d`, `2w` — written
/// back the same way, in the largest unit that divides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calendar;

impl Notation for Calendar {
    const UNITS: &'static [(&'static str, u64)] = &[
        ("s", 1_000),
        ("m", 60_000),
        ("h", 3_600_000),
        ("d", 86_400_000),
        ("w", 604_800_000),
    ];
    const FRACTIONS: bool = false;
    const BARE_MILLIS: bool = false;
    const EXPECTED: &'static str = "expected a duration such as \"30d\", \"12h\" or \"90m\"";

    fn invalid(text: &str) -> String {
        format!("expected a duration such as \"30d\", \"12h\" or \"90m\", got {text:?}")
    }

    fn write<S: Serializer>(duration: Duration, serializer: S) -> Result<S::Ok, S::Error> {
        let seconds = duration.as_secs();
        let text = [
            (7 * 24 * 60 * 60, "w"),
            (24 * 60 * 60, "d"),
            (60 * 60, "h"),
            (60, "m"),
        ]
        .iter()
        .find(|(unit, _)| seconds > 0 && seconds.checked_rem(*unit) == Some(0))
        .map(|(unit, name)| format!("{}{name}", seconds / unit))
        .unwrap_or_else(|| format!("{seconds}s"));
        serializer.serialize_str(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type Host = DurationOf<Millis>;
    type Storage = DurationOf<Calendar>;

    /// One type, two notations, each accepting and writing exactly what
    /// its settings always did.
    #[test]
    fn each_notation_accepts_and_writes_what_it_always_did() {
        let host = |value: serde_json::Value| {
            serde_json::from_value::<Host>(value)
                .ok()
                .map(|d| d.as_millis())
        };
        let storage = |value: serde_json::Value| {
            serde_json::from_value::<Storage>(value)
                .ok()
                .map(|d| d.as_millis())
        };
        for (text, millis) in [
            ("500ms", 500),
            ("30s", 30_000),
            ("1.5s", 1_500),
            ("5m", 300_000),
            ("2 hrs", 7_200_000),
            ("1d", 86_400_000),
            (" 10 s ", 10_000),
            ("3mins", 180_000),
        ] {
            assert_eq!(host(json!(text)), Some(millis), "{text}");
        }
        assert_eq!(host(json!(1500)), Some(1_500));
        for bad in [
            json!("2w"),
            json!("10"),
            json!("-1s"),
            json!("s"),
            json!(-5),
            json!(1.5),
            json!(true),
        ] {
            assert_eq!(host(bad.clone()), None, "{bad}");
        }
        for (text, millis) in [
            ("45s", 45_000),
            ("90m", 5_400_000),
            ("12h", 43_200_000),
            ("30d", 2_592_000_000),
            ("2w", 1_209_600_000),
            (" 12 h ", 43_200_000),
        ] {
            assert_eq!(storage(json!(text)), Some(millis), "{text}");
        }
        for bad in [
            json!("500ms"),
            json!("1.5h"),
            json!("3mins"),
            json!("12"),
            json!("h"),
            json!(1500),
            json!("99999999999999999999d"),
            json!("18446744073709551615w"),
        ] {
            assert_eq!(storage(bad.clone()), None, "{bad}");
        }
        // Written back as each always was.
        assert_eq!(json!(Host::parse("1.5s").unwrap()), json!(1500));
        for (text, written) in [
            ("14d", "2w"),
            ("36h", "36h"),
            ("90m", "90m"),
            ("0s", "0s"),
            ("61s", "61s"),
        ] {
            assert_eq!(json!(Storage::parse(text).unwrap()), json!(written));
        }
    }
}
