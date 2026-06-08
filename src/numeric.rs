use crate::error::{Error, Result};

/// Parses an English-form manifest floating-point field.
pub(crate) fn parse_manifest_f64(value: &str, field: &str) -> Result<f64> {
    let normalized = normalize_manifest_f64(value)
        .ok_or_else(|| Error::protocol(format!("{field} is invalid")))?;
    if normalized == "NaN" {
        return Ok(f64::NAN);
    }
    let parsed = normalized
        .parse::<f64>()
        .map_err(|_| Error::protocol(format!("{field} is invalid")))?;
    if parsed.is_finite() {
        Ok(parsed)
    } else {
        Err(Error::protocol(format!("{field} is invalid")))
    }
}

/// Converts a floating-point value through the manifest refresh interval integer shape.
pub(crate) fn refresh_interval_i32_value(value: f64) -> f64 {
    const I32_MAX_PLUS_ONE: f64 = 2_147_483_648.0;
    const I32_MIN_VALUE: f64 = -2_147_483_648.0;

    if !value.is_finite() || !(I32_MIN_VALUE..I32_MAX_PLUS_ONE).contains(&value) {
        I32_MIN_VALUE
    } else {
        value.trunc()
    }
}

fn normalize_manifest_f64(value: &str) -> Option<String> {
    let value = value.trim();
    if value == "NaN" {
        return Some(value.to_string());
    }
    if !value.contains(',') {
        return Some(value.to_string());
    }

    let exponent_index = value.find(['e', 'E']);
    let (mantissa, exponent) = match exponent_index {
        Some(index) => value.split_at(index),
        None => (value, ""),
    };
    if exponent.contains(',') {
        return None;
    }

    let decimal_index = mantissa.find('.');
    let (integer, fraction) = match decimal_index {
        Some(index) => mantissa.split_at(index),
        None => (mantissa, ""),
    };
    if fraction.contains(',') {
        return None;
    }

    let mut saw_digit = false;
    for ch in integer.chars() {
        match ch {
            '0'..='9' => saw_digit = true,
            ',' if saw_digit => {}
            ',' => return None,
            _ => {}
        }
    }

    let mut normalized = String::with_capacity(value.len());
    normalized.extend(integer.chars().filter(|ch| *ch != ','));
    normalized.push_str(fraction);
    normalized.push_str(exponent);
    Some(normalized)
}
