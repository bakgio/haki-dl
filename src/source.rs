//! Source and segment loading utilities.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::DownloadOptions;
use crate::error::{Error, Result};
use crate::http::{DefaultHttpClient, HttpRequest, HttpResponse};
use crate::manifest::ExtractorType;
use crate::processor::{
    ContentProcessor, DefaultDashContentProcessor, DefaultHlsContentProcessor, ParserConfig,
};

pub(crate) const HTTP_LIVE_TS_MARKER: &str = "<HAKI_LIVE_TS>";
pub(crate) const BINARY_DATA_MARKER: &str = "<HAKI_BINARY_DATA>";

/// Loaded source classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadedSourceKind {
    /// HLS text manifest.
    Hls,
    /// MPEG-DASH text manifest.
    Dash,
    /// Smooth Streaming manifest.
    Mss,
    /// Direct MPEG-TS live stream.
    HttpLiveTs,
    /// Direct binary input that is not a supported streaming manifest.
    BinaryData,
}

impl LoadedSourceKind {
    /// Returns the extractor family for this source.
    pub fn extractor_type(self) -> ExtractorType {
        match self {
            Self::Hls => ExtractorType::Hls,
            Self::Dash => ExtractorType::MpegDash,
            Self::Mss => ExtractorType::Mss,
            Self::HttpLiveTs => ExtractorType::HttpLive,
            Self::BinaryData => ExtractorType::HttpLive,
        }
    }

    fn raw_file_name(self) -> &'static str {
        match self {
            Self::Hls => "raw.m3u8",
            Self::Dash => "raw.mpd",
            Self::Mss => "raw.ism",
            Self::HttpLiveTs => "raw.txt",
            Self::BinaryData => "raw.bin",
        }
    }
}

/// Loaded source text and retained raw file map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedSource {
    /// Source kind.
    pub kind: LoadedSourceKind,
    /// Source text or live-TS marker.
    pub text: String,
    /// Original input URL/path.
    pub original_url: String,
    /// Final URL/path after redirects.
    pub final_url: String,
    /// Raw files that should be written to the temp directory later.
    pub raw_files: BTreeMap<String, String>,
    /// Debug lines captured while loading the source.
    pub debug_logs: Vec<String>,
}

/// Compatibility source loader.
pub struct SourceLoader {
    http: DefaultHttpClient,
    content_processors: Vec<Box<dyn ContentProcessor>>,
}

impl Default for SourceLoader {
    fn default() -> Self {
        Self {
            http: DefaultHttpClient::new(),
            content_processors: vec![
                Box::<DefaultHlsContentProcessor>::default(),
                Box::<DefaultDashContentProcessor>::default(),
            ],
        }
    }
}

impl SourceLoader {
    /// Creates a loader with default processors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a loader from public download options.
    pub fn from_options(options: &DownloadOptions) -> Self {
        Self::new().with_http(DefaultHttpClient::from_options(options))
    }

    /// Creates a loader with a custom HTTP client.
    pub fn with_http(mut self, http: DefaultHttpClient) -> Self {
        self.http = http;
        self
    }

    /// Loads a manifest, local source, or direct live-TS source.
    pub async fn load_source(
        &self,
        input: &str,
        config: &mut ParserConfig,
    ) -> Result<LoadedSource> {
        let (mut text, original_url, final_url, mut debug_logs) = if input.starts_with("file:") {
            let path = file_uri_to_path(input);
            (
                read_source_text_file(&path).await?,
                input.to_string(),
                input.to_string(),
                Vec::new(),
            )
        } else if input.starts_with("http://") || input.starts_with("https://") {
            let response = self.get_web_source(input, &config.headers).await?;
            (response.0, input.to_string(), response.1, response.2)
        } else if tokio::fs::metadata(input).await.is_ok() {
            let path = tokio::fs::canonicalize(input).await?;
            let file_uri =
                path_to_file_uri(&path).unwrap_or_else(|| path.to_string_lossy().to_string());
            (
                read_source_text_file(&path).await?,
                file_uri.clone(),
                file_uri,
                Vec::new(),
            )
        } else {
            return Err(Error::http("source input could not be loaded"));
        };

        if text.trim().is_empty() {
            return Err(Error::http("source input was empty"));
        }
        text = text.trim().trim_start_matches('\u{feff}').to_string();
        config.original_url = original_url.clone();
        config.url = final_url.clone();
        let kind = detect_source_kind(&text)?;
        let mut raw_files = BTreeMap::new();
        raw_files.insert(kind.raw_file_name().to_string(), text.clone());
        for processor in &self.content_processors {
            if processor.can_process(kind.extractor_type(), &text, config) {
                text = processor.process(&text, config)?;
            }
        }
        debug_logs.retain(|line| !line.trim().is_empty());
        Ok(LoadedSource {
            kind,
            text,
            original_url,
            final_url,
            raw_files,
            debug_logs,
        })
    }

    /// Loads one segment-like byte source.
    pub async fn load_segment_bytes(&self, url: &str, config: &ParserConfig) -> Result<Vec<u8>> {
        if url.starts_with("file:") {
            return Ok(tokio::fs::read(file_uri_to_path(url)).await?);
        }
        if let Some(value) = url.strip_prefix("base64://") {
            return base64_decode(value);
        }
        if let Some(value) = url.strip_prefix("hex://") {
            return hex_to_bytes(value);
        }
        if tokio::fs::metadata(url).await.is_ok() {
            return Ok(tokio::fs::read(url).await?);
        }
        let mut request = HttpRequest::new(url);
        request.headers = config.headers.clone();
        Ok(self.http.send(request).await?.body)
    }

    async fn get_web_source(
        &self,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<(String, String, Vec<String>)> {
        let mut request = HttpRequest::new(url);
        request.headers = headers.clone();
        let response = self.http.send_source(request).await?;
        let mut debug_logs = response.debug_logs.clone();
        if is_mpeg2_ts_buffer(&response.body) {
            debug_logs.push("Detected MPEG-TS stream".to_string());
            return Ok((HTTP_LIVE_TS_MARKER.to_string(), url.to_string(), debug_logs));
        }
        if let Some(encoding) = response.headers.get("content-encoding")
            && !encoding.trim().is_empty()
        {
            debug_logs.push(format!("Detected compression: {encoding}"));
        }
        let charset = response_charset(&response);
        if charset.is_some() || has_text_bom(&response.body) {
            let text = decode_text_bytes(&response.body, charset.as_deref())?;
            return Ok((text, response.final_url, debug_logs));
        }
        if looks_like_binary(&sample(&response.body)) {
            debug_logs.push("Heuristic detection: binary data".to_string());
            return Ok((
                BINARY_DATA_MARKER.to_string(),
                response.final_url,
                debug_logs,
            ));
        }
        let text = decode_text_bytes(&response.body, charset.as_deref())?;
        Ok((text, response.final_url, debug_logs))
    }
}

async fn read_source_text_file(path: &std::path::Path) -> Result<String> {
    decode_text_bytes(&tokio::fs::read(path).await?, None)
}

/// Writes retained raw files under a target directory.
pub async fn write_raw_files(
    raw_files: &BTreeMap<String, String>,
    directory: &std::path::Path,
) -> Result<Vec<PathBuf>> {
    tokio::fs::create_dir_all(directory).await?;
    let mut written = Vec::with_capacity(raw_files.len());
    for (name, text) in raw_files {
        let path = directory.join(name);
        if tokio::fs::metadata(&path).await.is_err() {
            tokio::fs::write(&path, n_utf8_text_file_bytes(text)).await?;
        }
        written.push(path);
    }
    Ok(written)
}

fn n_utf8_text_file_bytes(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(3 + text.len());
    bytes.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

fn detect_source_kind(text: &str) -> Result<LoadedSourceKind> {
    let trimmed = text.trim();
    if trimmed.starts_with("#EXTM3U") {
        Ok(LoadedSourceKind::Hls)
    } else if trimmed.contains("</MPD>") && trimmed.contains("<MPD") {
        Ok(LoadedSourceKind::Dash)
    } else if trimmed.contains("</SmoothStreamingMedia>")
        && trimmed.contains("<SmoothStreamingMedia")
    {
        Ok(LoadedSourceKind::Mss)
    } else if trimmed == HTTP_LIVE_TS_MARKER {
        Ok(LoadedSourceKind::HttpLiveTs)
    } else if trimmed == BINARY_DATA_MARKER {
        Ok(LoadedSourceKind::BinaryData)
    } else {
        Err(Error::compatibility("source input type is not supported"))
    }
}

fn response_charset(response: &HttpResponse) -> Option<String> {
    response
        .headers
        .get("content-type")
        .and_then(|value| content_type_charset(value))
}

fn decode_text_bytes(bytes: &[u8], charset: Option<&str>) -> Result<String> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Ok(String::from_utf8_lossy(&bytes[3..]).to_string());
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], false);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], true);
    }
    match charset {
        Some("utf-16") => decode_utf16_auto(bytes),
        Some("utf-16le") => decode_utf16(bytes, false),
        Some("utf-16be") => decode_utf16(bytes, true),
        Some(label) => Ok(decode_with_label(label, bytes)),
        None => Ok(String::from_utf8_lossy(bytes).to_string()),
    }
}

fn has_text_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
}

fn decode_with_label(label: &str, bytes: &[u8]) -> String {
    match encoding_rs::Encoding::for_label(label.as_bytes()) {
        Some(encoding) => {
            let (text, _, _) = encoding.decode(bytes);
            text.into_owned()
        }
        None => String::from_utf8_lossy(bytes).to_string(),
    }
}

fn content_type_charset(content_type: &str) -> Option<String> {
    content_type.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        if key.eq_ignore_ascii_case("charset") {
            Some(value.trim_matches('"').to_ascii_lowercase())
        } else {
            None
        }
    })
}

fn decode_utf16_auto(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], false);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], true);
    }
    decode_utf16(bytes, false)
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Result<String> {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let unit = if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        };
        units.push(unit);
    }
    let mut text = String::from_utf16_lossy(&units);
    if bytes.len() & 1 != 0 {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    Ok(text)
}

fn sample(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().take(4096).copied().collect()
}

fn looks_like_binary(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    let mut non_text = 0_usize;
    let mut total = 0_usize;
    let mut index = 0_usize;
    while index < data.len() {
        let byte = data[index];
        total += 1;
        if byte == 0 {
            return true;
        }
        if (0x20..=0x7e).contains(&byte) || matches!(byte, 0x09 | 0x0a | 0x0d) {
            index += 1;
            continue;
        }
        let seq_len = utf8_sequence_length(byte);
        if seq_len > 1
            && index + seq_len <= data.len()
            && valid_utf8_sequence(&data[index..index + seq_len])
        {
            index += seq_len;
            continue;
        }
        non_text += 1;
        index += 1;
    }
    (non_text as f64 / total as f64) > 0.3
}

fn utf8_sequence_length(byte: u8) -> usize {
    if byte & 0x80 == 0x00 {
        1
    } else if byte & 0xe0 == 0xc0 {
        2
    } else if byte & 0xf0 == 0xe0 {
        3
    } else if byte & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

fn valid_utf8_sequence(seq: &[u8]) -> bool {
    if seq.len() <= 1 {
        return false;
    }
    seq.iter().skip(1).all(|byte| byte & 0xc0 == 0x80)
}

fn is_mpeg2_ts_buffer(buffer: &[u8]) -> bool {
    const PACKET_SIZE: usize = 188;
    if buffer.len() < PACKET_SIZE {
        return false;
    }
    let packet_count = std::cmp::min(buffer.len() / PACKET_SIZE, 5);
    let sync_count = (0..packet_count)
        .filter(|index| buffer[index * PACKET_SIZE] == 0x47)
        .count();
    sync_count >= 3
}

fn file_uri_to_path(value: &str) -> PathBuf {
    if let Ok(url) = reqwest::Url::parse(value)
        && url.scheme() == "file"
        && let Ok(path) = url.to_file_path()
    {
        return path;
    }
    let stripped = value
        .trim_start_matches("file://")
        .trim_start_matches("file:");
    #[cfg(windows)]
    {
        let mut normalized = stripped.trim_start_matches('/');
        if let Some(rest) = normalized.strip_prefix("?/") {
            normalized = rest;
        }
        PathBuf::from(normalized)
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(stripped)
    }
}

fn path_to_file_uri(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    #[cfg(windows)]
    {
        let text = text.replace('\\', "/");
        let normalized = text.strip_prefix("//?/").unwrap_or(&text);
        Some(format!("file:///{normalized}"))
    }
    #[cfg(not(windows))]
    {
        Some(format!("file://{text}"))
    }
}

fn hex_to_bytes(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if value.len() & 1 != 0 {
        return Err(Error::config("hex length must be even"));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut chars = value.chars();
    while let Some(high) = chars.next() {
        let low = chars
            .next()
            .ok_or_else(|| Error::config("hex length must be even"))?;
        let high = hex_value(high).ok_or_else(|| Error::config("hex is invalid"))?;
        let low = hex_value(low).ok_or_else(|| Error::config("hex is invalid"))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn hex_value(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    crate::base64::decode_base64(value).map_err(Error::config)
}
