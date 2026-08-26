//! Date/time helpers over jiff (change `utils-crate`, design D3).
//!
//! The workspace's single date/time seam: it replaces the hand-rolled
//! calendar math that lived in `search` (enricher timestamp
//! normalization, reranker epoch parsing) and `ingestion` (RFC3339 UTC
//! formatting, VACUUM snapshot naming). All calendar computation is
//! delegated to jiff; this module owns the acceptance contract and the
//! rendering.
//!
//! **Byte-identical semantics (design D3).** [`normalize_to_rfc3339`] and
//! [`parse_epoch_seconds`] preserve the exact acceptance set and output of
//! the hand-rolled parser they replace:
//!
//! - fixed-width `YYYY-MM-DD` + `T`/`t`/` ` + `HH:MM:SS` (19 bytes);
//! - optional fractional part `.` + 1..=9 digits, dropped from the output;
//! - `T`/`t` separator: an offset is required — `Z`/`z` (normalized to
//!   `Z`) or `±HH:MM` (hours ≤ 23, minutes ≤ 59), then end of string;
//! - ` ` separator (the SQLite `CURRENT_TIMESTAMP` layout): nothing may
//!   follow the fractional part; the value is interpreted as UTC;
//! - calendar validity (leap days, hour/minute/second ranges) is enforced
//!   by jiff; empty or garbage input yields `None`.
//!
//! jiff's temporal parser is strictly more lenient than that contract
//! (variable-width fields, optional minutes/seconds, colon-less offsets),
//! so [`parse_input`] gates on the exact string shape first and then lets
//! jiff do every calendar computation — no hand-rolled calendar math.

use std::time::{SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use jiff::civil;
use jiff::fmt::temporal::DateTimeParser;
use jiff::tz::Offset;

/// The temporal parser is stateless; build it once.
static PARSER: DateTimeParser = DateTimeParser::new();

/// The current time as RFC 3339 UTC at second precision
/// (`2026-08-26T12:34:56Z`); `None` only if the system clock precedes the
/// Unix epoch.
pub fn now_rfc3339() -> Option<String> {
    format_rfc3339(SystemTime::now())
}

/// Formats `time` as an RFC 3339 UTC string at second precision
/// (`2026-08-23T12:34:56Z`) — the shape of Go's `time.RFC3339` for UTC
/// instants. Returns `None` for instants before the Unix epoch (not
/// representable as a `SystemTime` duration).
pub fn format_rfc3339(time: SystemTime) -> Option<String> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    // `Display` renders RFC 3339 with `Z` and no fractional part for
    // integer-second instants.
    Timestamp::from_second(seconds)
        .ok()
        .map(|ts| ts.to_string())
}

/// Formats `time` as the VACUUM snapshot naming timestamp
/// `%Y-%m-%dT%H-%M-%S-<ms>` (UTC; the ingestion design D6 contract).
///
/// Infallible by contract: instants before the Unix epoch clamp to the
/// epoch (the legacy `duration_since(UNIX_EPOCH).unwrap_or_default()`
/// behavior) and instants outside jiff's representable range clamp to the
/// epoch as well.
pub fn format_backup_stamp(time: SystemTime) -> String {
    let millis = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let seconds = (millis / 1000) as i64;
    let ms = (millis % 1000) as u32;
    let instant = Timestamp::from_second(seconds).unwrap_or(Timestamp::UNIX_EPOCH);
    let civil = Offset::UTC.to_datetime(instant);
    let base = civil.strftime("%Y-%m-%dT%H-%M-%S").to_string();
    format!("{base}-{ms:03}")
}

/// Normalizes a stored timestamp to canonical RFC 3339.
///
/// Accepts RFC 3339 (`2026-08-01T12:00:00Z`, with optional fractional
/// seconds and `±HH:MM` offsets — re-rendered canonically: the fractional
/// part is dropped, lowercase `z` is normalized to `Z`, a zero offset
/// renders as `Z`) and the SQLite `CURRENT_TIMESTAMP` layout
/// (`"2026-08-01 12:00:00"`, interpreted as UTC, with an optional
/// fractional part). Fractional seconds are at most 9 digits and are
/// dropped in the output (as in the oracle's `time.Format(RFC3339)`).
/// Returns `None` for empty or unparseable inputs so the caller can skip
/// the key instead of failing.
pub fn normalize_to_rfc3339(value: &str) -> Option<String> {
    let (civil, offset) = parse_input(value)?;
    let base = civil.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    Some(format!("{base}{}", render_offset(offset)))
}

/// Parses a stored timestamp to seconds since the Unix epoch, with the
/// same acceptance set as [`normalize_to_rfc3339`] (RFC 3339 and the
/// SQLite `CURRENT_TIMESTAMP` layout; fractional seconds are dropped).
/// This is the reranker's freshness/expiry comparison input.
pub fn parse_epoch_seconds(value: &str) -> Option<i64> {
    let (civil, offset) = parse_input(value)?;
    // The civil part is wall-clock; the offset pins it to an instant.
    let instant = offset.to_timestamp(civil).ok()?;
    Some(instant.as_second())
}

/// Renders the offset in canonical RFC 3339 form: `Z` for a zero offset,
/// `±HH:MM` otherwise (the oracle's `time.Format(time.RFC3339)` behavior).
fn render_offset(offset: Offset) -> String {
    if offset.is_zero() {
        return "Z".to_owned();
    }
    let seconds = offset.seconds().unsigned_abs();
    let sign = if offset.is_negative() { '-' } else { '+' };
    format!("{sign}{:02}:{:02}", seconds / 3_600, (seconds % 3_600) / 60)
}

/// Parses `value` into a (civil datetime, offset) pair under the exact
/// acceptance contract documented in the module docs.
fn parse_input(value: &str) -> Option<(civil::DateTime, Offset)> {
    let b = value.as_bytes();
    let end = datetime_shape(b)?;
    match b[10] {
        // SQLite CURRENT_TIMESTAMP layout: UTC, nothing after the
        // fractional part. Only the 19-byte base reaches strptime: the
        // fractional part was shape-validated and is dropped, exactly like
        // the replaced code.
        b' ' if end == b.len() => {
            let civil = civil::DateTime::strptime("%Y-%m-%d %H:%M:%S", &b[..19]).ok()?;
            Some((civil, Offset::UTC))
        }
        // RFC 3339: a required offset, then end of string. jiff extracts
        // the calendar pieces and enforces calendar validity.
        b'T' | b't' if end < b.len() && offset_shape(b, end) => {
            let pieces = PARSER.parse_pieces(value).ok()?;
            let time = pieces.time()?;
            // Drop the fractional part (the replaced code did the same).
            let time = civil::Time::new(time.hour(), time.minute(), time.second(), 0).ok()?;
            let civil = civil::DateTime::from_parts(pieces.date(), time);
            Some((civil, pieces.to_numeric_offset()?))
        }
        // SQLite layout with trailing data, or RFC 3339 without an offset.
        _ => None,
    }
}

/// Validates the fixed-width `YYYY-MM-DD<sep>HH:MM:SS` prefix (separator
/// at byte 10: `T`, `t` or ` `) and the optional fractional part
/// (`.` + 1..=9 digits); returns the position just past the fractional
/// part (or past the seconds).
///
/// This gate preserves the replaced hand-rolled parser's acceptance
/// contract byte-for-byte: jiff's parsers are strictly more lenient
/// (variable-width fields, optional minutes/seconds), so the shape is
/// checked here and all calendar math stays in jiff.
fn datetime_shape(b: &[u8]) -> Option<usize> {
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    for (start, len) in [(0usize, 4), (5, 2), (8, 2), (11, 2), (14, 2), (17, 2)] {
        if !b[start..start + len].iter().all(u8::is_ascii_digit) {
            return None;
        }
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    // Field ranges the contract pins: jiff's temporal parser is a touch
    // more lenient (it accepts the leap-second `:60`), which the replaced
    // hand-rolled parser rejected.
    let hours = two_digits(&b[11..13])?;
    let minutes = two_digits(&b[14..16])?;
    let seconds = two_digits(&b[17..19])?;
    if hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }
    let mut end = 19;
    if end < b.len() && b[end] == b'.' {
        end += 1;
        let mut digits = 0;
        while end < b.len() && b[end].is_ascii_digit() {
            digits += 1;
            if digits > 9 {
                return None;
            }
            end += 1;
        }
        if digits == 0 {
            return None;
        }
    }
    Some(end)
}

/// Validates the RFC 3339 offset tail starting at `start`: `Z`/`z` or
/// `±HH:MM` (hours ≤ 23, minutes ≤ 59), followed by the end of the string.
fn offset_shape(b: &[u8], start: usize) -> bool {
    match b[start] {
        b'Z' | b'z' => b.len() == start + 1,
        b'+' | b'-' => {
            if b.len() != start + 6 || b[start + 3] != b':' {
                return false;
            }
            let hours = two_digits(&b[start + 1..start + 3]);
            let minutes = two_digits(&b[start + 4..start + 6]);
            matches!((hours, minutes), (Some(h), Some(m)) if h <= 23 && m <= 59)
        }
        _ => false,
    }
}

/// A two-byte ASCII digit run as 0..=99.
fn two_digits(b: &[u8]) -> Option<u32> {
    let hi = b[0].checked_sub(b'0')?;
    let lo = b[1].checked_sub(b'0')?;
    if hi > 9 || lo > 9 {
        return None;
    }
    Some(u32::from(hi) * 10 + u32::from(lo))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use super::*;

    // The parity table moved from search/enrich.rs (task 1.2 deletes the
    // hand-rolled parser; these cases pin the acceptance set).
    #[test]
    fn normalize_parity_table() {
        let cases = [
            // (input, expected)
            ("2026-08-01 12:00:00", Some("2026-08-01T12:00:00Z")),
            ("2026-08-01 12:00:00.123", Some("2026-08-01T12:00:00Z")),
            (
                "2026-08-01 12:00:00.123456789",
                Some("2026-08-01T12:00:00Z"),
            ),
            ("2026-08-01T12:00:00Z", Some("2026-08-01T12:00:00Z")),
            ("2026-08-01T12:00:00z", Some("2026-08-01T12:00:00Z")),
            ("2026-08-01T12:00:00.5Z", Some("2026-08-01T12:00:00Z")),
            (
                "2026-08-01T12:00:00+02:00",
                Some("2026-08-01T12:00:00+02:00"),
            ),
            (
                "2026-08-01T12:00:00-05:30",
                Some("2026-08-01T12:00:00-05:30"),
            ),
            // Unparseable → None.
            ("", None),
            ("not-a-date", None),
            ("2026-13-01 12:00:00", None),
            ("2026-02-30 12:00:00", None),
            ("2024-02-29 12:00:00", Some("2024-02-29T12:00:00Z")),
            ("2026-02-29 12:00:00", None),
            ("2026-08-01T25:00:00Z", None),
            ("2026-08-01 12:00:60", None),
            ("2026-08-01 12:00:00Z", None),
            ("2026-08-01T12:00:00", None),
            ("2026-08-01 12:00:00.", None),
            ("2026-08-01 12:00:00.1234567890", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_to_rfc3339(input).as_deref(),
                expected,
                "normalize_to_rfc3339({input:?})"
            );
        }
    }

    // jiff's parser is strictly more lenient than the replaced contract;
    // these shapes must stay rejected (byte-identical acceptance).
    #[test]
    fn normalize_rejects_lenient_shapes() {
        let cases = [
            "2026-8-01 12:00:00",        // one-digit month
            "2026-08-01T12:00Z",         // seconds omitted
            "2026-08-01T12Z",            // minutes and seconds omitted
            "2026-08-01T12:00:00+0200",  // colon-less offset
            "2026-08-01T12:00:00-04",    // hour-only offset
            "2026-08-01 12:00:00 junk",  // trailing data on the SQLite layout
            "2026-08-01T12:00:60Z",      // leap second (jiff accepts it)
            "2026-08-01T12:00:00+24:00", // offset hours out of range
            "2026-08-01T12:00:00+02:60", // offset minutes out of range
        ];
        for input in cases {
            assert_eq!(normalize_to_rfc3339(input), None, "{input:?}");
        }
    }

    // A zero offset renders as `Z` (oracle `time.Format(RFC3339)`).
    #[test]
    fn normalize_zero_offset_renders_z() {
        assert_eq!(
            normalize_to_rfc3339("2026-08-01T12:00:00+00:00"),
            Some("2026-08-01T12:00:00Z".to_owned())
        );
    }

    // format_rfc3339: known instants (moved from the ingestion parsers
    // tests) plus round-trips through parse_epoch_seconds.
    #[test]
    fn format_rfc3339_round_trips() {
        // 1_767_225_600 s since the Unix epoch = 2026-01-01T00:00:00Z.
        let instant = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        let rendered = format_rfc3339(instant).unwrap();
        assert_eq!(rendered, "2026-01-01T00:00:00Z");
        assert_eq!(parse_epoch_seconds(&rendered), Some(1_767_225_600));
        assert_eq!(normalize_to_rfc3339(&rendered), Some(rendered));

        assert_eq!(
            format_rfc3339(UNIX_EPOCH),
            Some("1970-01-01T00:00:00Z".to_owned())
        );

        // Pre-epoch instants are not representable as a SystemTime duration.
        assert!(format_rfc3339(UNIX_EPOCH - Duration::from_secs(1)).is_none());
    }

    // The SQLite layout renders from a real instant and parses back to the
    // same epoch seconds.
    #[test]
    fn sqlite_layout_round_trips() {
        let seconds = 1_767_225_600i64 + 43_210; // 2026-01-01T12:00:10Z
        let instant = UNIX_EPOCH + Duration::from_secs(seconds as u64);
        assert_eq!(
            format_rfc3339(instant),
            Some("2026-01-01T12:00:10Z".to_owned())
        );
        let civil = Offset::UTC.to_datetime(Timestamp::from_second(seconds).unwrap());
        let sqlite = civil.strftime("%Y-%m-%d %H:%M:%S").to_string();
        assert_eq!(sqlite, "2026-01-01 12:00:10");
        assert_eq!(parse_epoch_seconds(&sqlite), Some(seconds));
        assert_eq!(
            normalize_to_rfc3339(&sqlite),
            Some("2026-01-01T12:00:10Z".to_owned())
        );
    }

    // Offsets parse to the right epoch seconds (wall clock minus offset);
    // fractional seconds are dropped, not rounded.
    #[test]
    fn parse_epoch_seconds_with_offsets() {
        // 2026-01-01T12:00:10Z.
        let base = 1_767_225_600i64 + 12 * 3_600 + 10;
        assert_eq!(parse_epoch_seconds("2026-01-01T12:00:10Z"), Some(base));
        assert_eq!(parse_epoch_seconds("2026-01-01T12:00:10z"), Some(base));
        // 12:00:10+02:00 is 10:00:10Z — two hours earlier.
        assert_eq!(
            parse_epoch_seconds("2026-01-01T12:00:10+02:00"),
            Some(base - 2 * 3_600)
        );
        // 12:00:10-05:30 is 17:30:10Z — seven and a half hours later.
        assert_eq!(
            parse_epoch_seconds("2026-01-01T12:00:10-05:30"),
            Some(base + 5 * 3_600 + 30 * 60)
        );
        assert_eq!(parse_epoch_seconds("2026-01-01T12:00:10.9Z"), Some(base));
        // Same acceptance set as normalize_to_rfc3339.
        assert_eq!(parse_epoch_seconds("not-a-date"), None);
        assert_eq!(parse_epoch_seconds(""), None);
    }

    // Backup stamp: the VACUUM snapshot naming contract (cases moved from
    // the ingestion backup tests).
    #[test]
    fn backup_stamp_matches_design_d6() {
        assert_eq!(format_backup_stamp(UNIX_EPOCH), "1970-01-01T00-00-00-000");
        // 2020-02-29T12:30:45.007Z (leap-day anchor).
        let leap = UNIX_EPOCH + Duration::from_millis(1_582_979_445_007);
        assert_eq!(format_backup_stamp(leap), "2020-02-29T12-30-45-007");
        // Pre-epoch clamps to the epoch (legacy unwrap_or_default behavior).
        assert_eq!(
            format_backup_stamp(UNIX_EPOCH - Duration::from_secs(1)),
            "1970-01-01T00-00-00-000"
        );
    }

    // now_rfc3339: canonical, and within a few seconds of the system clock.
    #[test]
    fn now_rfc3339_is_recent_and_canonical() {
        let rendered = now_rfc3339().expect("system clock is after the epoch");
        assert!(rendered.ends_with('Z'), "{rendered}");
        let parsed = parse_epoch_seconds(&rendered).expect("canonical output parses");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_secs() as i64;
        assert!(
            (parsed - now).abs() <= 5,
            "now_rfc3339 drifted: {rendered} vs {now}"
        );
    }
}
