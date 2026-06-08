//! Subtitle parsing, conversion, extraction, and image helpers.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;

use crate::config::SubtitleFormat;
use crate::error::{Error, Result};

#[cfg(windows)]
const NEWLINE: &str = "\r\n";
#[cfg(not(windows))]
const NEWLINE: &str = "\n";

/// One subtitle cue with millisecond timestamps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleCue {
    /// Cue start time in milliseconds.
    pub start_ms: i64,
    /// Cue end time in milliseconds.
    pub end_ms: i64,
    /// Cue payload text.
    pub payload: String,
    /// Cue settings text.
    pub settings: String,
}

/// Parsed WebVTT-compatible subtitle document.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WebVttSubtitle {
    /// Parsed cues.
    pub cues: Vec<SubtitleCue>,
    /// MPEG-TS timestamp value from `X-TIMESTAMP-MAP`.
    pub mpegts_timestamp: i64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct WvttExtraction {
    pub(crate) subtitle: WebVttSubtitle,
    pub(crate) console_lines: Vec<String>,
}

/// Extracted image subtitle file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubtitleImage {
    /// Written image path.
    pub path: PathBuf,
    /// Cue index that referenced the image.
    pub cue_index: usize,
}

/// Parses a WebVTT document.
pub fn parse_webvtt(text: &str, base_timestamp_ms: i64) -> Result<WebVttSubtitle> {
    if !text.trim().starts_with("WEBVTT") {
        return Err(Error::subtitle("invalid WebVTT header"));
    }

    let mut subtitle = WebVttSubtitle::default();
    let timestamp_map =
        Regex::new(r"X-TIMESTAMP-MAP.*").map_err(|error| Error::subtitle(error.to_string()))?;
    let timestamp_value =
        Regex::new(r"MPEGTS:(\d+)").map_err(|error| Error::subtitle(error.to_string()))?;
    if let Some(line) = timestamp_map.find(text)
        && let Some(captures) = timestamp_value.captures(line.as_str())
        && let Some(value) = captures.get(1)
    {
        subtitle.mpegts_timestamp = value.as_str().parse::<i64>().unwrap_or_default();
    }

    let mut need_payload = false;
    let mut time_line = String::new();
    let mut payloads = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if line.contains(" --> ") {
            need_payload = true;
            time_line = line.trim().to_string();
            continue;
        }
        if !need_payload {
            continue;
        }
        if line.trim().is_empty() {
            let payload = payloads.join(NEWLINE);
            if !payload.trim().is_empty() {
                let cue = parse_vtt_cue(&time_line, &payload)?;
                subtitle.cues.push(cue);
            }
            payloads.clear();
            need_payload = false;
        } else {
            payloads.push(line.trim().to_string());
        }
    }

    if base_timestamp_ms != 0 {
        subtitle.apply_base_timestamp(base_timestamp_ms);
    }

    Ok(subtitle)
}

/// Parses WebVTT bytes as UTF-8 with replacement for invalid sequences.
pub fn parse_webvtt_bytes(bytes: &[u8], base_timestamp_ms: i64) -> Result<WebVttSubtitle> {
    parse_webvtt(&String::from_utf8_lossy(bytes), base_timestamp_ms)
}

/// Formats a subtitle document using the requested output format.
pub fn format_subtitle(subtitle: &WebVttSubtitle, format: SubtitleFormat) -> String {
    match format {
        SubtitleFormat::Srt => subtitle.to_srt(),
        SubtitleFormat::Vtt => subtitle.to_vtt(),
    }
}

impl WebVttSubtitle {
    /// Adds cues from another subtitle, applying MPEG-TS offset repair and split-cue merging.
    pub fn add_cues_from_one(&mut self, mut other: WebVttSubtitle) {
        self.fix_timestamp(&mut other);
        for cue in other.cues {
            if self.cues.contains(&cue) {
                continue;
            }
            if let Some(last) = self.cues.last_mut()
                && cue.start_ms - last.end_ms <= 1
                && cue.payload == last.payload
            {
                last.end_ms = cue.end_ms;
                continue;
            }
            self.cues.push(cue);
        }
    }

    /// Shifts all cues left by the requested amount, saturating at zero.
    pub fn left_shift_ms(&mut self, amount_ms: i64) {
        for cue in &mut self.cues {
            cue.start_ms = (cue.start_ms - amount_ms).max(0);
            cue.end_ms = (cue.end_ms - amount_ms).max(0);
        }
    }

    /// Converts cues to WebVTT text.
    pub fn to_vtt(&self) -> String {
        format!("WEBVTT{NEWLINE}{NEWLINE}{}", self.to_vtt_body())
    }

    /// Converts cues to SRT text.
    pub fn to_srt(&self) -> String {
        let mut output = String::new();
        for (index, cue) in (1..).zip(self.non_empty_cues()) {
            output.push_str(&format!("{index}{NEWLINE}"));
            output.push_str(&format!(
                "{} --> {}{NEWLINE}",
                format_srt_time(cue.start_ms),
                format_srt_time(cue.end_ms)
            ));
            output.push_str(&cue.payload);
            output.push_str(NEWLINE);
            output.push_str(NEWLINE);
        }
        output.push_str(NEWLINE);
        if output.trim().is_empty() {
            "1\r\n00:00:00,000 --> 00:00:01,000".to_string()
        } else {
            output
        }
    }

    fn to_vtt_body(&self) -> String {
        let mut output = String::new();
        for cue in self.non_empty_cues() {
            output.push_str(&format!(
                "{} --> {} ",
                format_vtt_time(cue.start_ms),
                format_vtt_time(cue.end_ms)
            ));
            if !cue.settings.is_empty() {
                output.push_str(&cue.settings);
            }
            output.push_str(NEWLINE);
            output.push_str(&cue.payload);
            output.push_str(NEWLINE);
            output.push_str(NEWLINE);
        }
        output.push_str(NEWLINE);
        output
    }

    fn non_empty_cues(&self) -> impl Iterator<Item = &SubtitleCue> {
        self.cues.iter().filter(|cue| !cue.payload.is_empty())
    }

    fn apply_base_timestamp(&mut self, base_timestamp_ms: i64) {
        for cue in &mut self.cues {
            if cue.start_ms - base_timestamp_ms >= 0 {
                cue.start_ms -= base_timestamp_ms;
                cue.end_ms -= base_timestamp_ms;
            } else {
                break;
            }
        }
    }

    fn fix_timestamp(&mut self, other: &mut WebVttSubtitle) {
        if other.mpegts_timestamp == 0 {
            return;
        }
        let should_fix =
            if let (Some(first_new), Some(last_current)) = (other.cues.first(), self.cues.last()) {
                first_new.start_ms < last_current.end_ms && first_new.end_ms != last_current.end_ms
            } else {
                self.cues.is_empty()
            };
        if !should_fix {
            return;
        }
        let offset_ms = ((other.mpegts_timestamp - self.mpegts_timestamp) / 90_000) * 1000;
        if other
            .cues
            .first()
            .is_some_and(|cue| cue.start_ms < offset_ms)
        {
            for cue in &mut other.cues {
                cue.start_ms += offset_ms;
                cue.end_ms += offset_ms;
            }
        }
    }
}

/// Returns the timescale if an MP4 initialization segment contains WVTT.
pub fn check_wvtt_init(data: &[u8]) -> Result<Option<u32>> {
    if !data.windows(4).any(|window| window == b"wvtt") {
        return Ok(None);
    }
    Ok(find_first_full_box(data, b"mdhd")
        .as_deref()
        .and_then(parse_mdhd_timescale))
}

/// Returns true when an MP4 initialization segment contains STPP.
pub fn check_stpp_init(data: &[u8]) -> bool {
    data.windows(4).any(|window| window == b"stpp")
}

/// Extracts WVTT cues from MP4 media segment bytes.
pub fn extract_wvtt_from_segments(segments: &[Vec<u8>], timescale: u32) -> Result<WebVttSubtitle> {
    Ok(extract_wvtt_from_segments_with_console_lines(segments, timescale)?.subtitle)
}

pub(crate) fn extract_wvtt_from_segments_with_console_lines(
    segments: &[Vec<u8>],
    timescale: u32,
) -> Result<WvttExtraction> {
    if timescale == 0 {
        return Err(Error::subtitle("missing WVTT timescale"));
    }
    let mut cues: Vec<SubtitleCue> = Vec::new();
    let mut console_lines = Vec::new();
    for segment in segments {
        let mut fragment = WvttFragment::default();
        collect_wvtt_fragment(segment, &mut fragment)?;
        if !fragment.saw_mdat && !fragment.saw_tfdt && !fragment.saw_trun {
            return Err(Error::subtitle("a required WVTT box is missing"));
        }
        let Some(raw_payload) = fragment.mdat else {
            return Err(Error::subtitle("WVTT media data box is missing"));
        };
        if fragment.presentations.is_empty() {
            continue;
        }
        let extracted = parse_wvtt_mdat(
            &raw_payload,
            fragment.base_time,
            fragment.default_duration,
            &fragment.presentations,
            timescale,
            &mut console_lines,
        )?;
        for cue in extracted {
            if let Some(last) = cues.last_mut()
                && last.end_ms == cue.start_ms
                && last.settings == cue.settings
                && last.payload == cue.payload
            {
                last.end_ms = cue.end_ms;
                continue;
            }
            cues.push(cue);
        }
    }
    Ok(WvttExtraction {
        subtitle: WebVttSubtitle {
            cues,
            mpegts_timestamp: 0,
        },
        console_lines,
    })
}

/// Extracts WVTT cues from MP4 media segment files.
pub async fn extract_wvtt_from_files(paths: &[PathBuf], timescale: u32) -> Result<WebVttSubtitle> {
    Ok(extract_wvtt_from_files_with_console_lines(paths, timescale)
        .await?
        .subtitle)
}

pub(crate) async fn extract_wvtt_from_files_with_console_lines(
    paths: &[PathBuf],
    timescale: u32,
) -> Result<WvttExtraction> {
    let mut segments = Vec::with_capacity(paths.len());
    for path in paths {
        segments.push(tokio::fs::read(path).await?);
    }
    extract_wvtt_from_segments_with_console_lines(&segments, timescale)
}

/// Extracts TTML documents from plain TTML file paths.
pub async fn extract_ttml_from_files(
    paths: &[PathBuf],
    segment_time_ms: i64,
    base_timestamp_ms: i64,
) -> Result<WebVttSubtitle> {
    let mut documents = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let text = tokio::fs::read_to_string(path).await?;
        documents.push(shift_ttml_document(&text, segment_time_ms, index)?);
    }
    extract_ttml_documents(&documents, base_timestamp_ms)
}

/// Extracts STPP/TTML documents from MP4 media segment bytes.
pub fn extract_stpp_from_segments(
    segments: &[Vec<u8>],
    segment_time_ms: i64,
    base_timestamp_ms: i64,
) -> Result<WebVttSubtitle> {
    let mut documents = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let mdats = match collect_named_box_payloads(segment, b"mdat") {
            Ok(mdats) => mdats,
            Err(error) if error.to_string().contains("invalid MP4 box size") => continue,
            Err(error) => return Err(error),
        };
        for payload in mdats {
            let text = String::from_utf8_lossy(&payload);
            for doc in split_ttml_documents(&text)? {
                documents.push(shift_ttml_document(&doc, segment_time_ms, index)?);
            }
        }
    }
    extract_ttml_documents(&documents, base_timestamp_ms)
}

/// Extracts STPP/TTML documents from MP4 media segment files.
pub async fn extract_stpp_from_files(
    paths: &[PathBuf],
    segment_time_ms: i64,
    base_timestamp_ms: i64,
) -> Result<WebVttSubtitle> {
    let mut segments = Vec::with_capacity(paths.len());
    for path in paths {
        segments.push(tokio::fs::read(path).await?);
    }
    extract_stpp_from_segments(&segments, segment_time_ms, base_timestamp_ms)
}

/// Extracts WebVTT-compatible cues from TTML documents.
pub fn extract_ttml_documents(
    documents: &[String],
    base_timestamp_ms: i64,
) -> Result<WebVttSubtitle> {
    let mut final_subs: Vec<TtmlSub> = Vec::new();
    for document in documents {
        if !document.contains("<tt") {
            continue;
        }
        let document_text = repaired_ttml_document(document);
        let parsed = roxmltree::Document::parse(document_text.as_ref())
            .map_err(|error| Error::subtitle(format!("invalid TTML XML: {error}")))?;
        let image_map = collect_ttml_images(&parsed);
        let paragraph_nodes = collect_ttml_paragraphs(&parsed);
        for node in paragraph_nodes {
            let begin = attr_local(node, "begin").unwrap_or_default();
            let end = attr_local(node, "end").unwrap_or_default();
            let region = attr_local(node, "region").unwrap_or_default();
            let contents = ttml_contents(node, &image_map);
            if contents.is_empty() {
                continue;
            }
            if begin.trim().is_empty() || end.trim().is_empty() {
                continue;
            }
            let sub = TtmlSub {
                begin,
                end,
                region,
                contents,
            };
            if let Some(last) = final_subs.last_mut()
                && last.end == sub.begin
                && last.region == sub.region
                && last.contents == sub.contents
            {
                last.end = sub.end;
                continue;
            }
            if !final_subs.contains(&sub) {
                final_subs.push(sub);
            }
        }
    }

    let mut grouped: Vec<((String, String), Vec<String>)> = Vec::new();
    for sub in final_subs {
        let key = (sub.begin, sub.end);
        if let Some((_, contents)) = grouped
            .iter_mut()
            .find(|((begin, end), _)| begin == &key.0 && end == &key.1)
        {
            contents.extend(sub.contents);
        } else {
            grouped.push((key, sub.contents));
        }
    }

    let mut cues = Vec::new();
    for ((begin, end), payloads) in grouped {
        cues.push(SubtitleCue {
            start_ms: parse_timestamp_ms(&begin)?,
            end_ms: parse_timestamp_ms(&end)?,
            payload: payloads.join(NEWLINE),
            settings: String::new(),
        });
    }
    let mut subtitle = WebVttSubtitle {
        cues,
        mpegts_timestamp: 0,
    };
    if base_timestamp_ms != 0 {
        subtitle.apply_base_timestamp(base_timestamp_ms);
    }
    Ok(subtitle)
}

/// Writes image subtitle payloads to numbered PNG files and rewrites cue payloads to file names.
pub async fn write_image_pngs(
    subtitle: &mut WebVttSubtitle,
    directory: &Path,
) -> Result<Vec<SubtitleImage>> {
    tokio::fs::create_dir_all(directory).await?;
    let mut images = Vec::new();
    let mut next_index = 0_usize;
    for (cue_index, cue) in subtitle.cues.iter_mut().enumerate() {
        let Some(encoded) = cue.payload.strip_prefix("Base64::") else {
            continue;
        };
        let mut path;
        loop {
            path = directory.join(format!("{next_index}.png"));
            next_index += 1;
            if !tokio::fs::try_exists(&path).await? {
                break;
            }
        }
        let bytes = base64_decode(encoded.trim())?;
        tokio::fs::write(&path, bytes).await?;
        cue.payload = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_string();
        images.push(SubtitleImage { path, cue_index });
    }
    Ok(images)
}

fn parse_vtt_cue(time_line: &str, payload: &str) -> Result<SubtitleCue> {
    let split = Regex::new(r"\s+").map_err(|error| Error::subtitle(error.to_string()))?;
    let normalized = time_line.replace("-->", " ");
    let parts = split
        .split(&normalized)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return Err(Error::subtitle("invalid WebVTT cue timeline"));
    }
    Ok(SubtitleCue {
        start_ms: parse_timestamp_ms(parts[0])?,
        end_ms: parse_timestamp_ms(parts[1])?,
        payload: remove_vtt_class_tags(&remove_zero_width_spaces(payload))?,
        settings: if parts.len() > 2 {
            parts[2..].join(" ")
        } else {
            String::new()
        },
    })
}

fn remove_zero_width_spaces(text: &str) -> String {
    text.chars().filter(|ch| (*ch as u32) != 8203).collect()
}

fn remove_vtt_class_tags(text: &str) -> Result<String> {
    let regex =
        Regex::new(r"(?s)<c\..*?>(.*?)</c>").map_err(|error| Error::subtitle(error.to_string()))?;
    if !regex.is_match(text) {
        return Ok(text.to_string());
    }
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let mut output = String::new();
        for capture in regex.captures_iter(line.trim_end()) {
            if let Some(value) = capture.get(1) {
                output.push_str(value.as_str());
                output.push(' ');
            }
        }
        lines.push(output.trim_end().to_string());
    }
    Ok(lines.join(NEWLINE).trim_end().to_string())
}

fn parse_timestamp_ms(value: &str) -> Result<i64> {
    let value = value.trim().replace(',', ".");
    if let Some(seconds) = value.strip_suffix('s') {
        let seconds = seconds
            .parse::<f64>()
            .map_err(|_| Error::subtitle("invalid subtitle seconds timestamp"))?;
        return Ok((seconds * 1000.0).round() as i64);
    }
    let (time_part, millis) = match value.split_once('.') {
        Some((left, right)) => {
            let mut fraction = right.to_string();
            while fraction.len() < 3 {
                fraction.push('0');
            }
            let millis = fraction
                .parse::<i64>()
                .map_err(|_| Error::subtitle("invalid subtitle millisecond timestamp"))?;
            (left, millis)
        }
        None => (value.as_str(), 0),
    };
    let mut total = millis;
    for (power, part) in time_part.split(':').rev().enumerate() {
        let component = part
            .parse::<i64>()
            .map_err(|_| Error::subtitle("invalid subtitle timestamp"))?;
        total += component * 60_i64.pow(power as u32) * 1000;
    }
    Ok(total)
}

fn format_vtt_time(milliseconds: i64) -> String {
    let (hours, minutes, seconds, millis) = time_parts(milliseconds);
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

fn format_srt_time(milliseconds: i64) -> String {
    let (hours, minutes, seconds, millis) = time_parts(milliseconds);
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

fn time_parts(milliseconds: i64) -> (i64, i64, i64, i64) {
    let clamped = milliseconds.max(0);
    let hours = clamped / 3_600_000;
    let minutes = (clamped / 60_000) % 60;
    let seconds = (clamped / 1000) % 60;
    let millis = clamped % 1000;
    (hours, minutes, seconds, millis)
}

#[derive(Clone, Debug, Default)]
struct WvttFragment {
    base_time: u64,
    default_duration: Option<u32>,
    presentations: Vec<WvttSample>,
    mdat: Option<Vec<u8>>,
    saw_tfdt: bool,
    saw_trun: bool,
    saw_mdat: bool,
}

#[derive(Clone, Debug, Default)]
struct WvttSample {
    duration: Option<u32>,
    size: Option<u32>,
    composition_offset: Option<i64>,
}

fn collect_wvtt_fragment(data: &[u8], fragment: &mut WvttFragment) -> Result<()> {
    for_each_box(data, &mut |name, payload| {
        match &name {
            b"tfdt" => {
                fragment.saw_tfdt = true;
                fragment.base_time = parse_tfdt(payload)?;
            }
            b"tfhd" => fragment.default_duration = parse_tfhd_default_duration(payload)?,
            b"trun" => {
                fragment.saw_trun = true;
                fragment.presentations = parse_trun(payload)?;
            }
            b"mdat" => {
                if fragment.saw_mdat {
                    return Err(Error::subtitle(
                        "multiple WVTT media data boxes are unsupported",
                    ));
                }
                fragment.saw_mdat = true;
                fragment.mdat = Some(payload.to_vec());
            }
            b"moof" | b"traf" => collect_wvtt_fragment(payload, fragment)?,
            _ => {}
        }
        Ok(())
    })
}

fn parse_wvtt_mdat(
    raw_payload: &[u8],
    base_time: u64,
    default_duration: Option<u32>,
    presentations: &[WvttSample],
    timescale: u32,
    console_lines: &mut Vec<String>,
) -> Result<Vec<SubtitleCue>> {
    let mut cues = Vec::new();
    let mut current_time = base_time;
    let mut offset = 0_usize;
    for presentation in presentations {
        let duration = presentation
            .duration
            .or(default_duration)
            .ok_or_else(|| Error::subtitle("WVTT sample duration is missing"))?;
        if duration == 0 {
            return Err(Error::subtitle("WVTT sample duration is zero"));
        }
        let start_time = if let Some(composition_offset) = presentation.composition_offset {
            add_signed_u64(base_time, composition_offset)?
        } else {
            current_time
        };
        current_time = start_time.saturating_add(u64::from(duration));
        let mut total_size = 0_u32;
        loop {
            let payload_size = read_u32(raw_payload, offset)?;
            let payload_size_usize = payload_size as usize;
            if payload_size_usize < 8 || offset + payload_size_usize > raw_payload.len() {
                return Err(Error::subtitle("invalid WVTT payload size"));
            }
            let name = read_name(raw_payload, offset + 4)?;
            let payload = &raw_payload[offset + 8..offset + payload_size_usize];
            total_size = total_size.saturating_add(payload_size);
            match &name {
                b"vttc" => {
                    if let Some(cue) = parse_vttc_payload(
                        payload,
                        units_to_ms(start_time, timescale),
                        units_to_ms(current_time, timescale),
                    )? {
                        cues.push(cue);
                    }
                }
                b"vtte" => {}
                _ => console_lines.push(format!("Unknown box {}! Skipping!", mp4_box_name(&name))),
            }
            offset += payload_size_usize;
            if let Some(sample_size) = presentation.size {
                if total_size > sample_size {
                    return Err(Error::subtitle("WVTT samples exceed declared sample size"));
                }
                if sample_size != 0 && total_size < sample_size {
                    continue;
                }
            }
            break;
        }
    }
    Ok(cues)
}

fn parse_vttc_payload(data: &[u8], start_ms: i64, end_ms: i64) -> Result<Option<SubtitleCue>> {
    let mut payload = String::new();
    let mut settings = String::new();
    for_each_box(data, &mut |name, box_payload| {
        match &name {
            b"payl" => {
                payload = String::from_utf8(box_payload.to_vec())
                    .map_err(|error| Error::subtitle(format!("invalid WVTT payload: {error}")))?;
            }
            b"sttg" => {
                settings = String::from_utf8(box_payload.to_vec())
                    .map_err(|error| Error::subtitle(format!("invalid WVTT settings: {error}")))?;
            }
            _ => {}
        }
        Ok(())
    })?;
    if payload.is_empty() {
        return Ok(None);
    }
    Ok(Some(SubtitleCue {
        start_ms,
        end_ms,
        payload,
        settings,
    }))
}

fn units_to_ms(value: u64, timescale: u32) -> i64 {
    ((value as f64 / f64::from(timescale)) * 1000.0).round() as i64
}

fn parse_mdhd_timescale(payload: &[u8]) -> Option<u32> {
    let version = *payload.first()?;
    let offset = if version == 1 { 20 } else { 12 };
    read_u32_opt(payload, offset)
}

fn parse_tfdt(payload: &[u8]) -> Result<u64> {
    let version = *payload
        .first()
        .ok_or_else(|| Error::subtitle("invalid TFDT box"))?;
    if version == 1 {
        read_u64(payload, 4)
    } else {
        read_u32(payload, 4).map(u64::from)
    }
}

fn parse_tfhd_default_duration(payload: &[u8]) -> Result<Option<u32>> {
    if payload.len() < 8 {
        return Err(Error::subtitle("invalid TFHD box"));
    }
    let flags = read_full_box_flags(payload)?;
    let mut offset = 8_usize;
    if flags & 0x000001 != 0 {
        offset += 8;
    }
    if flags & 0x000002 != 0 {
        offset += 4;
    }
    if flags & 0x000008 != 0 {
        return read_u32(payload, offset).map(Some);
    }
    Ok(None)
}

fn parse_trun(payload: &[u8]) -> Result<Vec<WvttSample>> {
    if payload.len() < 8 {
        return Err(Error::subtitle("invalid TRUN box"));
    }
    let version = payload[0];
    let flags = read_full_box_flags(payload)?;
    let sample_count = read_u32(payload, 4)? as usize;
    let mut offset = 8_usize;
    if flags & 0x000001 != 0 {
        offset += 4;
    }
    if flags & 0x000004 != 0 {
        offset += 4;
    }
    let mut samples = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let mut sample = WvttSample::default();
        if flags & 0x000100 != 0 {
            sample.duration = Some(read_u32(payload, offset)?);
            offset += 4;
        }
        if flags & 0x000200 != 0 {
            sample.size = Some(read_u32(payload, offset)?);
            offset += 4;
        }
        if flags & 0x000400 != 0 {
            offset += 4;
        }
        if flags & 0x000800 != 0 {
            sample.composition_offset = if version == 0 {
                Some(i64::from(read_u32(payload, offset)?))
            } else {
                Some(i64::from(read_i32(payload, offset)?))
            };
            offset += 4;
        }
        samples.push(sample);
    }
    Ok(samples)
}

fn read_full_box_flags(payload: &[u8]) -> Result<u32> {
    if payload.len() < 4 {
        return Err(Error::subtitle("invalid full box header"));
    }
    Ok((u32::from(payload[1]) << 16) | (u32::from(payload[2]) << 8) | u32::from(payload[3]))
}

fn collect_named_box_payloads(data: &[u8], target: &[u8; 4]) -> Result<Vec<Vec<u8>>> {
    let mut payloads = Vec::new();
    for_each_box(data, &mut |name, payload| {
        if &name == target {
            payloads.push(payload.to_vec());
        } else if matches!(
            &name,
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"moof" | b"traf"
        ) {
            payloads.extend(collect_named_box_payloads(payload, target)?);
        }
        Ok(())
    })?;
    Ok(payloads)
}

fn find_first_full_box(data: &[u8], target: &[u8; 4]) -> Option<Vec<u8>> {
    let mut offset = 4_usize;
    while offset + 4 <= data.len() {
        if data.get(offset..offset + 4) == Some(target.as_slice()) {
            let size_offset = offset - 4;
            if let Some(size) = read_u32_opt(data, size_offset).map(|value| value as usize) {
                let end = size_offset.saturating_add(size);
                if size >= 8 && end <= data.len() {
                    return data.get(offset + 4..end).map(<[u8]>::to_vec);
                }
            }
        }
        offset += 1;
    }
    None
}

fn for_each_box<F>(data: &[u8], callback: &mut F) -> Result<()>
where
    F: FnMut([u8; 4], &[u8]) -> Result<()>,
{
    let mut offset = 0_usize;
    while offset + 8 <= data.len() {
        let mut size = read_u32(data, offset)? as usize;
        let name = read_name(data, offset + 4)?;
        let mut header_size = 8_usize;
        if size == 1 {
            let large = read_u64(data, offset + 8)?;
            size = usize::try_from(large)
                .map_err(|_| Error::subtitle("MP4 box is too large for this platform"))?;
            header_size = 16;
        } else if size == 0 {
            size = data.len() - offset;
        }
        if size < header_size || offset + size > data.len() {
            return Err(Error::subtitle("invalid MP4 box size"));
        }
        callback(name, &data[offset + header_size..offset + size])?;
        offset += size;
    }
    Ok(())
}

fn read_name(data: &[u8], offset: usize) -> Result<[u8; 4]> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| Error::subtitle("unexpected end of MP4 data"))?;
    Ok([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn mp4_box_name(name: &[u8; 4]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

fn read_u32(data: &[u8], offset: usize) -> Result<u32> {
    read_u32_opt(data, offset).ok_or_else(|| Error::subtitle("unexpected end of MP4 data"))
}

fn read_u32_opt(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_i32(data: &[u8], offset: usize) -> Result<i32> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or_else(|| Error::subtitle("unexpected end of MP4 data"))?;
    Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(data: &[u8], offset: usize) -> Result<u64> {
    let bytes = data
        .get(offset..offset + 8)
        .ok_or_else(|| Error::subtitle("unexpected end of MP4 data"))?;
    Ok(u64::from_be_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn add_signed_u64(value: u64, offset: i64) -> Result<u64> {
    if offset >= 0 {
        value
            .checked_add(offset as u64)
            .ok_or_else(|| Error::subtitle("WVTT composition timestamp overflow"))
    } else {
        value
            .checked_sub(offset.unsigned_abs())
            .ok_or_else(|| Error::subtitle("WVTT composition timestamp underflow"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TtmlSub {
    begin: String,
    end: String,
    region: String,
    contents: Vec<String>,
}

fn split_ttml_documents(text: &str) -> Result<Vec<String>> {
    let regex =
        Regex::new(r"(?s)<tt[\s\S]*?</tt>").map_err(|error| Error::subtitle(error.to_string()))?;
    Ok(regex
        .find_iter(text)
        .map(|matched| matched.as_str().to_string())
        .collect())
}

fn shift_ttml_document(text: &str, segment_time_ms: i64, index: usize) -> Result<String> {
    if segment_time_ms == 0 {
        return Ok(text.to_string());
    }
    let offset = segment_time_ms.saturating_mul(index as i64);
    let regex = Regex::new(r#"(begin|end)="(\d{2}:\d{2}:\d{2}\.\d{3})""#)
        .map_err(|error| Error::subtitle(error.to_string()))?;
    let mut output = String::with_capacity(text.len());
    let mut last = 0_usize;
    for capture in regex.captures_iter(text) {
        let Some(full) = capture.get(0) else {
            continue;
        };
        let Some(name) = capture.get(1) else {
            continue;
        };
        let Some(value) = capture.get(2) else {
            continue;
        };
        output.push_str(&text[last..full.start()]);
        output.push_str(name.as_str());
        output.push_str("=\"");
        output.push_str(&format_vtt_time(
            parse_timestamp_ms(value.as_str())? + offset,
        ));
        output.push('"');
        last = full.end();
    }
    output.push_str(&text[last..]);
    Ok(output)
}

fn collect_ttml_images(document: &roxmltree::Document<'_>) -> BTreeMap<String, String> {
    let mut images = BTreeMap::new();
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "image")
    {
        if let Some(id) = attr_local(node, "id") {
            images.insert(id, node.text().unwrap_or_default().trim().to_string());
        }
    }
    images
}

fn collect_ttml_paragraphs<'a>(
    document: &'a roxmltree::Document<'a>,
) -> Vec<roxmltree::Node<'a, 'a>> {
    let mut nodes = document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "p")
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        nodes = document
            .descendants()
            .filter(|node| node.is_element() && node.tag_name().name() == "div")
            .collect();
    }
    nodes
}

fn ttml_contents(node: roxmltree::Node<'_, '_>, images: &BTreeMap<String, String>) -> Vec<String> {
    if let Some(background) = attr_local(node, "backgroundImage") {
        let key = background.trim_start_matches('#');
        if let Some(value) = images.get(key) {
            return vec![format!("Base64::{value}")];
        }
    }

    let mut contents = Vec::new();
    for child in node.children() {
        if child.is_text() {
            let text = child.text().unwrap_or_default().trim();
            if !text.is_empty() {
                contents.push(text.to_string());
            }
        } else if child.is_element() {
            let text = text_from_ttml_element(child);
            if text.is_empty() {
                continue;
            }
            if matches!(
                attr_local(child, "fontStyle").as_deref(),
                Some("italic" | "oblique")
            ) {
                contents.push(format!("<i>{text}</i>"));
            } else {
                contents.push(text);
            }
        }
    }
    contents
}

fn text_from_ttml_element(node: roxmltree::Node<'_, '_>) -> String {
    let mut output = String::new();
    for child in node.children() {
        if child.is_text() {
            output.push_str(child.text().unwrap_or_default().trim());
        } else if child.is_element() && child.tag_name().name() == "br" {
            output.push_str(NEWLINE);
        } else if child.is_element() {
            output.push_str(&text_from_ttml_element(child));
        }
    }
    output
}

fn attr_local(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    node.attributes()
        .find(|attribute| {
            attribute.name() == name
                || attribute
                    .name()
                    .rsplit(':')
                    .next()
                    .is_some_and(|local| local == name)
        })
        .map(|attribute| attribute.value().to_string())
}

fn repaired_ttml_document(document: &str) -> Cow<'_, str> {
    if roxmltree::Document::parse(document).is_ok() {
        return Cow::Borrowed(document);
    }
    let Ok(paragraph) = Regex::new(r"(?s)<p\b[^>]*>(.*?)</p>") else {
        return Cow::Borrowed(document);
    };
    let Ok(attr_like) = Regex::new(r#" \w+:\w+="[^"]*""#) else {
        return Cow::Borrowed(document);
    };
    let mut output = String::with_capacity(document.len());
    let mut last = 0_usize;
    let mut changed = false;
    for captures in paragraph.captures_iter(document) {
        let Some(full) = captures.get(0) else {
            continue;
        };
        let Some(inner) = captures.get(1) else {
            continue;
        };
        output.push_str(&document[last..inner.start()]);
        let inner_text = inner.as_str();
        if attr_like.is_match(inner_text)
            && roxmltree::Document::parse(&format!("<p>{inner_text}</p>")).is_err()
        {
            output.push_str(&escape_xml_text(inner_text));
            changed = true;
        } else {
            output.push_str(inner_text);
        }
        last = inner.end();
        if full.end() < last {
            last = full.end();
        }
    }
    if !changed {
        return Cow::Borrowed(document);
    }
    output.push_str(&document[last..]);
    Cow::Owned(output)
}

fn escape_xml_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(ch),
        }
    }
    output
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    crate::base64::decode_base64(input).map_err(Error::subtitle)
}
