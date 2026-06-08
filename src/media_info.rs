//! ffmpeg media-info probing shared by download and session logic.

use std::path::Path;
use std::process::Stdio;

use crate::error::{Error, Result};
use crate::mux::MediaInfo;

pub(crate) fn media_info_console_label(info: &MediaInfo) -> String {
    let id = info.id.as_deref().unwrap_or("NaN");
    let fields = [
        info.media_type.as_deref(),
        info.base_info.as_deref(),
        info.resolution.as_deref(),
        info.fps.as_deref(),
        info.bitrate.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.is_empty())
    .collect::<Vec<_>>();
    let mut label = format!("{id}: {}", fields.join(", "));
    if info.hdr && !info.dolby_vision {
        label.push_str(" [HDR]");
    }
    if info.dolby_vision {
        label.push_str(" [DOVI]");
    }
    label
}

pub(crate) async fn probe_ffmpeg_media_infos(binary: &Path, file: &Path) -> Result<Vec<MediaInfo>> {
    if file.as_os_str().is_empty() || tokio::fs::metadata(file).await.is_err() {
        return Ok(Vec::new());
    }
    let output = tokio::process::Command::new(binary)
        .arg("-hide_banner")
        .arg("-i")
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| Error::compatibility(error.to_string()))?;
    parse_ffmpeg_media_infos(&String::from_utf8_lossy(&output.stderr))
}

fn parse_ffmpeg_media_infos(output: &str) -> Result<Vec<MediaInfo>> {
    let stream_re = regex::Regex::new(r"^  Stream #.*")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let id_re = regex::Regex::new(r"#0:\d(\[0x\w+?\])")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let type_re = regex::Regex::new(r": (\w+): (.*)")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let base_re = regex::Regex::new(r"(.*?)(,|$)")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let replace_re =
        regex::Regex::new(r" / 0x\w+").map_err(|error| Error::compatibility(error.to_string()))?;
    let resolution_re = regex::Regex::new(r"\d{2,}x\d+")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let bitrate_re =
        regex::Regex::new(r"\d+ kb/s").map_err(|error| Error::compatibility(error.to_string()))?;
    let fps_re = regex::Regex::new(r"(\d+(\.\d+)?) fps")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let dovi_re =
        regex::Regex::new(r"DOVI configuration record.*profile: (\d).*compatibility id: (\d)")
            .map_err(|error| Error::compatibility(error.to_string()))?;
    let start_re = regex::Regex::new(r"Duration.*?start: (\d+\.?\d{0,3})")
        .map_err(|error| Error::compatibility(error.to_string()))?;
    let start_time_millis = start_re
        .captures(output)
        .and_then(|captures| captures.get(1))
        .and_then(|capture| parse_seconds_to_millis(capture.as_str()));

    let mut infos = Vec::new();
    for line in output.lines().filter(|line| stream_re.is_match(line)) {
        let captures = type_re.captures(line);
        let media_type = captures
            .as_ref()
            .and_then(|captures| captures.get(1))
            .map(|capture| capture.as_str().to_string());
        let text = captures
            .as_ref()
            .and_then(|captures| captures.get(2))
            .map(|capture| capture.as_str().trim_end().to_string());
        let base_info = text.as_deref().and_then(|text| {
            base_re
                .captures(text)
                .and_then(|captures| captures.get(1))
                .map(|capture| replace_re.replace_all(capture.as_str(), "").to_string())
        });
        let dolby_vision = base_info.as_deref().is_some_and(|base| {
            base.contains("dvhe") || base.contains("dvh1") || base.contains("DOVI")
        }) || media_type
            .as_deref()
            .is_some_and(|value| value.contains("dvvideo"))
            || (dovi_re.is_match(output) && media_type.as_deref() == Some("Video"));
        let hdr = text
            .as_deref()
            .is_some_and(|value| value.contains("/bt2020/"));
        infos.push(MediaInfo {
            id: id_re
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|capture| capture.as_str().to_string()),
            text: text.clone(),
            base_info,
            bitrate: text.as_deref().and_then(|text| {
                bitrate_re
                    .find(text)
                    .map(|matched| matched.as_str().to_string())
            }),
            resolution: text.as_deref().and_then(|text| {
                resolution_re
                    .find(text)
                    .map(|matched| matched.as_str().to_string())
            }),
            fps: text.as_deref().and_then(|text| {
                fps_re
                    .find(text)
                    .map(|matched| matched.as_str().to_string())
            }),
            media_type,
            start_time_millis,
            dolby_vision,
            hdr,
        });
    }

    if infos.is_empty() {
        infos.push(MediaInfo {
            media_type: Some("Unknown".to_string()),
            ..MediaInfo::default()
        });
    }
    Ok(infos)
}

fn parse_seconds_to_millis(value: &str) -> Option<i64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole_ms = whole.trim().parse::<i64>().ok()?.checked_mul(1000)?;
    let mut frac = 0_i64;
    let mut scale = 100_i64;
    for ch in fraction.chars().take(3) {
        if !ch.is_ascii_digit() {
            break;
        }
        frac += i64::from(ch as u8 - b'0') * scale;
        scale /= 10;
    }
    whole_ms.checked_add(frac)
}
