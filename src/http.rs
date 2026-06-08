//! HTTP request and response abstractions.

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use brotli::Decompressor;
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{
    Client as AsyncClient, RequestBuilder as AsyncRequestBuilder, Response as AsyncResponse,
};

use crate::cancellation::CancellationToken;
use crate::config::DownloadOptions;
use crate::error::{Error, Result};

pub(crate) const HTTP_CONNECTION_POOL_LIMIT: usize = 1024;
pub(crate) const SOURCE_RETRY_ATTEMPTS: usize = 10;
pub(crate) const SOURCE_RETRY_DELAY: Duration = Duration::from_millis(1500);
pub(crate) const SOURCE_RETRY_DELAY_INCREMENT: Duration = Duration::from_millis(0);
pub(crate) const LIVE_REFRESH_RETRY_ATTEMPTS: usize = 5;
pub(crate) const LIVE_REFRESH_RETRY_DELAY: Duration = Duration::from_millis(1000);

/// HTTP request model used by transport implementations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpRequest {
    /// Request URL.
    pub url: String,
    /// Request headers.
    pub headers: BTreeMap<String, String>,
}

impl HttpRequest {
    /// Creates a request model.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
        }
    }
}

/// HTTP response model used by manifest and segment loaders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpResponse {
    /// Numeric status code.
    pub status: u16,
    /// Final URL after manual redirect handling.
    pub final_url: String,
    /// Response headers.
    pub headers: BTreeMap<String, String>,
    /// Response body bytes.
    pub body: Vec<u8>,
    /// Debug lines collected while following redirects for source requests.
    pub debug_logs: Vec<String>,
}

/// Default async HTTP transport used by manifest and segment loaders.
#[derive(Clone, Debug)]
pub struct DefaultHttpClient {
    timeout: Duration,
    proxy_mode: ProxyMode,
    allow_insecure_tls: bool,
    async_client: Arc<OnceLock<AsyncClient>>,
}

/// Proxy mode for HTTP transport.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ProxyMode {
    /// Use transport/system defaults.
    #[default]
    System,
    /// Do not configure a custom proxy.
    Disabled,
    /// Use a custom proxy URL.
    Custom(String),
}

impl Default for DefaultHttpClient {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(100),
            proxy_mode: ProxyMode::System,
            allow_insecure_tls: false,
            async_client: Arc::new(OnceLock::new()),
        }
    }
}

impl DefaultHttpClient {
    /// Creates a client with compatibility defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a client from public download options.
    pub fn from_options(options: &DownloadOptions) -> Self {
        let proxy_mode = match (&options.custom_proxy, options.use_system_proxy) {
            (Some(proxy), _) => ProxyMode::Custom(proxy.clone()),
            (None, true) => ProxyMode::System,
            (None, false) => ProxyMode::Disabled,
        };
        Self::new()
            .with_timeout(options.http_request_timeout)
            .with_proxy_mode(proxy_mode)
            .with_insecure_tls(options.allow_insecure_tls)
    }

    /// Overrides the request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.reset_clients();
        self
    }

    /// Overrides proxy behavior.
    pub fn with_proxy_mode(mut self, proxy_mode: ProxyMode) -> Self {
        self.proxy_mode = proxy_mode;
        self.reset_clients();
        self
    }

    /// Enables or disables invalid TLS certificate acceptance.
    pub fn with_insecure_tls(mut self, allow: bool) -> Self {
        self.allow_insecure_tls = allow;
        self.reset_clients();
        self
    }

    /// Returns configured timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Returns configured proxy mode.
    pub fn proxy_mode(&self) -> &ProxyMode {
        &self.proxy_mode
    }

    /// Returns whether invalid TLS certificates are accepted.
    pub fn allow_insecure_tls(&self) -> bool {
        self.allow_insecure_tls
    }

    /// Sends one GET request with manual redirect handling.
    pub async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let client = self.async_client()?;
        let mut current_url = request.url.clone();
        let mut debug_logs = Vec::new();
        loop {
            let response = self
                .single_get(&client, &current_url, &request.headers)
                .await?;
            let status = response.status().as_u16();
            if is_redirect(status)
                && let Some(location) = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
            {
                let redirected = resolve_redirect(&current_url, location);
                if redirected != current_url {
                    debug_logs.push(format_response_headers(response.headers()));
                    debug_logs.push(format!("Fetch: {redirected}"));
                    debug_logs.push(format_source_http_request_headers(&request.headers));
                    current_url = redirected;
                    let _ = response.bytes().await;
                    continue;
                }
            }
            if !(200..=299).contains(&status) {
                let _ = response.bytes().await;
                return Err(Error::http(format!(
                    "HTTP status {status} for {current_url}"
                )));
            }
            let headers = response_headers(response.headers());
            let body = response
                .bytes()
                .await
                .map_err(|error| Error::http(self.redact_error(error.to_string())))?
                .to_vec();
            let body = decode_content_body(body, &headers)?;
            return Ok(HttpResponse {
                status,
                final_url: current_url,
                headers,
                body,
                debug_logs,
            });
        }
    }

    /// Sends one source-probe GET without draining MPEG-TS live streams.
    pub async fn send_source(&self, request: HttpRequest) -> Result<HttpResponse> {
        let client = self.async_client()?;
        let mut current_url = request.url.clone();
        let mut debug_logs = Vec::new();
        loop {
            let response = self
                .single_get(&client, &current_url, &request.headers)
                .await?;
            let status = response.status().as_u16();
            if is_redirect(status)
                && let Some(location) = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
            {
                let redirected = resolve_redirect(&current_url, location);
                if redirected != current_url {
                    debug_logs.push(format_response_headers(response.headers()));
                    debug_logs.push(format!("Fetch: {redirected}"));
                    debug_logs.push(format_source_http_request_headers(&request.headers));
                    current_url = redirected;
                    let _ = response.bytes().await;
                    continue;
                }
            }
            if !(200..=299).contains(&status) {
                let _ = response.bytes().await;
                return Err(Error::http(format!(
                    "HTTP status {status} for {current_url}"
                )));
            }
            let headers = response_headers(response.headers());
            let body = read_source_probe_body(response, self).await?;
            let body = decode_content_body(body, &headers)?;
            return Ok(HttpResponse {
                status,
                final_url: current_url,
                headers,
                body,
                debug_logs,
            });
        }
    }

    fn reset_clients(&mut self) {
        self.async_client = Arc::new(OnceLock::new());
    }

    fn async_client(&self) -> Result<AsyncClient> {
        if let Some(client) = self.async_client.get() {
            return Ok(client.clone());
        }
        let client = match &self.proxy_mode {
            ProxyMode::System => {
                shared_http_client(self.timeout, None, true, self.allow_insecure_tls)
            }
            ProxyMode::Disabled => {
                shared_http_client(self.timeout, None, false, self.allow_insecure_tls)
            }
            ProxyMode::Custom(proxy) => {
                shared_http_client(self.timeout, Some(proxy), false, self.allow_insecure_tls)
            }
        }?;
        let _ = self.async_client.set(client.clone());
        Ok(client)
    }

    async fn single_get(
        &self,
        client: &AsyncClient,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<AsyncResponse> {
        let mut request = client
            .get(url)
            .header("Accept-Encoding", "gzip, deflate")
            .header("Cache-Control", "no-cache");
        request = apply_request_headers(request, headers);
        request
            .send()
            .await
            .map_err(|error| Error::http(self.redact_error(error.to_string())))
    }

    fn redact_error(&self, message: String) -> String {
        match &self.proxy_mode {
            ProxyMode::Custom(proxy) => redact_proxy_error_message(proxy, &message),
            ProxyMode::System | ProxyMode::Disabled => message,
        }
    }
}

fn build_http_client_inner(
    timeout: Duration,
    custom_proxy: Option<&str>,
    use_system_proxy: bool,
    allow_insecure_tls: bool,
) -> Result<AsyncClient> {
    let mut builder = AsyncClient::builder()
        .redirect(Policy::none())
        .timeout(timeout)
        .connect_timeout(timeout)
        .pool_max_idle_per_host(HTTP_CONNECTION_POOL_LIMIT)
        .danger_accept_invalid_certs(allow_insecure_tls);
    if !use_system_proxy {
        builder = builder.no_proxy();
    }
    if let Some(proxy) = custom_proxy {
        let proxy = reqwest::Proxy::all(proxy).map_err(|error| {
            Error::config(redact_proxy_error_message(proxy, &error.to_string()))
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|error| {
        let message = match custom_proxy {
            Some(proxy) => redact_proxy_error_message(proxy, &error.to_string()),
            None => error.to_string(),
        };
        Error::http(message)
    })
}

pub(crate) fn shared_http_client(
    timeout: Duration,
    custom_proxy: Option<&str>,
    use_system_proxy: bool,
    allow_insecure_tls: bool,
) -> Result<AsyncClient> {
    build_http_client_inner(timeout, custom_proxy, use_system_proxy, allow_insecure_tls)
}

impl crate::traits::HttpClient for DefaultHttpClient {
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl std::future::Future<Output = Result<HttpResponse>> + Send + '_ {
        self.send(request)
    }
}

pub(crate) async fn sleep_for_retry(
    duration: Duration,
    cancellation_token: Option<&CancellationToken>,
) -> Result<()> {
    let Some(cancellation_token) = cancellation_token else {
        tokio::time::sleep(duration).await;
        return Ok(());
    };
    let started = std::time::Instant::now();
    loop {
        cancellation_token.check()?;
        let elapsed = started.elapsed();
        if elapsed >= duration {
            return Ok(());
        }
        tokio::time::sleep(
            duration
                .saturating_sub(elapsed)
                .min(Duration::from_millis(50)),
        )
        .await;
    }
}

pub(crate) fn apply_request_headers(
    mut request: AsyncRequestBuilder,
    headers: &BTreeMap<String, String>,
) -> AsyncRequestBuilder {
    for (key, value) in headers {
        let Ok(name) = HeaderName::from_bytes(key.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        request = request.header(name, value);
    }
    request
}

fn redact_proxy_error_message(proxy: &str, message: &str) -> String {
    message.replace(proxy, &redact_proxy_url(proxy))
}

fn redact_proxy_url(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let authority_start = scheme_end + 3;
    let Some(at_offset) = value[authority_start..].find('@') else {
        return value.to_string();
    };
    let at = authority_start + at_offset;
    format!("{}***@{}", &value[..authority_start], &value[at + 1..])
}

fn is_redirect(status: u16) -> bool {
    (300..=399).contains(&status)
}

fn response_headers(headers_map: &HeaderMap) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    for (name, value) in headers_map {
        if let Ok(value) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), value.to_string());
        }
    }
    headers
}

fn format_response_headers(headers: &HeaderMap) -> String {
    let mut text = String::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            text.push_str(name.as_str());
            text.push_str(": ");
            text.push_str(value);
            text.push('\n');
        }
    }
    text.trim_end().to_string()
}

fn format_source_http_request_headers(headers: &BTreeMap<String, String>) -> String {
    let mut lines = vec![
        "Accept-Encoding: gzip, deflate".to_string(),
        "Cache-Control: no-cache".to_string(),
    ];
    lines.extend(headers.iter().map(|(key, value)| format!("{key}: {value}")));
    lines.join("\n")
}

fn decode_content_body(body: Vec<u8>, headers: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let Some(encoding) = headers.get("content-encoding") else {
        return Ok(body);
    };
    let mut decoded = body;
    for value in encoding
        .split(',')
        .map(|value| value.trim().to_ascii_lowercase())
    {
        decoded = match value.as_str() {
            "" | "identity" => decoded,
            "gzip" if decoded.starts_with(&[0x1f, 0x8b]) => {
                decode_with(GzDecoder::new(&decoded[..]))?
            }
            "gzip" => decoded,
            "deflate" => decode_deflate(decoded)?,
            "br" => match decode_with(Decompressor::new(&decoded[..], 4096)) {
                Ok(decoded_body) => decoded_body,
                Err(_) => decoded,
            },
            _ => decoded,
        };
    }
    Ok(decoded)
}

fn decode_deflate(body: Vec<u8>) -> Result<Vec<u8>> {
    match decode_with(ZlibDecoder::new(&body[..])) {
        Ok(decoded) => Ok(decoded),
        Err(_) => decode_with(DeflateDecoder::new(&body[..])).or(Ok(body)),
    }
}

fn decode_with(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut decoded = Vec::new();
    reader.read_to_end(&mut decoded)?;
    Ok(decoded)
}

async fn read_source_probe_body(
    mut response: AsyncResponse,
    transport: &DefaultHttpClient,
) -> Result<Vec<u8>> {
    const PROBE_SIZE: usize = 188 * 5;
    let mut body = Vec::new();
    while body.len() < PROBE_SIZE {
        let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| Error::http(transport.redact_error(error.to_string())))?
        else {
            return Ok(body);
        };
        let remaining = PROBE_SIZE.saturating_sub(body.len());
        let take = remaining.min(chunk.len());
        body.extend_from_slice(&chunk[..take]);
        if source_probe_is_mpeg2_ts(&body) || source_probe_looks_binary(&body) {
            return Ok(body);
        }
        if take < chunk.len() {
            body.extend_from_slice(&chunk[take..]);
            break;
        }
    }
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| Error::http(transport.redact_error(error.to_string())))?
    {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn source_probe_is_mpeg2_ts(buffer: &[u8]) -> bool {
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

fn source_probe_looks_binary(data: &[u8]) -> bool {
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
        let seq_len = source_probe_utf8_sequence_length(byte);
        if seq_len > 1
            && index + seq_len <= data.len()
            && source_probe_valid_utf8_sequence(&data[index..index + seq_len])
        {
            index += seq_len;
            continue;
        }
        non_text += 1;
        index += 1;
    }
    (non_text as f64 / total as f64) > 0.3
}

fn source_probe_utf8_sequence_length(byte: u8) -> usize {
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

fn source_probe_valid_utf8_sequence(seq: &[u8]) -> bool {
    if seq.len() <= 1 {
        return false;
    }
    seq.iter().skip(1).all(|byte| byte & 0xc0 == 0x80)
}

fn resolve_redirect(base: &str, location: &str) -> String {
    if let Ok(base_url) = reqwest::Url::parse(base)
        && let Ok(joined) = base_url.join(location)
    {
        return joined.to_string();
    }
    if reqwest::Url::parse(location).is_ok() {
        return location.to_string();
    }
    let Some((origin, base_path)) = split_url_origin_and_path(base) else {
        return location.to_string();
    };
    let (location_path, suffix) = split_path_suffix(location);
    let joined = if location_path.starts_with('/') {
        normalize_url_path(location_path)
    } else {
        let base_dir = base_path
            .rsplit_once('/')
            .map(|(prefix, _)| format!("{prefix}/"))
            .unwrap_or_else(|| "/".to_string());
        normalize_url_path(&format!("{base_dir}{location_path}"))
    };
    format!("{origin}{joined}{suffix}")
}

fn split_url_origin_and_path(url: &str) -> Option<(&str, &str)> {
    let scheme_end = url.find("://")?;
    let after_scheme = scheme_end + 3;
    let rest = url.get(after_scheme..)?;
    let path_start = rest.find('/').map(|index| after_scheme + index);
    let index = path_start.unwrap_or(url.len());
    let origin = url.get(..index)?;
    let path = url.get(index..).unwrap_or("/");
    Some((origin, path))
}

fn split_path_suffix(value: &str) -> (&str, &str) {
    let query = value.find('?');
    let fragment = value.find('#');
    let split = match (query, fragment) {
        (Some(left), Some(right)) => left.min(right),
        (Some(index), None) | (None, Some(index)) => index,
        (None, None) => value.len(),
    };
    (&value[..split], &value[split..])
}

fn normalize_url_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                let _ = parts.pop();
            }
            value => parts.push(value),
        }
    }
    format!("/{}", parts.join("/"))
}
