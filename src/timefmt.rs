//! UTC timestamps as `YYYY-MM-DDTHH:MM:SSZ`, the one format every record uses.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    format_unix(i64::try_from(secs).unwrap_or(i64::MAX))
}

/// Normalize an upstream timestamp (ISO string or Unix seconds) to UTC ISO.
///
/// A string that does not parse as an ISO timestamp with a UTC offset is kept
/// as it came, trimmed. Anything that is neither a string nor a number is null.
pub fn to_iso(value: &Value) -> Value {
    match value {
        Value::Number(number) => number
            .as_f64()
            .map_or(Value::Null, |secs| format_unix(secs.floor() as i64).into()),
        Value::String(text) => {
            let text = text.trim();
            parse_iso(text).map_or_else(|| text.into(), |secs| format_unix(secs).into())
        }
        _ => Value::Null,
    }
}

pub fn format_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `YYYY-MM-DD[T ]HH:MM:SS[.fraction](Z|±HH[:MM])` to Unix seconds; the
/// fraction is dropped.
fn parse_iso(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || !text.is_ascii() {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let separators = bytes[4] == b'-'
        && bytes[7] == b'-'
        && matches!(bytes[10], b'T' | b't' | b' ')
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !separators {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix(['.', ',']) {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits = rest[1..].replace(':', "");
            if digits.len() != 2 && digits.len() != 4 {
                return None;
            }
            let hours = digits[..2].parse::<i64>().ok()?;
            let minutes = digits
                .get(2..4)
                .map_or(Some(0), |m| m.parse::<i64>().ok())?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

// Both conversions are the proleptic Gregorian algorithms from Howard
// Hinnant's "chrono-Compatible Low-Level Date Algorithms".

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn unix_seconds_become_utc_iso() {
        assert_eq!(to_iso(&json!(1_788_747_923)), json!("2026-09-07T02:25:23Z"));
        assert_eq!(
            to_iso(&json!(1_788_747_923.9)),
            json!("2026-09-07T02:25:23Z")
        );
        assert_eq!(to_iso(&json!(0)), json!("1970-01-01T00:00:00Z"));
    }

    #[test]
    fn iso_strings_are_normalized_to_utc_seconds() {
        assert_eq!(
            to_iso(&json!("2026-08-31T11:00:00.000Z")),
            json!("2026-08-31T11:00:00Z")
        );
        assert_eq!(
            to_iso(&json!("2026-09-30T05:00:00.123456+00:00")),
            json!("2026-09-30T05:00:00Z")
        );
        assert_eq!(
            to_iso(&json!("2026-03-01T01:30:00+02:00")),
            json!("2026-02-28T23:30:00Z")
        );
        assert_eq!(
            to_iso(&json!("2024-02-29T23:59:59-05:30")),
            json!("2024-03-01T05:29:59Z")
        );
    }

    #[test]
    fn unparseable_text_is_kept_and_null_stays_null() {
        assert_eq!(to_iso(&json!(" next tuesday ")), json!("next tuesday"));
        assert_eq!(to_iso(&Value::Null), Value::Null);
    }
}
