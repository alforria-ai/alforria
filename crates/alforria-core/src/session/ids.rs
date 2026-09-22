//! `ses_`/`msg_`/`prt_`/`per_`/`que_` IDs — port of `id/id.ts` and
//! `session/schema.ts`.
//!
//! TS composes `<prefix>_<48-bit hex time><14 chars base62>` from a
//! `timestamp << 12 + counter` — the counter makes ids created in the
//! same millisecond monotonically ordered (id/id.ts:49-70). The Rust
//! port keeps the ULID string format (`prefix_` + 26 Crockford chars)
//! but embeds the same `timestamp << 12 + counter` in the ULID's 48-bit
//! time field, so same-millisecond ordering matches TS. Given IDs are
//! validated for the prefix, else
//! `ID {given} does not start with {prefix}` (id/id.ts:64-68).

use ulid::Ulid;

use crate::CoreError;

/// `lastTimestamp`/`counter` (id/id.ts:15-17) — the per-process monotonic
/// id state.
static ID_STATE: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0));

/// `create`'s time computation (id/id.ts:49-55): reset the counter when
/// the millisecond changes, then embed `timestamp * 0x1000 + counter`.
fn monotonic_time(now_ms: u64) -> u64 {
    let mut state = ID_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.0 != now_ms {
        *state = (now_ms, 0);
    }
    state.1 += 1;
    now_ms * 0x1000 + state.1
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// `prefixes` (id/id.ts:3-13) — the prefixes M5 needs. Aliases of the
/// generated `PREFIX` consts below.
pub mod prefix {
    pub const SESSION: &str = super::SessionId::PREFIX;
    pub const MESSAGE: &str = super::MessageId::PREFIX;
    pub const PART: &str = super::PartId::PREFIX;
    pub const PERMISSION: &str = super::PermissionId::PREFIX;
    pub const QUESTION: &str = super::QuestionId::PREFIX;
}

fn create(prefix: &str, direction: Direction, given: Option<&str>) -> Result<String, CoreError> {
    if let Some(given) = given {
        if !given.starts_with(prefix) {
            return Err(CoreError::Storage(format!(
                "ID {} does not start with {}",
                given, prefix
            )));
        }
        return Ok(given.to_string());
    }
    let value = Ulid::new().0;
    let random = value & ((1u128 << 80) - 1);
    let time = monotonic_time(now_ms()) & 0xFFFF_FFFF_FFFF;
    let time = match direction {
        Direction::Ascending => time,
        Direction::Descending => !time & 0xFFFF_FFFF_FFFF,
    };
    let combined = ((time as u128) << 80) | random;
    Ok(format!("{prefix}{}", Ulid(combined)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Ascending,
    Descending,
}

macro_rules! id_type {
    ($name:ident, $prefix:expr, $doc:literal) => {
        #[doc = $doc]
        #[allow(non_snake_case)]
        pub mod $name {
            /// The prefix every ID of this kind starts with.
            pub const PREFIX: &str = $prefix;

            /// `Identifier.ascending(prefix, given)` (id/id.ts:24).
            pub fn ascending(given: Option<&str>) -> Result<String, super::CoreError> {
                super::create($prefix, super::Direction::Ascending, given)
            }

            /// `Identifier.descending(prefix, given)` (id/id.ts:29).
            pub fn descending(given: Option<&str>) -> Result<String, super::CoreError> {
                super::create($prefix, super::Direction::Descending, given)
            }

            /// `generateID` — no `given` (id/id.ts:31-40).
            pub fn generate() -> String {
                super::create($prefix, super::Direction::Ascending, None).unwrap()
            }
        }
    };
}

id_type!(SessionId, "ses_", "`ses_` IDs (`session` prefix).");
id_type!(MessageId, "msg_", "`msg_` IDs (`message` prefix).");
id_type!(PartId, "prt_", "`prt_` IDs (`part` prefix).");
id_type!(PermissionId, "per_", "`per_` IDs (`permission` prefix).");

/// `generateID` for prefixes without a dedicated type (id/id.ts:31-40).
pub fn generate_id(prefix: &str) -> String {
    create(prefix, Direction::Ascending, None).expect("generated id is prefix-valid")
}
id_type!(QuestionId, "que_", "`que_` IDs (`question` prefix).");
id_type!(JobId, "job_", "`job_` IDs (`job` prefix).");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_match_ts() {
        assert_eq!(SessionId::PREFIX, "ses_");
        assert_eq!(MessageId::PREFIX, "msg_");
        assert_eq!(PartId::PREFIX, "prt_");
        assert_eq!(PermissionId::PREFIX, "per_");
        assert_eq!(QuestionId::PREFIX, "que_");
    }

    #[test]
    fn ascending_sorts_after_earlier_ids() {
        let a = MessageId::ascending(None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MessageId::ascending(None).unwrap();
        assert!(a < b, "{a} should sort before {b}");
        assert!(a.starts_with("msg_"));
    }

    #[test]
    fn ascending_is_monotonic_within_a_millisecond() {
        let a = MessageId::ascending(None).unwrap();
        let b = MessageId::ascending(None).unwrap();
        let c = MessageId::ascending(None).unwrap();
        assert!(
            a < b && b < c,
            "same-millisecond ids sort in creation order"
        );
    }

    #[test]
    fn descending_sorts_before_earlier_ids() {
        let a = SessionId::descending(None).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = SessionId::descending(None).unwrap();
        assert!(a > b, "{a} should sort after {b}");
    }

    #[test]
    fn given_ids_are_validated_for_prefix() {
        assert_eq!(
            MessageId::ascending(Some("msg_01J")).unwrap(),
            "msg_01J".to_string()
        );
        let err = MessageId::ascending(Some("prt_01J")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "StorageError: ID prt_01J does not start with msg_"
        );
        let err = SessionId::descending(Some("")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "StorageError: ID  does not start with ses_"
        );
    }
}
