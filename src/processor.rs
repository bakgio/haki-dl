//! Parser configuration and processor hooks.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::{Digest, Md5};
use regex::Regex;
use time::OffsetDateTime;

use crate::attribute::hls_attribute;
use crate::config::{DownloadOptions, LogLevel};
use crate::error::{Error, Result};
use crate::http::{DefaultHttpClient, HttpRequest};
use crate::manifest::{EncryptionInfo, EncryptionMethod, ExtractorType, KeySource};

/// Compatibility default user-agent applied before user headers override it.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/78.0.3904.108 Safari/537.36";

/// Parser diagnostic produced while applying processor hooks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParserDiagnostic {
    /// Diagnostic log level.
    pub level: LogLevel,
    /// Diagnostic text.
    pub message: String,
}

#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct ParserDiagnostics {
    inner: Arc<Mutex<Vec<ParserDiagnostic>>>,
}

impl ParserDiagnostics {
    fn push(&self, level: LogLevel, message: impl Into<String>) {
        if let Ok(mut diagnostics) = self.inner.lock() {
            diagnostics.push(ParserDiagnostic {
                level,
                message: message.into(),
            });
        }
    }

    fn drain(&self) -> Vec<ParserDiagnostic> {
        match self.inner.lock() {
            Ok(mut diagnostics) => diagnostics.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Parser and loader configuration shared by manifest processors.
#[derive(Clone, Debug)]
pub struct ParserConfig {
    /// Current URL after redirects.
    pub url: String,
    /// Original user input URL.
    pub original_url: String,
    /// Base URL override.
    pub base_url: String,
    /// Custom parser arguments.
    pub custom_parser_args: BTreeMap<String, String>,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
    /// Append the manifest query string to processed segment URLs.
    pub append_url_params: bool,
    /// Arguments for custom URL processors.
    pub urlprocessor_args: Option<String>,
    /// Key retry count.
    pub key_retry_count: u32,
    /// Segment URL keywords retained for higher-level cleanup after selection.
    pub ad_keywords: Vec<String>,
    /// Custom encryption method override.
    pub custom_method: Option<EncryptionMethod>,
    /// Custom key override.
    pub custom_key: Option<Vec<u8>>,
    /// Custom IV override.
    pub custom_iv: Option<Vec<u8>>,
    #[doc(hidden)]
    pub diagnostics: ParserDiagnostics,
}

impl Default for ParserConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            original_url: String::new(),
            base_url: String::new(),
            custom_parser_args: BTreeMap::new(),
            headers: compatibility_headers(&BTreeMap::new()),
            append_url_params: false,
            urlprocessor_args: None,
            key_retry_count: 3,
            ad_keywords: Vec::new(),
            custom_method: None,
            custom_key: None,
            custom_iv: None,
            diagnostics: ParserDiagnostics::default(),
        }
    }
}

impl ParserConfig {
    /// Builds parser configuration from public download options.
    pub fn from_options(options: &DownloadOptions) -> Self {
        let mut custom_parser_args = BTreeMap::new();
        if options.allow_hls_multi_ext_map {
            custom_parser_args.insert("AllowHlsMultiExtMap".to_string(), "true".to_string());
        }
        Self {
            base_url: options.base_url.clone().unwrap_or_default(),
            custom_parser_args,
            headers: compatibility_headers(&options.headers),
            append_url_params: options.append_url_params,
            urlprocessor_args: options.urlprocessor_args.clone(),
            ad_keywords: Vec::new(),
            custom_method: options
                .custom_hls_method
                .as_ref()
                .map(|method| match method {
                    crate::config::HlsMethod::None => EncryptionMethod::None,
                    crate::config::HlsMethod::Aes128 => EncryptionMethod::Aes128,
                    crate::config::HlsMethod::Aes128Ecb => EncryptionMethod::Aes128Ecb,
                    crate::config::HlsMethod::Cenc => EncryptionMethod::Cenc,
                    crate::config::HlsMethod::SampleAes => EncryptionMethod::SampleAes,
                    crate::config::HlsMethod::SampleAesCtr => EncryptionMethod::SampleAesCtr,
                    crate::config::HlsMethod::Chacha20 => EncryptionMethod::Chacha20,
                    crate::config::HlsMethod::Unknown => EncryptionMethod::Unknown,
                }),
            custom_key: options
                .custom_hls_key
                .as_ref()
                .filter(|key| !key.is_empty())
                .cloned(),
            custom_iv: options
                .custom_hls_iv
                .as_ref()
                .filter(|iv| !iv.is_empty())
                .cloned(),
            ..Self::default()
        }
    }

    /// Appends a parser diagnostic for the session to consume.
    pub fn push_diagnostic(&self, level: LogLevel, message: impl Into<String>) {
        self.diagnostics.push(level, message);
    }

    /// Drains pending parser diagnostics.
    pub fn drain_diagnostics(&self) -> Vec<ParserDiagnostic> {
        self.diagnostics.drain()
    }
}

/// Builds request headers with compatibility defaults and case-insensitive user overrides.
pub fn compatibility_headers(user_headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    insert_header_override(&mut headers, "user-agent", DEFAULT_USER_AGENT);
    for (key, value) in user_headers {
        insert_header_override(&mut headers, key, value);
    }
    headers
}

fn insert_header_override(headers: &mut BTreeMap<String, String>, key: &str, value: &str) {
    let existing_key = headers
        .keys()
        .find(|existing| existing.eq_ignore_ascii_case(key))
        .cloned();
    if let Some(existing_key) = existing_key {
        headers.remove(&existing_key);
    }
    headers.insert(key.to_string(), value.to_string());
}

/// Content preprocessor hook.
pub trait ContentProcessor: Send + Sync {
    /// Returns whether this processor should run.
    fn can_process(
        &self,
        extractor_type: ExtractorType,
        raw_text: &str,
        config: &ParserConfig,
    ) -> bool;

    /// Processes manifest text.
    fn process(&self, raw_text: &str, config: &ParserConfig) -> Result<String>;
}

/// URL preprocessor hook.
pub trait UrlProcessor: Send + Sync {
    /// Returns whether this processor should run.
    fn can_process(&self, extractor_type: ExtractorType, url: &str, config: &ParserConfig) -> bool;

    /// Processes one URL.
    fn process(&self, url: &str, config: &ParserConfig) -> Result<String>;
}

/// Key parser hook.
pub trait KeyProcessor: Send + Sync {
    /// Returns whether this processor should run.
    fn can_process(
        &self,
        extractor_type: ExtractorType,
        key_line: &str,
        manifest_url: &str,
        manifest_content: &str,
        config: &ParserConfig,
    ) -> bool;

    /// Processes one key line.
    fn process<'a>(
        &'a self,
        key_line: &'a str,
        manifest_url: &'a str,
        manifest_content: &'a str,
        config: &'a ParserConfig,
    ) -> Pin<Box<dyn Future<Output = Result<EncryptionInfo>> + Send + 'a>>;
}

/// Default URL processor for append-url-params behavior.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultUrlProcessor;

impl UrlProcessor for DefaultUrlProcessor {
    fn can_process(
        &self,
        _extractor_type: ExtractorType,
        url: &str,
        config: &ParserConfig,
    ) -> bool {
        config.append_url_params && url.starts_with("http")
    }

    fn process(&self, url: &str, config: &ParserConfig) -> Result<String> {
        let processed = append_query_params(url, &config.url);
        if !query_part(&processed).is_empty() {
            config.push_diagnostic(LogLevel::Debug, format!("Before: {url}"));
            config.push_diagnostic(LogLevel::Debug, format!("After: {processed}"));
        }
        Ok(processed)
    }
}

/// DASH URL processor for signed media URLs using `--urlprocessor-args`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SignedDashUrlProcessor;

impl UrlProcessor for SignedDashUrlProcessor {
    fn can_process(
        &self,
        extractor_type: ExtractorType,
        _url: &str,
        config: &ParserConfig,
    ) -> bool {
        extractor_type == ExtractorType::MpegDash
            && config
                .urlprocessor_args
                .as_deref()
                .is_some_and(|args| args.starts_with("nowehoryzonty:"))
    }

    fn process(&self, url: &str, config: &ParserConfig) -> Result<String> {
        const SIGNED_DASH_PROCESSOR_TAG: &str = "nowehoryzonty:";
        let args = config
            .urlprocessor_args
            .as_deref()
            .and_then(|value| value.strip_prefix(SIGNED_DASH_PROCESSOR_TAG))
            .ok_or_else(|| Error::config("url processor arguments are missing"))?;
        let token = hls_attribute(args, "filminfo.secureToken")?.unwrap_or_default();
        let time_difference = match hls_attribute(args, "timeDifference")? {
            Some(value) => i64::from(parse_i32(value.trim(), "url processor timeDifference")?),
            None => 0,
        };
        let now_ms = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .map_err(|_| Error::config("current timestamp is out of range"))?;
        let path = absolute_url_path(url)?;
        Ok(format!(
            "{url}?secure={}",
            signed_dash_secure_value(&path, &token, time_difference, now_ms)
        ))
    }
}

/// Default DASH content processor for missing namespace declarations.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultDashContentProcessor;

impl ContentProcessor for DefaultDashContentProcessor {
    fn can_process(
        &self,
        extractor_type: ExtractorType,
        raw_text: &str,
        _config: &ParserConfig,
    ) -> bool {
        extractor_type == ExtractorType::MpegDash
            && ["cenc", "mspr", "mas"]
                .iter()
                .any(|prefix| namespace_missing(raw_text, prefix))
    }

    fn process(&self, raw_text: &str, config: &ParserConfig) -> Result<String> {
        config.push_diagnostic(LogLevel::Info, "Namespace missing, try fix...");
        let mut declarations = Vec::new();
        for (prefix, uri) in [
            ("cenc", "urn:mpeg:cenc:2013"),
            ("mspr", "urn:microsoft:playready"),
            ("mas", "urn:marlin:mas:1-0:services:schemas:mpd"),
        ] {
            if namespace_missing(raw_text, prefix) {
                declarations.push(format!("xmlns:{prefix}=\"{uri}\""));
            }
        }
        if declarations.is_empty() {
            return Ok(raw_text.to_string());
        }
        Ok(replace_first(
            raw_text,
            "<MPD ",
            &format!("<MPD {} ", declarations.join(" ")),
        ))
    }
}

/// Default HLS content processor for low-risk text normalizations.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultHlsContentProcessor;

impl ContentProcessor for DefaultHlsContentProcessor {
    fn can_process(
        &self,
        extractor_type: ExtractorType,
        _raw_text: &str,
        _config: &ParserConfig,
    ) -> bool {
        extractor_type == ExtractorType::Hls
    }

    fn process(&self, raw_text: &str, config: &ParserConfig) -> Result<String> {
        let mut normalized = if raw_text.contains('\r') && !raw_text.contains('\n') {
            raw_text.replace('\r', "\n")
        } else {
            raw_text.to_string()
        };
        if config.url.contains("tlivecloud-playback-cdn.ysp.cctv.cn")
            && config.url.contains("endtime=")
            && !normalized.contains("#EXT-X-ENDLIST")
        {
            normalized.push('\n');
            normalized.push_str("#EXT-X-ENDLIST");
        }
        if normalized.contains("#EXT-X-DISCONTINUITY")
            && normalized.contains("#EXT-X-MAP")
            && normalized.contains("ott.cibntv.net")
            && normalized.contains("ccode=")
        {
            normalized = rewrite_youku_dolby_maps(&normalized)?;
        }
        if normalized.contains("#EXT-X-DISCONTINUITY")
            && normalized.contains("#EXT-X-MAP")
            && config.url.contains("media.dssott.com/")
        {
            normalized = replace_first_regex(
                &normalized,
                r#"(?s)#EXT-X-MAP:URI=".*?BUMPER/.*?#EXT-X-DISCONTINUITY"#,
                "#XXX",
            )?;
        }
        if normalized.contains("#EXT-X-DISCONTINUITY")
            && normalized.contains("seg_00000.vtt")
            && config.url.contains("media.dssott.com/")
        {
            normalized = replace_first_regex(
                &normalized,
                r#"(?s)#EXTINF:.*?,\s+.*BUMPER.*\s+?#EXT-X-DISCONTINUITY"#,
                "#XXX",
            )?;
        }
        if normalized.contains("#EXT-X-DISCONTINUITY")
            && normalized.contains("#EXT-X-MAP")
            && (config.url.contains(".apple.com/")
                || Regex::new(r"#EXT-X-MAP.*\.apple\.com/")
                    .map_err(|error| Error::protocol(error.to_string()))?
                    .is_match(&normalized))
        {
            normalized = keep_apple_encrypted_hls_range(&normalized)?;
        }
        Ok(fix_hls_key_order(&normalized))
    }
}

/// Default HLS key processor.
#[derive(Clone, Debug)]
pub struct DefaultHlsKeyProcessor {
    http: DefaultHttpClient,
}

impl Default for DefaultHlsKeyProcessor {
    fn default() -> Self {
        Self {
            http: DefaultHttpClient::new(),
        }
    }
}

impl DefaultHlsKeyProcessor {
    /// Creates a key processor with a custom HTTP client.
    pub fn new(http: DefaultHttpClient) -> Self {
        Self { http }
    }
}

impl KeyProcessor for DefaultHlsKeyProcessor {
    fn can_process(
        &self,
        extractor_type: ExtractorType,
        _key_line: &str,
        _manifest_url: &str,
        _manifest_content: &str,
        _config: &ParserConfig,
    ) -> bool {
        extractor_type == ExtractorType::Hls
    }

    fn process<'a>(
        &'a self,
        key_line: &'a str,
        manifest_url: &'a str,
        _manifest_content: &'a str,
        config: &'a ParserConfig,
    ) -> Pin<Box<dyn Future<Output = Result<EncryptionInfo>> + Send + 'a>> {
        Box::pin(async move {
            let method = hls_attribute(key_line, "METHOD")?;
            let uri = hls_attribute(key_line, "URI")?;
            let iv = hls_attribute(key_line, "IV")?;
            let original_method = method.as_deref().unwrap_or_default().to_string();
            config.push_diagnostic(
                LogLevel::Debug,
                format!(
                    "METHOD:{},URI:{},IV:{}",
                    method.as_deref().unwrap_or_default(),
                    uri.as_deref().unwrap_or_default(),
                    iv.as_deref().unwrap_or_default()
                ),
            );
            let mut info = EncryptionInfo {
                method: EncryptionMethod::parse(method.as_deref()),
                iv: match iv {
                    Some(value) => Some(hex_to_bytes(strip_hex_prefix(&value))?),
                    None => None,
                },
                ..EncryptionInfo::default()
            };
            if let Some(custom_iv) = &config.custom_iv
                && !custom_iv.is_empty()
            {
                info.iv = Some(custom_iv.clone());
            }
            if let Some(custom_key) = &config.custom_key
                && !custom_key.is_empty()
            {
                info.key = Some(custom_key.clone());
                info.source = KeySource::Custom;
            } else if let Some(uri) = uri {
                match self.load_key(&uri, manifest_url, config).await {
                    Ok(key) => {
                        info.key = Some(key.0);
                        info.source = key.1;
                    }
                    Err(error) => {
                        config.push_diagnostic(
                            LogLevel::Error,
                            format!(
                                "Failed to get KEY, ignore.: {}",
                                error.compatibility_message()
                            ),
                        );
                        info.method = EncryptionMethod::Unknown;
                    }
                }
            } else if info.method != EncryptionMethod::None {
                info.method = EncryptionMethod::Unknown;
            }
            if let Some(custom_method) = config.custom_method {
                config.push_diagnostic(
                    LogLevel::Warn,
                    format!(
                        "METHOD changed from {} to {}",
                        original_method,
                        encryption_method_compat_name(custom_method)
                    ),
                );
                info.method = custom_method;
            }
            Ok(info)
        })
    }
}

/// Resolves a media URL with base-url override and URL processors.
pub fn resolve_media_url(
    extractor_type: ExtractorType,
    url: &str,
    manifest_url: &str,
    config: &ParserConfig,
    processors: &[&dyn UrlProcessor],
) -> Result<String> {
    let base = if config.base_url.is_empty() {
        manifest_url
    } else {
        &config.base_url
    };
    let mut resolved = combine_url(base, url);
    for processor in processors {
        if processor.can_process(extractor_type, &resolved, config) {
            resolved = processor.process(&resolved, config)?;
        }
    }
    Ok(resolved)
}

impl DefaultHlsKeyProcessor {
    async fn load_key(
        &self,
        uri: &str,
        manifest_url: &str,
        config: &ParserConfig,
    ) -> Result<(Vec<u8>, KeySource)> {
        let lower = uri.to_ascii_lowercase();
        if let Some(value) = lower
            .strip_prefix("base64:")
            .and_then(|_| uri.get("base64:".len()..))
        {
            return Ok((base64_decode(value)?, KeySource::Inline));
        }
        if let Some(value) = lower
            .strip_prefix("data:;base64,")
            .and_then(|_| uri.get("data:;base64,".len()..))
        {
            return Ok((base64_decode(value)?, KeySource::Inline));
        }
        if let Some(value) = lower
            .strip_prefix("data:text/plain;base64,")
            .and_then(|_| uri.get("data:text/plain;base64,".len()..))
        {
            return Ok((base64_decode(value)?, KeySource::Inline));
        }
        let path = Path::new(uri);
        if tokio::fs::metadata(path).await.is_ok() {
            return Ok((tokio::fs::read(path).await?, KeySource::File));
        }
        let key_url = resolve_media_url(
            ExtractorType::Hls,
            uri,
            manifest_url,
            config,
            &[&DefaultUrlProcessor],
        )?;
        if key_url.starts_with("file:") {
            return Ok((
                tokio::fs::read(file_uri_to_path(&key_url)).await?,
                KeySource::File,
            ));
        }
        let path = Path::new(&key_url);
        if tokio::fs::metadata(path).await.is_ok() {
            return Ok((tokio::fs::read(path).await?, KeySource::File));
        }
        let attempts = config.key_retry_count.saturating_add(1);
        let mut last_error = None;
        for attempt in 0..attempts {
            match self.load_http_key_once(&key_url, config).await {
                Ok(bytes) => return Ok((bytes, KeySource::Uri)),
                Err(error) => {
                    let remaining_retries = attempts.saturating_sub(attempt + 1);
                    if !error
                        .compatibility_message()
                        .contains("scheme is not supported.")
                    {
                        config.push_diagnostic(
                            LogLevel::Warn,
                            format!(
                                "{} retryCount: {remaining_retries}",
                                error.compatibility_message()
                            ),
                        );
                    }
                    last_error = Some(error);
                    if attempt + 1 < attempts {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| Error::http("key request failed")))
    }

    async fn load_http_key_once(&self, key_url: &str, config: &ParserConfig) -> Result<Vec<u8>> {
        config.push_diagnostic(LogLevel::Debug, format!("Fetch: {key_url}"));
        config.push_diagnostic(
            LogLevel::Debug,
            format_source_http_request_headers(&config.headers),
        );
        let mut request = HttpRequest::new(key_url);
        request.headers = config.headers.clone();
        let response = self.http.send(request).await?;
        for debug_log in &response.debug_logs {
            config.push_diagnostic(LogLevel::Debug, debug_log.clone());
        }
        config.push_diagnostic(LogLevel::Debug, bytes_to_spaced_hex(&response.body));
        Ok(response.body)
    }
}

fn format_source_http_request_headers(headers: &BTreeMap<String, String>) -> String {
    let mut lines = vec![
        "Accept-Encoding: gzip, deflate".to_string(),
        "Cache-Control: no-cache".to_string(),
    ];
    lines.extend(headers.iter().map(|(key, value)| format!("{key}: {value}")));
    lines.join("\n")
}

fn bytes_to_spaced_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn encryption_method_compat_name(method: EncryptionMethod) -> &'static str {
    match method {
        EncryptionMethod::None => "NONE",
        EncryptionMethod::Aes128 => "AES_128",
        EncryptionMethod::Aes128Ecb => "AES_128_ECB",
        EncryptionMethod::SampleAes => "SAMPLE_AES",
        EncryptionMethod::SampleAesCtr => "SAMPLE_AES_CTR",
        EncryptionMethod::Cenc => "CENC",
        EncryptionMethod::Chacha20 => "CHACHA20",
        EncryptionMethod::Unknown => "UNKNOWN",
    }
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

fn namespace_missing(raw_text: &str, prefix: &str) -> bool {
    !raw_text.contains(&format!("xmlns:{prefix}")) && raw_text.contains(&format!("<{prefix}:"))
}

fn replace_first(source: &str, old: &str, new: &str) -> String {
    match source.find(old) {
        Some(index) => {
            let before = source.get(..index).unwrap_or_default();
            let after = source.get(index + old.len()..).unwrap_or_default();
            format!("{before}{new}{after}")
        }
        None => source.to_string(),
    }
}

/// Calculates the signed query value for the DASH URL processor.
pub fn signed_dash_secure_value(
    path: &str,
    secure_token: &str,
    time_difference_ms: i64,
    now_ms: i64,
) -> String {
    let ms_time = now_ms
        .saturating_add(60_000)
        .saturating_add(time_difference_ms);
    let payload = format!("{ms_time}{path}{secure_token}");
    let mut hasher = Md5::new();
    hasher.update(payload.as_bytes());
    let hash = hasher.finalize();
    let hash_text = base64_encode(&hash).replace('+', "-").replace('/', "_");
    format!("{hash_text},{ms_time}")
}

fn absolute_url_path(url: &str) -> Result<String> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| Error::config("url processor input must be absolute"))?;
    Ok(parsed.path().to_string())
}

fn parse_i32(value: &str, field: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| Error::config(format!("{field} is invalid")))
}

fn replace_first_regex(source: &str, pattern: &str, replacement: &str) -> Result<String> {
    let regex = Regex::new(pattern).map_err(|error| Error::protocol(error.to_string()))?;
    if let Some(matched) = regex.find(source) {
        let before = source.get(..matched.start()).unwrap_or_default();
        let after = source.get(matched.end()..).unwrap_or_default();
        return Ok(format!("{before}{replacement}{after}"));
    }
    Ok(source.to_string())
}

fn rewrite_youku_dolby_maps(raw_text: &str) -> Result<String> {
    let regex = Regex::new(r#"#EXT-X-DISCONTINUITY\s+#EXT-X-MAP:URI="(.*?)",BYTERANGE="(.*?)""#)
        .map_err(|error| Error::protocol(error.to_string()))?;
    Ok(regex
        .replace_all(raw_text, |captures: &regex::Captures<'_>| {
            let uri = captures
                .get(1)
                .map(|value| value.as_str())
                .unwrap_or_default();
            let byterange = captures
                .get(2)
                .map(|value| value.as_str())
                .unwrap_or_default();
            format!("#EXTINF:0.000000,\n#EXT-X-BYTERANGE:{byterange}\n{uri}")
        })
        .into_owned())
}

fn keep_apple_encrypted_hls_range(raw_text: &str) -> Result<String> {
    let regex = Regex::new(r#"(?s)(#EXT-X-KEY:.*?)(#EXT-X-DISCONTINUITY|#EXT-X-ENDLIST)"#)
        .map_err(|error| Error::protocol(error.to_string()))?;
    let Some(captures) = regex.captures(raw_text) else {
        return Ok(raw_text.to_string());
    };
    let key_range = captures
        .get(1)
        .map(|value| value.as_str())
        .unwrap_or_default();
    Ok(format!("#EXTM3U\r\n{key_range}\r\n#EXT-X-ENDLIST"))
}

fn fix_hls_key_order(raw_text: &str) -> String {
    let separator = if raw_text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let lines = raw_text.lines().map(str::to_string).collect::<Vec<_>>();
    let mut fixed = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let current = lines.get(index).map(String::as_str).unwrap_or_default();
        if current.starts_with("#EXTINF") {
            let mut key_index = index + 1;
            while lines
                .get(key_index)
                .is_some_and(|line| line.trim().is_empty())
            {
                key_index += 1;
            }
            if lines
                .get(key_index)
                .is_some_and(|line| line.starts_with("#EXT-X-KEY"))
            {
                fixed.push(lines[key_index].clone());
                fixed.extend(lines[index + 1..key_index].iter().cloned());
                fixed.push(current.to_string());
                index = key_index + 1;
                continue;
            }
        }
        fixed.push(current.to_string());
        index += 1;
    }
    fixed.join(separator)
}

fn append_query_params(url: &str, source_url: &str) -> String {
    let source_query = query_part(source_url);
    if source_query.is_empty() {
        return url.to_string();
    }
    let mut target_pairs = parse_query(query_part(url), DuplicateMode::Keep);
    for pair in parse_query(source_query, DuplicateMode::Join) {
        set_or_add_query_pair(&mut target_pairs, pair);
    }
    let path = url
        .split_once('?')
        .map_or(url, |(path, _)| path)
        .split_once('#')
        .map_or_else(
            || url.split_once('?').map_or(url, |(path, _)| path),
            |(path, _)| path,
        );
    if target_pairs.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", format_query(&target_pairs))
    }
}

fn query_part(url: &str) -> &str {
    url.split_once('?').map_or("", |(_, query)| {
        query.split_once('#').map_or(query, |(query, _)| query)
    })
}

#[derive(Clone, Copy)]
enum DuplicateMode {
    Keep,
    Join,
}

#[derive(Clone, Debug)]
struct QueryPair {
    key: String,
    value: String,
    has_equals: bool,
}

fn parse_query(query: &str, duplicate_mode: DuplicateMode) -> Vec<QueryPair> {
    let pairs = query
        .trim_end_matches('?')
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((key, value)) => QueryPair {
                key: form_decode(key),
                value: form_decode(value),
                has_equals: true,
            },
            None => QueryPair {
                key: form_decode(part),
                value: String::new(),
                has_equals: false,
            },
        })
        .collect::<Vec<_>>();
    match duplicate_mode {
        DuplicateMode::Keep => pairs,
        DuplicateMode::Join => join_duplicate_query_pairs(pairs),
    }
}

fn join_duplicate_query_pairs(pairs: Vec<QueryPair>) -> Vec<QueryPair> {
    let mut output: Vec<QueryPair> = Vec::new();
    for pair in pairs {
        if let Some(existing) = output.iter_mut().find(|existing| existing.key == pair.key) {
            existing.has_equals |= pair.has_equals;
            if existing.value.is_empty() {
                existing.value = pair.value;
            } else if !pair.value.is_empty() {
                existing.value.push(',');
                existing.value.push_str(&pair.value);
            }
        } else {
            output.push(pair);
        }
    }
    output
}

fn set_or_add_query_pair(pairs: &mut Vec<QueryPair>, pair: QueryPair) {
    if !pair.has_equals {
        pairs.push(pair);
        return;
    }
    let Some(first_index) = pairs.iter().position(|existing| existing.key == pair.key) else {
        pairs.push(pair);
        return;
    };
    pairs[first_index].value = pair.value;
    pairs[first_index].has_equals = pair.has_equals;
    let mut index = 0;
    let key = pairs[first_index].key.clone();
    pairs.retain(|existing| {
        let keep = existing.key != key || index == first_index;
        index += 1;
        keep
    });
}

fn format_query(pairs: &[QueryPair]) -> String {
    pairs
        .iter()
        .map(|pair| {
            let key = form_encode(&pair.key);
            if pair.has_equals {
                format!("{key}={}", form_encode(&pair.value))
            } else {
                key
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value_char(bytes[index + 1]);
                let low = hex_value_char(bytes[index + 2]);
                if let (Some(high), Some(low)) = (high, low) {
                    output.push((high << 4) | low);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).to_string()
}

fn form_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'*'
            | b'('
            | b')' => output.push(char::from(*byte)),
            b' ' => output.push('+'),
            byte => {
                output.push('%');
                output.push(hex_digit(byte >> 4));
                output.push(hex_digit(byte & 0x0f));
            }
        }
    }
    output
}

fn hex_value_char(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(value: u8) -> char {
    char::from(b"0123456789abcdef"[usize::from(value)])
}

pub(crate) fn combine_url(base: &str, value: &str) -> String {
    if base.is_empty() {
        return value.to_string();
    }
    if let Ok(base_url) = reqwest::Url::parse(base)
        && let Ok(joined) = base_url.join(value)
    {
        return joined.to_string().replace("%7B", "{").replace("%7D", "}");
    }
    if reqwest::Url::parse(value).is_ok() {
        return value.to_string();
    }
    match base.rfind(['/', '\\']) {
        Some(index) => {
            let prefix = base.get(..=index).unwrap_or(base);
            format!("{prefix}{value}")
        }
        None => value.to_string(),
    }
}

fn hex_to_bytes(value: &str) -> Result<Vec<u8>> {
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

fn strip_hex_prefix(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed)
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

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = u32::from(chunk[0]);
        let second = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let third = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let packed = (first << 16) | (second << 8) | third;
        let indexes = [
            ((packed >> 18) & 0x3f) as usize,
            ((packed >> 12) & 0x3f) as usize,
            ((packed >> 6) & 0x3f) as usize,
            (packed & 0x3f) as usize,
        ];
        output.push(char::from(TABLE[indexes[0]]));
        output.push(char::from(TABLE[indexes[1]]));
        if chunk.len() > 1 {
            output.push(char::from(TABLE[indexes[2]]));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(char::from(TABLE[indexes[3]]));
        } else {
            output.push('=');
        }
    }
    output
}
