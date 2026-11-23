//! A UTC RFC3339 timestamp shared by every favetto client.
//!
//! [`Timestamp`] is a thin newtype around [`time::OffsetDateTime`] that always
//! stores a UTC instant and serializes as an RFC3339 string (`...Z`). It exists
//! so the wire format lives in exactly one place: `time`'s own `serde` support
//! would emit a nine-element sequence (or a non-RFC3339 human-readable form),
//! which would break every existing JSON/MessagePack consumer.

use std::fmt;
use std::ops::{Add, AddAssign, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// A UTC instant serialized as an RFC3339 string.
///
/// The inner [`OffsetDateTime`] is always normalized to [`time::UtcOffset::UTC`],
/// so two timestamps that name the same instant compare equal and serialize
/// identically regardless of the offset they were parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    /// The Unix epoch, `1970-01-01T00:00:00Z`.
    pub const UNIX_EPOCH: Self = Self(OffsetDateTime::UNIX_EPOCH);

    /// The current instant from the system (or browser) clock, in UTC.
    pub fn now() -> Self {
        Self::new(OffsetDateTime::now_utc())
    }

    /// Wrap an [`OffsetDateTime`], normalizing it to UTC.
    pub fn new(datetime: OffsetDateTime) -> Self {
        Self(datetime.to_offset(time::UtcOffset::UTC))
    }

    /// Build a timestamp from Unix epoch milliseconds.
    ///
    /// Returns `None` when the value is outside `time`'s representable range.
    pub fn from_unix_timestamp_millis(millis: i64) -> Option<Self> {
        let nanos = i128::from(millis).checked_mul(1_000_000)?;
        Self::from_unix_timestamp_nanos(nanos)
    }

    /// Build a timestamp from Unix epoch seconds.
    ///
    /// Returns `None` when the value is outside `time`'s representable range.
    pub fn from_unix_timestamp(seconds: i64) -> Option<Self> {
        OffsetDateTime::from_unix_timestamp(seconds)
            .ok()
            .map(Self::new)
    }

    /// Build a timestamp from Unix epoch nanoseconds.
    ///
    /// Returns `None` when the value is outside `time`'s representable range.
    pub fn from_unix_timestamp_nanos(nanos: i128) -> Option<Self> {
        OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .ok()
            .map(Self::new)
    }

    /// Unix epoch milliseconds for this instant.
    pub fn unix_timestamp_millis(self) -> i64 {
        (self.0.unix_timestamp_nanos().div_euclid(1_000_000)) as i64
    }

    /// Unix epoch seconds for this instant (truncating sub-second precision).
    pub fn unix_timestamp(self) -> i64 {
        self.0.unix_timestamp()
    }

    /// The wrapped UTC [`OffsetDateTime`].
    pub fn to_offset_datetime(self) -> OffsetDateTime {
        self.0
    }
}

impl Default for Timestamp {
    fn default() -> Self {
        Self::UNIX_EPOCH
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.format(&Rfc3339) {
            Ok(s) => f.write_str(&s),
            Err(_) => Err(fmt::Error),
        }
    }
}

impl FromStr for Timestamp {
    type Err = time::error::Parse;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        OffsetDateTime::parse(s, &Rfc3339).map(Self::new)
    }
}

impl Serialize for Timestamp {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let formatted = self.0.format(&Rfc3339).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&formatted)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

impl From<OffsetDateTime> for Timestamp {
    fn from(datetime: OffsetDateTime) -> Self {
        Self::new(datetime)
    }
}

impl From<Timestamp> for OffsetDateTime {
    fn from(timestamp: Timestamp) -> Self {
        timestamp.0
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Duration) -> Self::Output {
        Timestamp(self.0 + rhs)
    }
}

impl AddAssign<Duration> for Timestamp {
    fn add_assign(&mut self, rhs: Duration) {
        self.0 += rhs;
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: Duration) -> Self::Output {
        Timestamp(self.0 - rhs)
    }
}

impl SubAssign<Duration> for Timestamp {
    fn sub_assign(&mut self, rhs: Duration) {
        self.0 -= rhs;
    }
}

impl Sub<Timestamp> for Timestamp {
    type Output = Duration;

    fn sub(self, rhs: Timestamp) -> Self::Output {
        self.0 - rhs.0
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Timestamp {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Timestamp".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "format": "date-time",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_as_rfc3339_utc() {
        let ts = Timestamp::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(ts.to_string(), "2023-11-14T22:13:20Z");
        assert_eq!(
            serde_json::to_string(&ts).unwrap(),
            "\"2023-11-14T22:13:20Z\""
        );
    }

    #[test]
    fn parses_z_and_numeric_offsets_to_the_same_instant() {
        let z: Timestamp = "2023-11-14T22:13:20Z".parse().unwrap();
        let offset: Timestamp = "2023-11-14T23:13:20+01:00".parse().unwrap();
        assert_eq!(z, offset);
        assert_eq!(z.to_string(), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn round_trips_fractional_seconds_in_json() {
        for text in [
            "2023-11-14T22:13:20.100Z",
            "2023-11-14T22:13:20.120Z",
            "2023-11-14T22:13:20.123456Z",
        ] {
            let ts: Timestamp = serde_json::from_str(&format!("{text:?}")).unwrap();
            let back: Timestamp =
                serde_json::from_str(&serde_json::to_string(&ts).unwrap()).unwrap();
            assert_eq!(ts, back);
        }
    }

    #[test]
    fn round_trips_optional_timestamps_in_json() {
        let some = Some(Timestamp::from_unix_timestamp(1_700_000_000).unwrap());
        let json = serde_json::to_string(&some).unwrap();
        assert_eq!(json, "\"2023-11-14T22:13:20Z\"");
        let back: Option<Timestamp> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, some);

        let none: Option<Timestamp> = None;
        assert_eq!(serde_json::to_string(&none).unwrap(), "null");
    }

    #[test]
    fn round_trips_in_messagepack_as_a_string() {
        let ts = Timestamp::from_unix_timestamp_millis(1_700_000_000_123).unwrap();
        let bytes = rmp_serde::to_vec(&ts).unwrap();
        let back: Timestamp = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, ts);
        // The payload is a MessagePack string, not a nine-element sequence.
        assert_eq!(bytes[0] & 0xe0, 0xa0);
    }

    #[test]
    fn round_trips_optional_and_offset_timestamps_in_messagepack() {
        let expected: Timestamp = "2023-11-14T23:13:20.123456+01:00".parse().unwrap();
        let some = Some(expected);
        let bytes = rmp_serde::to_vec(&some).unwrap();
        let back: Option<Timestamp> = rmp_serde::from_slice(&bytes).unwrap();
        let back = back.expect("some timestamp");
        assert_eq!(back, expected);
        assert_eq!(back.to_string(), "2023-11-14T22:13:20.123456Z");

        let none: Option<Timestamp> = None;
        let bytes = rmp_serde::to_vec(&none).unwrap();
        let back: Option<Timestamp> = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, None);
    }

    #[cfg(feature = "schema")]
    #[test]
    fn schema_describes_an_rfc3339_string() {
        let schema = schemars::schema_for!(Timestamp);
        let value = serde_json::to_value(schema).unwrap();
        assert_eq!(value["type"], "string");
        assert_eq!(value["format"], "date-time");
    }

    #[test]
    fn millisecond_conversions_round_trip() {
        let ts = Timestamp::from_unix_timestamp_millis(1_700_000_000_123).unwrap();
        assert_eq!(ts.unix_timestamp_millis(), 1_700_000_000_123);
        assert_eq!(ts.unix_timestamp(), 1_700_000_000);
    }

    #[test]
    fn arithmetic_uses_time_durations() {
        let base = Timestamp::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(
            (base + Duration::hours(1)).to_string(),
            "2023-11-14T23:13:20Z"
        );
        assert_eq!((base + Duration::hours(2) - base).whole_seconds(), 7200);
    }
}
