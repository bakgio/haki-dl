use time::{OffsetDateTime, UtcOffset};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManifestTimestamp {
    pub(crate) unix_seconds: i64,
    pub(crate) unix_millis: i64,
}

pub(crate) fn parse_manifest_timestamp(value: &str) -> Option<ManifestTimestamp> {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 19
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }

    let year = parse_digits(bytes.get(0..4)?)?;
    let month = parse_digits(bytes.get(5..7)?)?;
    let day = parse_digits(bytes.get(8..10)?)?;
    let hour = parse_digits(bytes.get(11..13)?)?;
    let minute = parse_digits(bytes.get(14..16)?)?;
    let second = parse_digits(bytes.get(17..19)?)?;
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let mut cursor = 19;
    let mut millis = 0_i64;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        let mut digits = 0_u8;
        while let Some(byte) = bytes.get(cursor)
            && byte.is_ascii_digit()
        {
            if digits < 3 {
                millis = millis * 10 + i64::from(byte - b'0');
                digits += 1;
            }
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
        while digits < 3 {
            millis *= 10;
            digits += 1;
        }
    }

    let offset_seconds = parse_offset_seconds(bytes.get(cursor..).unwrap_or_default())?;
    let days = days_from_civil(year, month, day)?;
    let local_seconds = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    let unix_seconds = local_seconds.checked_sub(i64::from(offset_seconds))?;
    let unix_millis = unix_seconds.checked_mul(1_000)?.checked_add(millis)?;
    Some(ManifestTimestamp {
        unix_seconds,
        unix_millis,
    })
}

pub(crate) fn parse_manifest_timestamp_seconds(value: &str) -> Option<i64> {
    parse_manifest_timestamp(value).map(|timestamp| timestamp.unix_seconds)
}

pub(crate) fn parse_manifest_timestamp_millis(value: &str) -> Option<i64> {
    parse_manifest_timestamp(value).map(|timestamp| timestamp.unix_millis)
}

pub(crate) fn current_local_iso_timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    iso_timestamp(now)
}

fn iso_timestamp(value: OffsetDateTime) -> String {
    let offset_seconds = value.offset().whole_seconds();
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let absolute_offset = offset_seconds.unsigned_abs();
    let offset_hours = absolute_offset / 3_600;
    let offset_minutes = (absolute_offset % 3_600) / 60;
    let fractional_ticks = value.nanosecond() / 100;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}{sign}{:02}:{:02}",
        value.year(),
        u8::from(value.month()),
        value.day(),
        value.hour(),
        value.minute(),
        value.second(),
        fractional_ticks,
        offset_hours,
        offset_minutes
    )
}

fn parse_offset_seconds(bytes: &[u8]) -> Option<i32> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    if text.is_empty() {
        return Some(local_offset_seconds());
    }
    if matches!(text, "Z" | "z") {
        return Some(0);
    }
    let mut chars = text.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let rest = chars.as_str();
    let (hour, minute) = if let Some((hour, minute)) = rest.split_once(':') {
        (parse_ascii_i32(hour)?, parse_ascii_i32(minute)?)
    } else if rest.len() == 2 {
        (parse_ascii_i32(rest)?, 0)
    } else if rest.len() == 4 {
        (
            parse_ascii_i32(rest.get(0..2)?)?,
            parse_ascii_i32(rest.get(2..4)?)?,
        )
    } else {
        return None;
    };
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(sign * (hour * 3_600 + minute * 60))
}

fn parse_ascii_i32(value: &str) -> Option<i32> {
    if value.is_empty() || !value.as_bytes().iter().all(u8::is_ascii_digit) {
        return None;
    }
    value.parse().ok()
}

fn local_offset_seconds() -> i32 {
    UtcOffset::current_local_offset()
        .map(|offset| offset.whole_seconds())
        .unwrap_or(0)
}

fn parse_digits(bytes: &[u8]) -> Option<i64> {
    let mut value = 0_i64;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(i64::from(byte - b'0'))?;
    }
    Some(value)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month)?;
    if day < 1 || day > max_day {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn days_in_month(year: i64, month: i64) -> Option<i64> {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => Some(31),
        4 | 6 | 9 | 11 => Some(30),
        2 => Some(if is_leap_year(year) { 29 } else { 28 }),
        _ => None,
    }
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}
