use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

const UUID_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CaudraIdParseError {
    #[error("empty id")]
    Empty,
    #[error("invalid base58 character {0:?} at {1}")]
    InvalidBase58(char, usize),
    #[error("base58 string has a length that cannot decode to whole bytes")]
    InvalidBase58Length,
    #[error("id decoded to {0} bytes, expected {UUID_BYTES}")]
    InvalidByteLen(usize),
}

/// The canonical unique id for anything in caudra (sessions, and message
/// nodes once history is a tree): time-ordered, base58-encoded, backed by a
/// UUIDv7.
///
/// Serializes as base58, which is variable-length (21-22 chars for 16 bytes).
/// v7 ids encode to a stable 21 chars, so lexical sort orders them
/// chronologically. Nothing in caudra sorts by the string form today; storage
/// uses the embedded timestamp directly. See issue #264 for future
/// tree-ordered history work.
///
/// Ids compare by their bytes, which is the order this process generated
/// them in, so "after a message" is `id > message`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CaudraId([u8; UUID_BYTES]);

impl CaudraId {
    #[allow(clippy::disallowed_methods)]
    pub fn generate() -> Self {
        Self(Uuid::now_v7().into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; UUID_BYTES] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; UUID_BYTES]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for CaudraId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(&self.0).into_string())
    }
}

impl FromStr for CaudraId {
    type Err = CaudraIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(CaudraIdParseError::Empty);
        }
        decode_base58(s)
    }
}

fn decode_base58(s: &str) -> Result<CaudraId, CaudraIdParseError> {
    let bytes = bs58::decode(s).into_vec().map_err(|e| match e {
        bs58::decode::Error::InvalidCharacter { character, index } => {
            CaudraIdParseError::InvalidBase58(character, index)
        }
        bs58::decode::Error::NonAsciiCharacter { index } => {
            CaudraIdParseError::InvalidBase58('\u{FFFD}', index)
        }
        _ => CaudraIdParseError::InvalidBase58Length,
    })?;
    if bytes.len() != UUID_BYTES {
        return Err(CaudraIdParseError::InvalidByteLen(bytes.len()));
    }
    let mut arr = [0u8; UUID_BYTES];
    arr.copy_from_slice(&bytes);
    Ok(CaudraId(arr))
}

impl Serialize for CaudraId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CaudraId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A reference to a session as provided at an application boundary (ACP,
/// session resume, SDK mode).
///
/// Preserves the caller's exact string verbatim (legacy hex ids resume
/// unchanged) so wire echo and client correlation hold. The parsed [`CaudraId`]
/// is cached so [`id`](Self::id) is infallible. Canonical when self-generated
/// via [`from_id`](Self::from_id) (base58).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionRef {
    id: CaudraId,
    raw: String,
}

impl SessionRef {
    pub fn from_id(id: CaudraId) -> Self {
        Self {
            id,
            raw: id.to_string(),
        }
    }

    pub fn generate() -> Self {
        Self::from_id(CaudraId::generate())
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn id(&self) -> CaudraId {
        self.id
    }
}

impl From<CaudraId> for SessionRef {
    fn from(id: CaudraId) -> Self {
        Self::from_id(id)
    }
}

impl fmt::Display for SessionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for SessionRef {
    type Err = CaudraIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s.parse::<CaudraId>()?;
        Ok(Self {
            id,
            raw: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for SessionRef {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for SessionRef {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    /// Enough to generate many ids within one millisecond, where only the
    /// generator's counter keeps them in order.
    const MONOTONIC_SAMPLES: usize = 10_000;
    const NOT_MONOTONIC: &str = "ids generated in sequence must sort in that sequence";

    fn parse(s: &str) -> CaudraId {
        s.parse().unwrap()
    }

    #[test]
    fn generate_is_v7() {
        let id = CaudraId::generate();
        let uuid = Uuid::from_bytes(id.0);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
    }

    /// A change record is selected by comparing its id with a message's, which
    /// holds only while ids generated in one process never go backwards.
    #[test]
    fn generate_is_strictly_increasing() {
        let ids: Vec<CaudraId> = (0..MONOTONIC_SAMPLES)
            .map(|_| CaudraId::generate())
            .collect();
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "{NOT_MONOTONIC}"
        );
    }

    #[test]
    fn roundtrip_base58() {
        let id = CaudraId::generate();
        let s = id.to_string();
        assert!((21..=22).contains(&s.len()));
        assert_eq!(s.parse::<CaudraId>().unwrap(), id);
    }

    #[test_case([0; UUID_BYTES] ; "all zero bytes")]
    #[test_case([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1] ; "leading zero bytes")]
    fn roundtrips_leading_zero_bytes(bytes: [u8; UUID_BYTES]) {
        let id = CaudraId::from_bytes(bytes);
        assert_eq!(parse(&id.to_string()), id);
    }

    #[test_case("01965087-4c71-7f00-8000-000000000000")]
    #[test_case("019650874c717f008000000000000000")]
    fn rejects_uuid_hex(s: &str) {
        assert!(s.parse::<CaudraId>().is_err());
    }

    #[test_case("" => matches Err(CaudraIdParseError::Empty))]
    #[test_case("O" => matches Err(CaudraIdParseError::InvalidBase58('O', 0)))]
    #[test_case("2j87v4grC" => matches Err(CaudraIdParseError::InvalidByteLen(_)))]
    fn rejects_bad(s: &str) -> Result<CaudraId, CaudraIdParseError> {
        s.parse()
    }

    #[test]
    fn serde_keyed_base58() {
        let id = CaudraId::generate();
        let s = serde_json::to_string(&id).unwrap();
        assert!((23..=24).contains(&s.len()));
        let back: CaudraId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn ref_round_trips_through_its_string_form() {
        let session_ref = SessionRef::generate();
        assert_eq!(
            session_ref.as_str().parse::<SessionRef>().unwrap(),
            session_ref
        );
    }

    #[test]
    fn ref_from_id_is_canonical_base58() {
        let id = CaudraId::generate();
        let session_ref = SessionRef::from(id);
        assert_eq!(session_ref.as_str(), id.to_string());
        assert_eq!(session_ref.id(), id);
    }
}
