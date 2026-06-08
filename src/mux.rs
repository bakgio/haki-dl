//! Container, merge, mux, and output artifact models.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::MuxerKind;
use crate::datetime::current_local_iso_timestamp;
use crate::error::{Error, Result};
use crate::manifest::{EncryptionMethod, MediaType, Stream};

/// Final mux output format.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MuxFormat {
    /// MP4 output.
    #[default]
    Mp4,
    /// Matroska output.
    Mkv,
    /// MPEG-TS output.
    Ts,
}

/// Per-stream ffmpeg merge output format.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MergeOutputFormat {
    /// MP4 output.
    #[default]
    Mp4,
    /// Matroska output.
    Mkv,
    /// Flash Video output.
    Flv,
    /// MPEG-4 audio output.
    M4a,
    /// MPEG-TS output.
    Ts,
    /// E-AC-3 elementary output.
    Eac3,
    /// AAC audio output.
    Aac,
    /// AC-3 elementary output.
    Ac3,
}

/// Planned external command without executing a process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxCommandPlan {
    /// Program path.
    pub program: PathBuf,
    /// Command arguments.
    pub arguments: String,
    /// Working directory.
    pub working_directory: PathBuf,
    /// Expected output path.
    pub output_path: PathBuf,
}

/// Metadata used when planning a per-stream ffmpeg merge.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FfmpegMergeMetadata {
    pub poster: Option<PathBuf>,
    /// Optional extra DD+ audio input prepended before the original audio track.
    pub ddp_audio: Option<PathBuf>,
    pub audio_name: String,
    pub title: String,
    pub copyright: String,
    pub comment: String,
    pub encoding_tool: String,
    pub recording_time: Option<String>,
    pub date_string: Option<String>,
}

/// Input for planning a per-stream ffmpeg merge command.
#[derive(Clone, Copy, Debug)]
pub struct FfmpegMergeRequest<'a> {
    /// Program path.
    pub binary: &'a Path,
    /// Ordered input segment files.
    pub files: &'a [PathBuf],
    /// Output path without an extension.
    pub output_base_path: &'a Path,
    /// Requested output format.
    pub format: MergeOutputFormat,
    /// Apply AAC ADTS-to-ASC bitstream filter.
    pub use_aac_filter: bool,
    /// Apply MP4 faststart flag.
    pub fast_start: bool,
    /// Write date metadata.
    pub write_date: bool,
    /// Use ffmpeg concat demuxer instead of concat protocol.
    pub use_concat_demuxer: bool,
    /// Concat demuxer list file path when concat demuxer mode is enabled.
    pub concat_list_path: Option<&'a Path>,
    /// Metadata fields.
    pub metadata: &'a FfmpegMergeMetadata,
}

/// mp4forge validation result for a single stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mp4forgeSupport {
    /// Metadata is supported.
    Supported,
    /// Metadata is insufficient and a bounded probe is required before download.
    RequiresProbe { reason: String },
    /// Metadata identifies an unsupported case.
    Unsupported { reason: String },
}

/// Versioned support matrix for explicit mp4forge backend requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mp4forgeSupportMatrix {
    pub version: String,
    pub video_codecs: Vec<String>,
    pub audio_codecs: Vec<String>,
}

impl Default for Mp4forgeSupportMatrix {
    fn default() -> Self {
        Self {
            version: "haki-dl-mp4forge-matrix-v1".to_string(),
            video_codecs: vec![
                "avc1".to_string(),
                "h264".to_string(),
                "hvc1".to_string(),
                "hev1".to_string(),
                "hevc".to_string(),
                "dvhe".to_string(),
                "dvh1".to_string(),
            ],
            audio_codecs: vec![
                "mp4a".to_string(),
                "aac".to_string(),
                "ec-3".to_string(),
                "ec3".to_string(),
                "ac-3".to_string(),
                "ac3".to_string(),
            ],
        }
    }
}

/// File created by a download, merge, decrypt, subtitle, or mux operation.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputArtifact {
    /// Final artifact path.
    pub path: PathBuf,
    /// Media type associated with the artifact when known.
    pub media_type: Option<crate::manifest::MediaType>,
    /// Language associated with the artifact when known.
    pub language: Option<String>,
    /// Human-readable stream description when known.
    pub description: Option<String>,
}

impl OutputArtifact {
    /// Creates a new output artifact model.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            media_type: None,
            language: None,
            description: None,
        }
    }
}

/// External file imported into a final mux operation.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxImport {
    /// Input file path.
    pub path: PathBuf,
    /// Optional language code.
    pub language: Option<String>,
    /// Optional track name.
    pub name: Option<String>,
    /// Track ordering index used by the compatibility layer.
    pub index: i32,
}

impl MuxImport {
    /// Creates an import with compatibility ordering.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            language: None,
            name: None,
            index: 999,
        }
    }
}

/// Final mux options matching the compatibility workflow model.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MuxOptions {
    /// Use mkvmerge instead of the default ffmpeg path.
    pub use_mkvmerge: bool,
    /// Requested mux format.
    pub format: MuxFormat,
    /// Keep intermediate files after mux succeeds.
    pub keep_files: bool,
    /// Exclude subtitle artifacts from mux input.
    pub skip_subtitle: bool,
    /// Optional process binary path.
    pub bin_path: Option<PathBuf>,
}

impl Default for MuxOptions {
    fn default() -> Self {
        Self {
            use_mkvmerge: false,
            format: MuxFormat::Mp4,
            keep_files: false,
            skip_subtitle: false,
            bin_path: None,
        }
    }
}

/// Media information captured for an output file.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MediaInfo {
    pub id: Option<String>,
    pub text: Option<String>,
    pub base_info: Option<String>,
    pub bitrate: Option<String>,
    pub resolution: Option<String>,
    pub fps: Option<String>,
    pub media_type: Option<String>,
    pub start_time_millis: Option<i64>,
    pub dolby_vision: bool,
    pub hdr: bool,
}

/// Output file model used by merge, decrypt, subtitle, and mux work.
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputFile {
    /// Media type associated with the file when known.
    pub media_type: Option<crate::manifest::MediaType>,
    /// Compatibility ordering index.
    pub index: i32,
    /// File path.
    pub file_path: PathBuf,
    /// Language code.
    pub lang_code: Option<String>,
    /// Track description.
    pub description: Option<String>,
    /// Optional media-info records.
    pub media_infos: Vec<MediaInfo>,
}

impl OutputFile {
    /// Creates a new output file with the required index and path.
    pub fn new(index: i32, file_path: impl Into<PathBuf>) -> Self {
        Self {
            media_type: None,
            index,
            file_path: file_path.into(),
            lang_code: None,
            description: None,
            media_infos: Vec::new(),
        }
    }
}

/// Returns the final mux extension for a mux-after-done format.
pub fn mux_extension(format: MuxFormat) -> &'static str {
    match format {
        MuxFormat::Mp4 => ".mp4",
        MuxFormat::Mkv => ".mkv",
        MuxFormat::Ts => ".ts",
    }
}

/// Returns the per-stream merge extension for an ffmpeg merge format.
pub fn merge_extension(format: MergeOutputFormat) -> &'static str {
    match format {
        MergeOutputFormat::Mp4 => ".mp4",
        MergeOutputFormat::Mkv => ".mkv",
        MergeOutputFormat::Flv => ".flv",
        MergeOutputFormat::M4a | MergeOutputFormat::Aac => ".m4a",
        MergeOutputFormat::Ts => ".ts",
        MergeOutputFormat::Eac3 => ".eac3",
        MergeOutputFormat::Ac3 => ".ac3",
    }
}

/// Combines files by byte concatenation, copying a single input directly.
pub async fn combine_files(files: &[PathBuf], output_path: &Path) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if files.len() == 1 {
        tokio::fs::copy(&files[0], output_path).await?;
        return Ok(());
    }
    let mut output = tokio::fs::File::create(output_path).await?;
    for path in files {
        if path.as_os_str().is_empty() {
            continue;
        }
        let mut input = tokio::fs::File::open(path).await?;
        tokio::io::copy(&mut input, &mut output).await?;
    }
    Ok(())
}

/// Partially combines large file lists into grouped temporary TS files and deletes grouped inputs.
pub async fn partial_combine_files(files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let div = if files.len() <= 90_000 { 100 } else { 200 };
    let parent = files[0]
        .parent()
        .ok_or_else(|| Error::mux("input file has no parent directory"))?;
    let mut new_files = Vec::new();
    for (index, chunk) in files.chunks(div).enumerate() {
        if chunk.is_empty() {
            continue;
        }
        let output = parent.join(format!("T{index:04}.ts"));
        combine_files(chunk, &output).await?;
        for path in chunk {
            if tokio::fs::try_exists(path).await? {
                tokio::fs::remove_file(path).await?;
            }
        }
        new_files.push(output);
    }
    Ok(new_files)
}

/// Builds the ffmpeg per-stream merge command line.
pub fn plan_ffmpeg_merge(request: FfmpegMergeRequest<'_>) -> Result<MuxCommandPlan> {
    let FfmpegMergeRequest {
        binary,
        files,
        output_base_path,
        format,
        use_aac_filter,
        fast_start,
        write_date,
        use_concat_demuxer,
        concat_list_path,
        metadata,
    } = request;
    let first = files
        .first()
        .ok_or_else(|| Error::mux("ffmpeg merge requires at least one input"))?;
    let working_directory = first
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| Error::mux("input file has no parent directory"))?;
    let mut command = String::from("-loglevel warning -nostdin ");
    if use_concat_demuxer {
        let concat_list_path = concat_list_path
            .ok_or_else(|| Error::mux("ffmpeg concat demuxer requires a concat list path"))?;
        command.push_str(&format!(
            " -f concat -safe 0 -i \"{}\"",
            concat_list_path.display()
        ));
    } else {
        command.push_str(" -i concat:\"");
        for path in files {
            if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
                command.push_str(name);
                command.push('|');
            }
        }
        command.push('"');
    }

    let output_base_path = absolute_output_base_path(output_base_path)?;
    let output_path = path_with_appended_extension(&output_base_path, merge_extension(format));
    let generated_date;
    let date_string = match metadata
        .recording_time
        .as_deref()
        .or(metadata.date_string.as_deref())
    {
        Some(value) => value,
        None => {
            generated_date = current_local_iso_timestamp();
            generated_date.as_str()
        }
    };
    let ddp_audio = metadata.ddp_audio.as_ref();
    let mut use_aac_filter = use_aac_filter;
    if ddp_audio.is_some() {
        use_aac_filter = false;
    }
    match format {
        MergeOutputFormat::Mp4 => {
            if let Some(poster) = &metadata.poster {
                command.push_str(&format!(" -i \"{}\"", poster.display()));
            }
            if let Some(ddp_audio) = ddp_audio {
                command.push_str(&format!(" -i \"{}\"", ddp_audio.display()));
            }
            command.push_str(" -map 0:v?");
            if ddp_audio.is_some() {
                let ddp_input_index = if metadata.poster.is_some() { 2 } else { 1 };
                command.push_str(&format!(" -map {ddp_input_index}:a -map 0:a?"));
            } else {
                command.push_str(" -map 0:a?");
            }
            command.push_str(" -map 0:s?");
            if metadata.poster.is_some() {
                command.push_str(" -map 1 -c:v:1 copy -disposition:v:1 attached_pic");
            }
            if write_date {
                command.push_str(&format!(" -metadata date=\"{date_string}\""));
            }
            let audio_metadata_index = if ddp_audio.is_some() { 1 } else { 0 };
            command.push_str(&format!(
                " -metadata encoding_tool=\"{}\" -metadata title=\"{}\" -metadata copyright=\"{}\" -metadata comment=\"{}\" -metadata:s:a:{audio_metadata_index} title=\"{}\" -metadata:s:a:{audio_metadata_index} handler=\"{}\"",
                metadata.encoding_tool,
                metadata.title,
                metadata.copyright,
                metadata.comment,
                metadata.audio_name,
                metadata.audio_name
            ));
            if ddp_audio.is_some() {
                command.push_str(" -metadata:s:a:0 title=\"DD+\" -metadata:s:a:0 handler=\"DD+\"");
            }
            if fast_start {
                command.push_str(" -movflags +faststart");
            }
            command.push_str(" -c copy -y");
            if use_aac_filter {
                command.push_str(" -bsf:a aac_adtstoasc");
            }
            command.push_str(&format!(" \"{}\"", output_path.display()));
        }
        MergeOutputFormat::Mkv => append_simple_ffmpeg_output(
            &mut command,
            " -map 0 -c copy -y",
            use_aac_filter,
            &output_path,
        ),
        MergeOutputFormat::Flv => append_simple_ffmpeg_output(
            &mut command,
            " -map 0 -c copy -y",
            use_aac_filter,
            &output_path,
        ),
        MergeOutputFormat::M4a => append_simple_ffmpeg_output(
            &mut command,
            " -map 0 -c copy -f mp4 -y",
            use_aac_filter,
            &output_path,
        ),
        MergeOutputFormat::Ts => {
            command.push_str(&format!(
                " -map 0 -c copy -y -f mpegts -bsf:v h264_mp4toannexb \"{}\"",
                output_path.display()
            ));
        }
        MergeOutputFormat::Eac3 => {
            command.push_str(&format!(
                " -map 0:a -c copy -y \"{}\"",
                output_path.display()
            ));
        }
        MergeOutputFormat::Aac => {
            command.push_str(&format!(
                " -map 0:a -c copy -y \"{}\"",
                output_path.display()
            ));
        }
        MergeOutputFormat::Ac3 => {
            command.push_str(&format!(
                " -map 0:a -c copy -y \"{}\"",
                output_path.display()
            ));
        }
    }

    Ok(MuxCommandPlan {
        program: binary.to_path_buf(),
        arguments: command,
        working_directory,
        output_path,
    })
}

/// Builds the ffmpeg mux-after-done command line.
pub fn plan_ffmpeg_mux(
    binary: impl Into<PathBuf>,
    files: &[OutputFile],
    output_base_path: &Path,
    format: MuxFormat,
    date_info: bool,
    date_string: Option<&str>,
) -> Result<MuxCommandPlan> {
    let mut command = String::from("-loglevel warning -nostdin -y -dn ");
    for file in files {
        command.push_str(&format!(" -i \"{}\" ", file.file_path.display()));
    }
    for index in 0..files.len() {
        command.push_str(&format!(" -map {index} "));
    }
    let has_srt = files
        .iter()
        .any(|file| has_extension(&file.file_path, "srt"));
    match format {
        MuxFormat::Mp4 => {
            command.push_str(" -strict unofficial -c:a copy -c:v copy -c:s mov_text ")
        }
        MuxFormat::Ts => command.push_str(" -strict unofficial -c:a copy -c:v copy "),
        MuxFormat::Mkv => {
            command.push_str(" -strict unofficial -c:a copy -c:v copy -c:s ");
            command.push_str(if has_srt { "srt " } else { "webvtt " });
        }
    }
    command.push_str(" -map_metadata -1 ");
    let mut stream_index = 0_usize;
    for file in files {
        let (language, description) = mux_language_and_description(file);
        command.push_str(&format!(
            " -metadata:s:{stream_index} language=\"{}\" ",
            language
        ));
        if let Some(description) = &description
            && !description.is_empty()
        {
            command.push_str(&format!(
                " -metadata:s:{stream_index} title=\"{description}\" "
            ));
        }
        stream_index += file.media_infos.len().max(1);
    }
    if files.iter().any(|file| {
        file.media_type != Some(MediaType::Audio) && file.media_type != Some(MediaType::Subtitles)
    }) {
        command.push_str(" -disposition:v:0 default ");
    }
    let audio_count = files
        .iter()
        .filter(|file| file.media_type == Some(MediaType::Audio))
        .count();
    if audio_count > 0 {
        command.push_str(" -disposition:a:0 default ");
        for index in 1..audio_count {
            command.push_str(&format!(" -disposition:a:{index} 0 "));
        }
        command.push_str(" -disposition:s 0 ");
    }
    if date_info {
        let generated_date;
        let date_string = match date_string {
            Some(value) => value,
            None => {
                generated_date = current_local_iso_timestamp();
                generated_date.as_str()
            }
        };
        command.push_str(&format!(" -metadata date=\"{}\" ", date_string));
    }
    command.push_str(" -ignore_unknown -copy_unknown ");
    let output_base_path = absolute_output_base_path(output_base_path)?;
    let output_path = path_with_appended_extension(&output_base_path, mux_extension(format));
    command.push_str(&format!(" \"{}\"", output_path.display()));
    Ok(MuxCommandPlan {
        program: binary.into(),
        arguments: command,
        working_directory: std::env::current_dir()?,
        output_path,
    })
}

/// Builds the mkvmerge mux-after-done command line.
pub fn plan_mkvmerge_mux(
    binary: impl Into<PathBuf>,
    files: &[OutputFile],
    output_base_path: &Path,
) -> Result<MuxCommandPlan> {
    let output_base_path = absolute_output_base_path(output_base_path)?;
    let output_path = path_with_appended_extension(&output_base_path, ".mkv");
    let mut command = format!("-q --output \"{}\"  --no-chapters ", output_path.display());
    let mut audio_default_seen = false;
    for file in files {
        let (language, description) = mux_language_and_description(file);
        command.push_str(&format!(" --language 0:\"{}\" ", language));
        if file.media_type == Some(MediaType::Subtitles) {
            command.push_str(" --default-track 0:no ");
        }
        if file.media_type == Some(MediaType::Audio) {
            if audio_default_seen {
                command.push_str(" --default-track 0:no ");
            }
            audio_default_seen = true;
        }
        if let Some(description) = &description
            && !description.is_empty()
        {
            command.push_str(&format!(" --track-name 0:\"{description}\" "));
        }
        command.push_str(&format!(" \"{}\" ", file.file_path.display()));
    }
    Ok(MuxCommandPlan {
        program: binary.into(),
        arguments: command,
        working_directory: std::env::current_dir()?,
        output_path,
    })
}

fn mux_language_and_description(file: &OutputFile) -> (String, Option<String>) {
    let Some(original) = file.lang_code.as_deref().filter(|value| !value.is_empty()) else {
        return ("und".to_string(), file.description.clone());
    };
    let language = convert_language_code(original);
    let description = file
        .description
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| language_display_name(original));
    (language, description)
}

fn convert_language_code(value: &str) -> String {
    let normalized = value.trim();
    let lower = normalized.to_ascii_lowercase();
    if let Some(language) = language_code_map(lower.as_str()) {
        return language.to_string();
    }
    if let Some((prefix, _)) = lower.split_once('-')
        && let Some(language) = language_code_map(prefix)
    {
        return language.to_string();
    }
    if normalized.len() == 3 && normalized.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return lower;
    }
    "und".to_string()
}

fn language_display_name(value: &str) -> Option<String> {
    let lower = value.trim().to_ascii_lowercase();
    language_name_map(lower.as_str())
        .or_else(|| {
            lower
                .split_once('-')
                .and_then(|(prefix, _)| language_name_map(prefix))
        })
        .map(str::to_string)
        .or_else(|| Some(value.to_string()))
}

fn language_code_map(value: &str) -> Option<&'static str> {
    match value {
        "default" => Some("und"),
        "ar" => Some("ara"),
        "bg" => Some("bul"),
        "ca" => Some("cat"),
        "zh" | "zho" | "chi" | "chs" | "zh-cn" | "zh-sg" | "zh-mo" | "zh-hans" | "zh-hant"
        | "zh-tw" | "zh-hant-tw" | "zh-hk" | "zh-hant-hk" | "yue" | "cmn" | "cmn-hans"
        | "cmn-hant" | "cantonese" | "mandarin" | "cn" | "cc" | "cz" => Some("chi"),
        "cs" => Some("ces"),
        "da" => Some("dan"),
        "de" => Some("deu"),
        "el" => Some("ell"),
        "en" | "english" => Some("eng"),
        "es" => Some("spa"),
        "fi" => Some("fin"),
        "fr" => Some("fra"),
        "he" => Some("heb"),
        "hu" => Some("hun"),
        "is" => Some("isl"),
        "it" => Some("ita"),
        "ja" | "japanese" => Some("jpn"),
        "ko" | "korean" => Some("kor"),
        "nl" => Some("nld"),
        "nb" => Some("nob"),
        "pl" => Some("pol"),
        "pt" => Some("por"),
        "ro" => Some("ron"),
        "ru" => Some("rus"),
        "hr" => Some("hrv"),
        "sk" => Some("slk"),
        "sq" => Some("sqi"),
        "sv" => Some("swe"),
        "th" | "thai" => Some("tha"),
        "tr" => Some("tur"),
        "ur" => Some("urd"),
        "id" => Some("ind"),
        "uk" => Some("ukr"),
        "vi" | "vietnamese" => Some("vie"),
        "ms" | "ma" => Some("msa"),
        _ => None,
    }
}

fn language_name_map(value: &str) -> Option<&'static str> {
    match value {
        "default" => Some("default"),
        "en" | "eng" | "english" => Some("English"),
        "ja" | "jpn" | "japanese" => Some("\u{65e5}\u{672c}\u{8a9e}"),
        "ko" | "kor" | "korean" => Some("\u{d55c}\u{ad6d}\u{c5b4}"),
        "vi" | "vie" | "vietnamese" => Some("Vietnamese"),
        "th" | "tha" | "thai" => Some("Thai"),
        "fr" | "fra" => Some("French"),
        "es" | "spa" => Some("Spanish"),
        "de" | "deu" => Some("German"),
        "it" | "ita" => Some("Italian"),
        "pt" | "por" => Some("Portuguese"),
        "ru" | "rus" => Some("Russian"),
        "zh" | "zho" | "chi" | "chs" | "zh-cn" | "zh-sg" | "zh-hans" | "cmn" | "cmn-hans"
        | "mandarin" | "cn" | "cz" => Some("\u{4e2d}\u{6587}"),
        "zh-mo" | "zh-hant" | "zh-tw" | "zh-hant-tw" | "zh-hk" | "zh-hant-hk" | "yue"
        | "cmn-hant" | "cantonese" | "cc" => Some("\u{4e2d}\u{6587}"),
        _ => None,
    }
}

fn path_with_appended_extension(base: &Path, extension: &str) -> PathBuf {
    let mut value = OsString::from(base.as_os_str());
    value.push(extension);
    PathBuf::from(value)
}

fn absolute_output_base_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

/// Creates output-file entries for mux imports with compatibility ordering.
pub fn output_files_with_imports(
    mut outputs: Vec<OutputFile>,
    imports: &[MuxImport],
    skip_subtitle: bool,
) -> Vec<OutputFile> {
    outputs.extend(imports.iter().map(|import| OutputFile {
        media_type: None,
        index: import.index,
        file_path: import.path.clone(),
        lang_code: import.language.clone(),
        description: import.name.clone(),
        media_infos: Vec::new(),
    }));
    if skip_subtitle {
        outputs.retain(|file| file.media_type != Some(MediaType::Subtitles));
    }
    outputs.sort_by_key(|file| file.index);
    outputs
}

/// Validates explicit mp4forge mux-after-done requests before side effects.
pub fn validate_mp4forge_mux_after_done(
    format: MuxFormat,
    muxer: MuxerKind,
    files: &[OutputFile],
    matrix: &Mp4forgeSupportMatrix,
) -> Result<()> {
    if muxer != MuxerKind::Mp4forge {
        return Ok(());
    }
    if format != MuxFormat::Mp4 {
        return Err(Error::mux(
            "mp4forge mux-after-done supports only mp4 output",
        ));
    }
    for file in files {
        if file.media_type == Some(MediaType::Subtitles) {
            return Err(Error::mux(
                "mp4forge mux-after-done does not support subtitle inputs",
            ));
        }
        if let Some(info) = file.media_infos.first()
            && let Some(codec) = info.base_info.as_deref().or(info.text.as_deref())
        {
            validate_codec_text(codec, file.media_type, matrix)?;
        }
    }
    Ok(())
}

/// Validates explicit mp4forge per-stream merge requests before side effects.
pub fn validate_mp4forge_merge_request(
    output_path: &Path,
    stream: &Stream,
    binary_concat_only: bool,
    live_pipe_mux: bool,
    matrix: &Mp4forgeSupportMatrix,
) -> Result<Mp4forgeSupport> {
    if binary_concat_only {
        return Err(Error::mux(
            "mp4forge merge cannot be used for binary-concat-only paths",
        ));
    }
    if live_pipe_mux {
        return Err(Error::mux(
            "mp4forge merge cannot be used for live pipe mux paths",
        ));
    }
    if stream.media_type == Some(MediaType::Subtitles) {
        return Err(Error::mux(
            "mp4forge merge does not support subtitle outputs",
        ));
    }
    let extension = output_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "mp4" | "m4a") {
        return Err(Error::mux(
            "mp4forge merge supports only mp4 and m4a outputs",
        ));
    }
    mp4forge_support_for_stream(stream, matrix)
}

/// Checks stream metadata against the explicit mp4forge support matrix.
pub fn mp4forge_support_for_stream(
    stream: &Stream,
    matrix: &Mp4forgeSupportMatrix,
) -> Result<Mp4forgeSupport> {
    if stream.playlist.as_ref().is_some_and(|playlist| {
        playlist.media_parts.iter().any(|part| {
            part.media_segments.iter().any(|segment| {
                !matches!(
                    segment.encryption.method,
                    EncryptionMethod::None | EncryptionMethod::Aes128 | EncryptionMethod::Aes128Ecb
                )
            })
        })
    }) {
        return Ok(Mp4forgeSupport::Unsupported {
            reason: "unsupported encrypted sample family".to_string(),
        });
    }
    let Some(codecs) = stream.codecs.as_deref().filter(|value| !value.is_empty()) else {
        return Ok(Mp4forgeSupport::RequiresProbe {
            reason: "codec metadata is not available before media probing".to_string(),
        });
    };
    validate_codec_text(codecs, stream.media_type, matrix)?;
    Ok(Mp4forgeSupport::Supported)
}

fn validate_codec_text(
    codec_text: &str,
    media_type: Option<MediaType>,
    matrix: &Mp4forgeSupportMatrix,
) -> Result<()> {
    let codecs = codec_text
        .split(',')
        .map(|codec| codec.trim().to_ascii_lowercase())
        .filter(|codec| !codec.is_empty())
        .collect::<Vec<_>>();
    if codecs.is_empty() {
        return Ok(());
    }
    let supported = match media_type {
        Some(MediaType::Audio) => codecs
            .iter()
            .all(|codec| has_any_prefix(codec, &matrix.audio_codecs)),
        Some(MediaType::Video) | None => codecs.iter().all(|codec| {
            has_any_prefix(codec, &matrix.video_codecs)
                || has_any_prefix(codec, &matrix.audio_codecs)
        }),
        Some(MediaType::Subtitles | MediaType::ClosedCaptions) => false,
    };
    if supported {
        Ok(())
    } else {
        Err(Error::mux(format!(
            "mp4forge does not support codec family: {codec_text}"
        )))
    }
}

fn has_any_prefix(value: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| value.starts_with(prefix))
}

fn append_simple_ffmpeg_output(
    command: &mut String,
    base: &str,
    use_aac_filter: bool,
    output_path: &Path,
) {
    command.push_str(base);
    if use_aac_filter {
        command.push_str(" -bsf:a aac_adtstoasc");
    }
    command.push_str(&format!(" \"{}\"", output_path.display()));
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension))
}
