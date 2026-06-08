use crate::error::{Error, Result};

pub(crate) fn hls_attribute(line: &str, key: &str) -> Result<Option<String>> {
    let line = line.trim();
    if key.is_empty() {
        let start = line.find(':').map_or(0, |index| index.saturating_add(1));
        return Ok(line.get(start..).map(str::to_string));
    }

    let quoted = format!("{key}=\"");
    if let Some(index) = line.find(&quoted) {
        let start = index + quoted.len();
        let tail = line
            .get(start..)
            .ok_or_else(|| Error::protocol(format!("{key} attribute is invalid")))?;
        let end = tail
            .find('"')
            .ok_or_else(|| Error::protocol(format!("{key} attribute is invalid")))?;
        return Ok(tail.get(..end).map(str::to_string));
    }

    let unquoted = format!("{key}=");
    if let Some(index) = line.find(&unquoted) {
        let start = index + unquoted.len();
        let tail = line
            .get(start..)
            .ok_or_else(|| Error::protocol(format!("{key} attribute is invalid")))?;
        let end = tail.find(',').unwrap_or(tail.len());
        return Ok(tail.get(..end).map(str::to_string));
    }

    Ok(None)
}

pub(crate) fn non_empty_hls_attribute(line: &str, key: &str) -> Result<Option<String>> {
    Ok(hls_attribute(line, key)?.filter(|value| !value.is_empty()))
}
