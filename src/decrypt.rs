//! Decryption helpers and external decrypt command planning.

use std::path::{Path, PathBuf};

use aes::Aes128;
use chacha20::ChaCha20;
use cipher::block_padding::Pkcs7;
use cipher::{BlockDecryptMut, KeyInit, KeyIvInit, StreamCipher};

use crate::config::{CustomKey, DecryptionEngine};
use crate::error::{Error, Result};
use crate::manifest::EncryptionMethod;

const ZERO_KID: &str = "00000000000000000000000000000000";
const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d, 0x21, 0xed,
];
const PLAYREADY_SYSTEM_ID: [u8; 16] = [
    0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88, 0x5f, 0x95,
];
const FAIRPLAY_SYSTEM_ID: [u8; 16] = [
    0x94, 0xce, 0x86, 0xfb, 0x07, 0xff, 0x4f, 0x43, 0xad, 0xb8, 0x93, 0xd2, 0xfa, 0x96, 0x8c, 0xa2,
];

/// Parsed MP4 initialization encryption metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mp4ProtectionInfo {
    /// First retained PSSH payload in base64, kept for compatibility.
    pub pssh: Option<String>,
    /// DRM system identified for the first retained PSSH payload.
    pub pssh_system: Option<PsshSystem>,
    /// Retained PSSH payloads for recognized DRM system boxes.
    pub psshs: Vec<PsshInfo>,
    /// KID in lowercase hex.
    pub kid: Option<String>,
    /// Common-encryption scheme such as cenc/cbcs when present.
    pub scheme: Option<String>,
    /// Whether a Widevine PSSH identified a multi-DRM init.
    pub is_multi_drm: bool,
}

/// DRM system identified for a retained PSSH payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PsshSystem {
    /// Widevine system ID.
    Widevine,
    /// PlayReady system ID.
    PlayReady,
    /// FairPlay system ID.
    FairPlay,
}

/// Parsed PSSH payload associated with a recognized DRM system ID.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PsshInfo {
    /// DRM system identified by the PSSH system ID.
    pub system: PsshSystem,
    /// PSSH data payload in base64.
    pub data: String,
}

/// Selected key material for an MP4 decrypt command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedKey {
    /// Optional track ID override.
    pub track_id: Option<String>,
    /// Normalized key pair as `kid:key` or `key`.
    pub key_pair: String,
}

/// Redacted external decrypt command plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalDecryptPlan {
    /// Requested decrypt engine.
    pub engine: DecryptionEngine,
    /// Program path.
    pub program: PathBuf,
    /// Redacted command arguments suitable for logs and diagnostics.
    pub redacted_arguments: String,
    /// Optional working directory.
    pub work_dir: Option<PathBuf>,
    /// Whether source and init must be concatenated before running the engine.
    pub requires_init_concat: bool,
}

/// Input for planning an external MP4 decrypt command.
#[derive(Clone, Copy, Debug)]
pub struct ExternalDecryptRequest<'a> {
    /// Requested decrypt engine.
    pub engine: DecryptionEngine,
    /// Program path.
    pub program: &'a Path,
    /// Available key pairs in engine-compatible text form.
    pub keys: &'a [String],
    /// Encrypted source path.
    pub source: &'a Path,
    /// Decrypted destination path.
    pub dest: &'a Path,
    /// Optional KID in lowercase hex.
    pub kid: Option<&'a str>,
    /// Optional initialization segment path.
    pub init: Option<&'a Path>,
    /// Whether the source metadata indicates multi-DRM handling.
    pub is_multi_drm: bool,
}

/// Decrypts AES-128-CBC data with PKCS padding.
pub fn aes_128_cbc_decrypt(encrypted: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>> {
    type Aes128CbcDec = cbc::Decryptor<Aes128>;
    let decryptor = Aes128CbcDec::new_from_slices(key, iv)
        .map_err(|_| Error::decrypt("AES-128-CBC key or IV length is invalid"))?;
    decryptor
        .decrypt_padded_vec_mut::<Pkcs7>(encrypted)
        .map_err(|_| Error::decrypt("AES-128-CBC decrypt failed"))
}

/// Decrypts AES-128-ECB data with PKCS padding.
pub fn aes_128_ecb_decrypt(encrypted: &[u8], key: &[u8]) -> Result<Vec<u8>> {
    type Aes128EcbDec = ecb::Decryptor<Aes128>;
    let decryptor = Aes128EcbDec::new_from_slice(key)
        .map_err(|_| Error::decrypt("AES-128-ECB key length is invalid"))?;
    decryptor
        .decrypt_padded_vec_mut::<Pkcs7>(encrypted)
        .map_err(|_| Error::decrypt("AES-128-ECB decrypt failed"))
}

/// Applies the in-place HLS segment decrypt behavior for supported methods.
pub fn decrypt_hls_segment_bytes(
    method: EncryptionMethod,
    encrypted: &[u8],
    key: Option<&[u8]>,
    iv: Option<&[u8]>,
) -> Result<Vec<u8>> {
    match method {
        EncryptionMethod::Aes128 => aes_128_cbc_decrypt(
            encrypted,
            key.ok_or_else(|| Error::decrypt("AES key is missing"))?,
            iv.ok_or_else(|| Error::decrypt("AES IV is missing"))?,
        ),
        EncryptionMethod::Aes128Ecb => aes_128_ecb_decrypt(
            encrypted,
            key.ok_or_else(|| Error::decrypt("AES key is missing"))?,
        ),
        EncryptionMethod::Chacha20 => chacha20_decrypt_per_1024_bytes(
            encrypted,
            key.ok_or_else(|| Error::decrypt("ChaCha20 key is missing"))?,
            iv.ok_or_else(|| Error::decrypt("ChaCha20 nonce is missing"))?,
        ),
        EncryptionMethod::SampleAesCtr => Ok(encrypted.to_vec()),
        _ => Ok(encrypted.to_vec()),
    }
}

/// Decrypts a file in place with an HLS segment decrypt method.
pub async fn decrypt_hls_segment_file(
    path: &Path,
    method: EncryptionMethod,
    key: Option<&[u8]>,
    iv: Option<&[u8]>,
) -> Result<()> {
    let encrypted = tokio::fs::read(path).await?;
    let decrypted = decrypt_hls_segment_bytes(method, &encrypted, key, iv)?;
    tokio::fs::write(path, decrypted).await?;
    Ok(())
}

/// Decrypts data by reinitializing ChaCha20 at counter zero for every 1024-byte chunk.
pub fn chacha20_decrypt_per_1024_bytes(
    encrypted: &[u8],
    key: &[u8],
    nonce: &[u8],
) -> Result<Vec<u8>> {
    if key.len() != 32 {
        return Err(Error::decrypt("ChaCha20 key must be 32 bytes"));
    }
    let nonce = match nonce.len() {
        12 => nonce.to_vec(),
        8 => {
            let mut padded = vec![0, 0, 0, 0];
            padded.extend(nonce);
            padded
        }
        _ => return Err(Error::decrypt("ChaCha20 nonce must be 12 or 8 bytes")),
    };
    let mut output = Vec::with_capacity(encrypted.len());
    for chunk in encrypted.chunks(1024) {
        let mut buffer = chunk.to_vec();
        let mut cipher = ChaCha20::new_from_slices(key, &nonce)
            .map_err(|_| Error::decrypt("ChaCha20 key or nonce length is invalid"))?;
        cipher.apply_keystream(&mut buffer);
        output.extend(buffer);
    }
    Ok(output)
}

/// Converts typed custom key entries into the string form consumed by external engines.
pub fn custom_keys_to_pairs(keys: &[CustomKey]) -> Vec<String> {
    keys.iter()
        .map(|key| match key {
            CustomKey::Track { track_id, key_hex } => format!("{track_id}:{key_hex}"),
            CustomKey::Kid { kid_hex, key_hex } => format!("{kid_hex}:{key_hex}"),
            CustomKey::Key { key_hex } => key_hex.clone(),
        })
        .collect()
}

/// Searches a key text file for a line that starts with the requested KID.
pub async fn search_key_text_file(
    path: Option<&Path>,
    kid: Option<&str>,
) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(kid) = kid.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let Ok(bytes) = tokio::fs::read(path).await else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(kid))
        .map(str::to_string))
}

/// Reads MP4 protection metadata from initialization bytes.
pub fn read_mp4_protection_info(data: &[u8]) -> Mp4ProtectionInfo {
    let mut info = Mp4ProtectionInfo::default();
    scan_boxes(data, &mut info);
    info
}

/// Selects a key pair using MP4 decrypt compatibility rules.
pub fn select_key_pair(
    keys: &[String],
    kid: Option<&str>,
    is_multi_drm: bool,
) -> Option<SelectedKey> {
    if keys.is_empty() {
        return None;
    }
    let mut track_id = if is_multi_drm {
        Some("1".to_string())
    } else {
        None
    };
    let kid = kid.unwrap_or_default();
    let mut key_pair = keys
        .iter()
        .find(|key| !kid.is_empty() && key.starts_with(kid))
        .cloned();
    if kid == ZERO_KID {
        key_pair = keys.first().cloned();
        track_id = Some("1".to_string());
    }
    if key_pair.is_none() && keys.len() == 1 && !keys[0].contains(':') {
        key_pair = Some(format!("{kid}:{}", keys[0]));
    }
    key_pair.map(|key_pair| SelectedKey { track_id, key_pair })
}

/// Plans an external MP4 decrypt command with redacted arguments.
pub fn plan_external_decrypt(
    request: ExternalDecryptRequest<'_>,
) -> Result<Option<ExternalDecryptPlan>> {
    let ExternalDecryptRequest {
        engine,
        program,
        keys,
        source,
        dest,
        kid,
        init,
        is_multi_drm,
    } = request;
    let Some(selected) = select_key_pair(keys, kid, is_multi_drm) else {
        return Ok(None);
    };
    if engine == DecryptionEngine::Mp4forge {
        return Ok(None);
    }
    if source
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.ends_with("_init.mp4"))
        && engine != DecryptionEngine::Mp4decrypt
    {
        return Ok(None);
    }
    let program = program.to_path_buf();
    let key_value = selected
        .key_pair
        .split_once(':')
        .map(|(_, key)| key)
        .unwrap_or(selected.key_pair.as_str());
    let kid = kid.unwrap_or_default();
    let requires_init_concat = init.is_some() && engine != DecryptionEngine::Mp4decrypt;
    let redacted_arguments = match engine {
        DecryptionEngine::Mp4forge => return Ok(None),
        DecryptionEngine::ShakaPackager => {
            let key_id = selected
                .track_id
                .as_deref()
                .map(|_| ZERO_KID)
                .or_else(|| {
                    selected
                        .key_pair
                        .split_once(':')
                        .map(|(key_id, _)| key_id)
                        .filter(|key_id| !key_id.is_empty())
                })
                .unwrap_or(kid);
            let label = selected
                .track_id
                .as_deref()
                .map(|track_id| format!("label={track_id}:"))
                .unwrap_or_default();
            format!(
                "--quiet --enable_raw_key_decryption input=\"{}\",stream=0,output=\"{}\" --keys {label}key_id={key_id}:key={}",
                source.display(),
                dest.display(),
                redact_key(key_value)
            )
        }
        DecryptionEngine::Mp4decrypt => {
            let key_args = if let Some(track_id) = &selected.track_id {
                keys.iter()
                    .filter_map(|key| key.split_once(':').map(|(_, key)| key))
                    .map(|key| format!("--key {track_id}:{}", redact_key(key)))
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                keys.iter()
                    .map(|key| redact_key_pair_argument(key))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            format!(
                "{key_args} \"{}\" \"{}\"",
                source
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default(),
                dest.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
            )
        }
        DecryptionEngine::Ffmpeg => format!(
            "-loglevel error -nostdin -decryption_key {} -i \"{}\" -c copy \"{}\"",
            redact_key(key_value),
            source.display(),
            dest.display()
        ),
    };
    Ok(Some(ExternalDecryptPlan {
        engine,
        program,
        redacted_arguments,
        work_dir: if engine == DecryptionEngine::Mp4decrypt {
            source.parent().map(Path::to_path_buf)
        } else {
            None
        },
        requires_init_concat,
    }))
}

/// Redacts keys and common signed URL query values from diagnostics.
pub fn redact_secrets(input: &str) -> String {
    let mut output = input.to_string();
    for marker in ["key=", "decryption_key ", "--key "] {
        output = redact_after_marker(&output, marker);
    }
    for query in ["token=", "signature=", "sig=", "Policy=", "Key-Pair-Id="] {
        output = redact_query_value(&output, query);
    }
    output
}

fn scan_boxes(data: &[u8], info: &mut Mp4ProtectionInfo) {
    let mut offset = 0_usize;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        if size < 8 || offset + size > data.len() {
            offset += 1;
            continue;
        }
        let box_type = &data[offset + 4..offset + 8];
        let payload = &data[offset + 8..offset + size];
        if box_type == b"pssh" {
            read_pssh(payload, info);
        } else if matches!(
            box_type,
            b"moov"
                | b"trak"
                | b"mdia"
                | b"minf"
                | b"stbl"
                | b"stsd"
                | b"encv"
                | b"enca"
                | b"enct"
                | b"encs"
                | b"sinf"
                | b"schi"
        ) {
            read_encryption_sample_entry(payload, info);
            scan_boxes(payload, info);
        }
        offset += size;
    }
}

fn read_pssh(payload: &[u8], info: &mut Mp4ProtectionInfo) {
    if payload.len() < 24 {
        return;
    }
    let version = payload[0];
    if version > 1 {
        return;
    }
    let system_id = &payload[4..20];
    let Some(system) = pssh_system(system_id) else {
        return;
    };
    let mut data_size_offset = 20;
    if version == 1 {
        let Some(kid_count_bytes) = payload.get(data_size_offset..data_size_offset + 4) else {
            return;
        };
        let kid_count = u32::from_be_bytes([
            kid_count_bytes[0],
            kid_count_bytes[1],
            kid_count_bytes[2],
            kid_count_bytes[3],
        ]) as usize;
        data_size_offset += 4 + kid_count.saturating_mul(16);
    }
    let Some(size_bytes) = payload.get(data_size_offset..data_size_offset + 4) else {
        return;
    };
    let data_size =
        u32::from_be_bytes([size_bytes[0], size_bytes[1], size_bytes[2], size_bytes[3]]) as usize;
    let Some(pssh_data) = payload.get(data_size_offset + 4..data_size_offset + 4 + data_size)
    else {
        return;
    };
    let data = base64_encode(pssh_data);
    if info.pssh.is_none() {
        info.pssh = Some(data.clone());
        info.pssh_system = Some(system);
    }
    info.psshs.push(PsshInfo { system, data });
    if system == PsshSystem::Widevine
        && info.kid.as_deref() == Some(ZERO_KID)
        && let Some(kid) = pssh_data.get(2..18)
    {
        info.kid = Some(bytes_to_hex_lower(kid));
        info.is_multi_drm = true;
    }
}

fn pssh_system(system_id: &[u8]) -> Option<PsshSystem> {
    match system_id {
        value if value == WIDEVINE_SYSTEM_ID.as_slice() => Some(PsshSystem::Widevine),
        value if value == PLAYREADY_SYSTEM_ID.as_slice() => Some(PsshSystem::PlayReady),
        value if value == FAIRPLAY_SYSTEM_ID.as_slice() => Some(PsshSystem::FairPlay),
        _ => None,
    }
}

fn read_encryption_sample_entry(data: &[u8], info: &mut Mp4ProtectionInfo) {
    if let Some(index) = find_subslice(data, b"schm")
        && let Some(scheme) = data.get(index + 8..index + 12)
    {
        info.scheme = Some(String::from_utf8_lossy(scheme).to_string());
    }
    if let Some(index) = find_subslice(data, b"tenc")
        && let Some(kid) = data.get(index + 12..index + 28)
    {
        info.kid = Some(bytes_to_hex_lower(kid));
    }
}

fn find_subslice(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .position(|window| window == needle)
}

fn redact_key_pair_argument(key: &str) -> String {
    match key.split_once(':') {
        Some((kid, value)) => format!("--key {kid}:{}", redact_key(value)),
        None => format!("--key {}", redact_key(key)),
    }
}

fn redact_key(value: &str) -> String {
    if value.is_empty() {
        "<redacted>".to_string()
    } else {
        format!("<redacted:{}>", value.len())
    }
}

fn redact_after_marker(input: &str, marker: &str) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(index) = rest.find(marker) {
        let before = rest.get(..index).unwrap_or_default();
        output.push_str(before);
        output.push_str(marker);
        output.push_str("<redacted>");
        let after_marker = index + marker.len();
        let tail = rest.get(after_marker..).unwrap_or_default();
        let end = tail
            .find(|ch: char| ch.is_whitespace() || ch == '&' || ch == ',' || ch == '"')
            .unwrap_or(tail.len());
        rest = tail.get(end..).unwrap_or_default();
    }
    output.push_str(rest);
    output
}

fn redact_query_value(input: &str, marker: &str) -> String {
    let mut output = String::new();
    let mut rest = input;
    while let Some(index) = rest.find(marker) {
        output.push_str(rest.get(..index).unwrap_or_default());
        output.push_str(marker);
        output.push_str("<redacted>");
        let tail = rest.get(index + marker.len()..).unwrap_or_default();
        let end = tail.find('&').unwrap_or(tail.len());
        rest = tail.get(end..).unwrap_or_default();
    }
    output.push_str(rest);
    output
}

fn bytes_to_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        output.push(char::from(TABLE[((triple >> 18) & 0x3f) as usize]));
        output.push(char::from(TABLE[((triple >> 12) & 0x3f) as usize]));
        if chunk.len() > 1 {
            output.push(char::from(TABLE[((triple >> 6) & 0x3f) as usize]));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(char::from(TABLE[(triple & 0x3f) as usize]));
        } else {
            output.push('=');
        }
    }
    output
}
