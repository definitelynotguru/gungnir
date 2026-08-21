//! Entry identity: ULID-based, time-sortable, filesystem-safe.

use std::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

/// Unique, lexicographically time-sortable entry identifier.
///
/// ULIDs are 128-bit: 48-bit millisecond timestamp + 80 bits of randomness.
/// Collisions are practically impossible, so unlike random-hex schemes no
/// collision-retry loop is needed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(Ulid);

impl EntryId {
    /// Generate an id stamped with the current time.
    pub fn generate() -> Self {
        Self(Ulid::new())
    }

    /// The creation timestamp embedded in the id (millisecond precision).
    pub fn timestamp_ms(&self) -> u64 {
        self.0.timestamp_ms()
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Crockford base32, 26 chars: [0-9A-HJKMNP-TV-Z], no I/L/O/U.
        write!(f, "{}", self.0)
    }
}

impl FromStr for EntryId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ulid::from_string(s).map(Self)
    }
}

impl Serialize for EntryId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EntryId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_str(&s).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_string() {
        let id = EntryId::generate();
        let parsed: EntryId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn sorts_chronologically() {
        let a = EntryId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = EntryId::generate();
        assert!(a < b);
    }

    #[test]
    fn rejects_garbage() {
        assert!("not-an-id".parse::<EntryId>().is_err());
    }
}
