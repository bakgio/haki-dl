//! Stream display labels for logs, prompts, and progress rows.

use std::collections::BTreeSet;

use crate::manifest::{EncryptionMethod, MediaType, RoleType, Stream};

pub(crate) fn stream_short_label(stream: &Stream) -> String {
    match stream.media_type {
        Some(MediaType::Audio) => compact_label(
            "Aud",
            [
                bitrate_kbps(stream.bandwidth),
                stream.name.clone(),
                stream.language.clone(),
                channel_label(stream.channels.as_deref()),
                role_label(stream.role),
            ],
        ),
        Some(MediaType::Subtitles) | Some(MediaType::ClosedCaptions) => compact_label(
            "Sub",
            [
                stream.language.clone(),
                stream.name.clone(),
                stream.codecs.clone(),
                role_label(stream.role),
                None,
            ],
        ),
        Some(MediaType::Video) | None => compact_label(
            "Vid",
            [
                stream.resolution.clone(),
                video_bitrate_kbps(stream.bandwidth),
                frame_rate_label(stream.frame_rate),
                stream.video_range.clone(),
                role_label(stream.role),
            ],
        ),
    }
}

pub(crate) fn stream_full_label(stream: &Stream) -> String {
    match stream.media_type {
        Some(MediaType::Audio) => compact_label_with_encryption(
            "Aud",
            encrypted_label(stream),
            [
                stream.group_id.clone(),
                bitrate_kbps(stream.bandwidth),
                stream.name.clone(),
                stream.codecs.clone(),
                stream.language.clone(),
                channel_label(stream.channels.as_deref()),
                segment_count_label(stream.segments_count()),
                role_label(stream.role),
                duration_label(stream.total_duration()),
            ],
        ),
        Some(MediaType::Subtitles) | Some(MediaType::ClosedCaptions) => {
            compact_label_with_encryption(
                "Sub",
                encrypted_label(stream),
                [
                    stream.group_id.clone(),
                    stream.language.clone(),
                    stream.name.clone(),
                    stream.codecs.clone(),
                    stream.characteristics.clone(),
                    segment_count_label(stream.segments_count()),
                    role_label(stream.role),
                    duration_label(stream.total_duration()),
                ],
            )
        }
        Some(MediaType::Video) | None => compact_label_with_encryption(
            "Vid",
            encrypted_label(stream),
            [
                stream.resolution.clone(),
                video_bitrate_kbps(stream.bandwidth),
                stream.group_id.clone(),
                frame_rate_label(stream.frame_rate),
                stream.codecs.clone(),
                stream.video_range.clone(),
                segment_count_label(stream.segments_count()),
                role_label(stream.role),
                duration_label(stream.total_duration()),
            ],
        ),
    }
}

pub(crate) fn stream_download_label(stream: &Stream) -> String {
    match stream.media_type {
        Some(MediaType::Audio) => compact_label(
            "Aud",
            [
                stream.group_id.clone(),
                bitrate_kbps(stream.bandwidth),
                stream.name.clone(),
                stream.codecs.clone(),
                stream.language.clone(),
                channel_label(stream.channels.as_deref()),
                role_label(stream.role),
            ],
        ),
        Some(MediaType::Subtitles) | Some(MediaType::ClosedCaptions) => compact_label(
            "Sub",
            [
                stream.group_id.clone(),
                stream.language.clone(),
                stream.name.clone(),
                stream.codecs.clone(),
                role_label(stream.role),
                None,
                None,
            ],
        ),
        Some(MediaType::Video) | None => compact_label(
            "Vid",
            [
                stream.resolution.clone(),
                video_bitrate_kbps(stream.bandwidth),
                stream.group_id.clone(),
                frame_rate_label(stream.frame_rate),
                stream.codecs.clone(),
                stream.video_range.clone(),
                role_label(stream.role),
            ],
        ),
    }
}

fn compact_label<const N: usize>(prefix: &str, fields: [Option<String>; N]) -> String {
    compact_label_with_encryption(prefix, None, fields)
}

fn compact_label_with_encryption<const N: usize>(
    prefix: &str,
    encryption: Option<String>,
    fields: [Option<String>; N],
) -> String {
    let body = fields
        .into_iter()
        .flatten()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    match (
        encryption.filter(|value| !value.trim().is_empty()),
        body.is_empty(),
    ) {
        (Some(encryption), true) => format!("{prefix} {}", encryption.trim()),
        (Some(encryption), false) => format!("{prefix} {} {body}", encryption.trim()),
        (None, true) => prefix.to_string(),
        (None, false) => format!("{prefix} {body}"),
    }
}

fn bitrate_kbps(value: Option<i64>) -> Option<String> {
    value.filter(|value| *value > 0).map(|value| {
        let kbps = value / 1000;
        format!("{kbps} Kbps")
    })
}

fn video_bitrate_kbps(value: Option<i64>) -> Option<String> {
    Some(
        value
            .filter(|value| *value > 0)
            .map(|value| format!("{} Kbps", value / 1000))
            .unwrap_or_else(|| "Kbps".to_string()),
    )
}

fn frame_rate_label(value: Option<f64>) -> Option<String> {
    value.filter(|value| value.is_finite()).map(|value| {
        if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            value.to_string()
        }
    })
}

fn channel_label(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.to_ascii_uppercase().ends_with("CH") {
                value.to_string()
            } else {
                format!("{value}CH")
            }
        })
}

fn role_label(value: Option<RoleType>) -> Option<String> {
    value.map(|role| match role {
        RoleType::Subtitle => "Subtitle".to_string(),
        RoleType::Main => "Main".to_string(),
        RoleType::Alternate => "Alternate".to_string(),
        RoleType::Supplementary => "Supplementary".to_string(),
        RoleType::Commentary => "Commentary".to_string(),
        RoleType::Dub => "Dub".to_string(),
        RoleType::Description => "Description".to_string(),
        RoleType::Sign => "Sign".to_string(),
        RoleType::Metadata => "Metadata".to_string(),
        RoleType::ForcedSubtitle => "ForcedSubtitle".to_string(),
        RoleType::Numeric(value) => value.to_string(),
    })
}

fn encrypted_label(stream: &Stream) -> Option<String> {
    let playlist = stream.playlist.as_ref()?;
    let methods = playlist
        .media_init
        .iter()
        .chain(
            playlist
                .media_parts
                .iter()
                .flat_map(|part| part.media_segments.iter()),
        )
        .map(|segment| segment.encryption.method)
        .filter(|method| *method != EncryptionMethod::None)
        .map(encryption_method_label)
        .collect::<BTreeSet<_>>();
    if methods.is_empty() {
        None
    } else {
        Some(format!(
            "*{}",
            methods.into_iter().collect::<Vec<_>>().join(",")
        ))
    }
}

fn encryption_method_label(method: EncryptionMethod) -> &'static str {
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

fn segment_count_label(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 Segment".to_string()),
        value => Some(format!("{value} Segments")),
    }
}

fn duration_label(value: Option<f64>) -> Option<String> {
    let seconds = value?.trunc();
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some(format!("~{}", format_duration_narrow(seconds as u64)))
}

fn format_duration_narrow(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours == 0 {
        format!("{minutes:02}m{seconds:02}s")
    } else {
        format!("{hours:02}h{minutes:02}m{seconds:02}s")
    }
}
