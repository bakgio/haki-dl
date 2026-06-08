//! Terminal rendering for the command-line frontend.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use time::OffsetDateTime;

use crate::config::{DownloadOptions, LogLevel};
use crate::error::Result;
use crate::event::ProgressEvent;
use crate::observability::{format_duration, should_log};
use crate::progress::{AggregateProgress, SegmentProgress, StreamProgress};

const BAR_WIDTH: usize = 30;
const PROGRESS_LABEL_WIDTH: usize = 48;
const CLEAR_WIDTH: usize = 160;
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);
const ANSI_AQUA: &str = "96";
const ANSI_CYAN: &str = "96";
const ANSI_DEEPSKYBLUE3: &str = "38;5;32";
const ANSI_DEEPSKYBLUE3_1: &str = "38;5;45";
const ANSI_DEEPSKYBLUE1: &str = "38;5;39";
const ANSI_DARK_CYAN: &str = "38;2;0;139;139";
const ANSI_DARK_GREEN: &str = "38;2;0;100;0";
const ANSI_PROGRESS_COMPLETE: &str = "38;5;11";
const ANSI_PROGRESS_REMAINING: &str = "38;5;8";
const ANSI_PROGRESS_FINISHED: &str = "38;5;2";
const ANSI_REMAINING_TIME: &str = "94";
const ANSI_GREEN: &str = "32";
const ANSI_GREY: &str = "90";
const ANSI_STEELBLUE: &str = "38;5;67";
const ANSI_ENCRYPTION_METHOD: &str = "38;5;9";
const ANSI_DARK_ORANGE_3_1: &str = "38;5;172";
const ANSI_WHITE_ON_DARK_ORANGE_3_1: &str = "37;48;5;172";
const ANSI_WHITE_ON_GREEN: &str = "37;42";
const ANSI_WHITE_ON_RED: &str = "37;41";
const PROGRESS_ASCII_BAR: &str = "-";

#[derive(Clone, Debug)]
struct StreamSnapshot {
    label: String,
    started: bool,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    bytes_per_second: u64,
    low_speed_count: u32,
    completed_segments: u64,
    total_segments: Option<u64>,
}

#[derive(Clone, Debug)]
struct LiveSnapshot {
    label: String,
    bytes_per_second: u64,
    refreshed_duration: Duration,
    recorded_duration: Duration,
    recorded_segments: u64,
    total_segments: u64,
    is_waiting: bool,
    recorded_size: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct LiveProgressWidths {
    label: usize,
    duration: usize,
    size: usize,
    count: usize,
    percent: usize,
}

/// Stateful renderer used by the CLI callback.
#[derive(Debug)]
pub(crate) struct ConsoleRenderer {
    log_level: LogLevel,
    progress_enabled: bool,
    ansi_color: bool,
    ansi_progress: bool,
    progress_active: bool,
    progress_width: usize,
    progress_line_count: usize,
    non_ansi_progress_block_written: bool,
    last_progress_at: Option<Instant>,
    last_stream_id: Option<String>,
    stream_labels: BTreeMap<String, String>,
    stream_order: Vec<String>,
    stream_snapshots: BTreeMap<String, StreamSnapshot>,
    live_order: Vec<String>,
    live_snapshots: BTreeMap<String, LiveSnapshot>,
    pending_media_info: BTreeMap<String, Vec<String>>,
    next_media_info_index: usize,
    aggregate_snapshot: Option<AggregateProgress>,
    write_generation: u64,
}

impl ConsoleRenderer {
    pub(crate) fn new(options: &DownloadOptions) -> Self {
        let mut renderer = Self::with_log_level(options.log_level);
        renderer.ansi_color = !options.no_ansi_color;
        renderer.ansi_progress = !options.no_ansi_color;
        renderer
    }

    pub(crate) fn with_log_level(log_level: LogLevel) -> Self {
        Self {
            log_level,
            progress_enabled: log_level != LogLevel::Off,
            ansi_color: false,
            ansi_progress: false,
            progress_active: false,
            progress_width: CLEAR_WIDTH,
            progress_line_count: 0,
            non_ansi_progress_block_written: false,
            last_progress_at: None,
            last_stream_id: None,
            stream_labels: BTreeMap::new(),
            stream_order: Vec::new(),
            stream_snapshots: BTreeMap::new(),
            live_order: Vec::new(),
            live_snapshots: BTreeMap::new(),
            pending_media_info: BTreeMap::new(),
            next_media_info_index: 0,
            aggregate_snapshot: None,
            write_generation: 0,
        }
    }

    pub(crate) fn render_stdout(&mut self, event: &ProgressEvent) -> Result<()> {
        let mut stdout = io::stdout().lock();
        let write_generation = self.write_generation;
        self.render(&mut stdout, event)?;
        if self.write_generation != write_generation {
            stdout.flush()?;
        }
        Ok(())
    }

    pub(crate) fn render_error_stdout(&mut self, error_text: &str) -> Result<()> {
        let mut stdout = io::stdout().lock();
        self.finish_progress_line(&mut stdout)?;
        self.write_log_line(&mut stdout, LogLevel::Error, error_text)?;
        self.write_log_line(&mut stdout, LogLevel::Error, "Failed")?;
        stdout.flush()?;
        Ok(())
    }

    pub(crate) fn render<W: Write>(&mut self, writer: &mut W, event: &ProgressEvent) -> Result<()> {
        match event {
            ProgressEvent::PlanningStarted => Ok(()),
            ProgressEvent::Log { level, message } => self.write_log_line(writer, *level, message),
            ProgressEvent::ExtraLog { .. } => Ok(()),
            ProgressEvent::MediaInfo { stream_id, lines } => {
                self.write_media_info_event(writer, stream_id.as_deref(), lines)
            }
            ProgressEvent::LogFileCreated { path } => self.write_log_line(
                writer,
                LogLevel::Debug,
                &format!("Log file: {}", path.display()),
            ),
            ProgressEvent::UpdateCheckStarted => {
                self.write_log_line(writer, LogLevel::Debug, "Update check started")
            }
            ProgressEvent::TaskStartDelay {
                until,
                remaining: _,
            } => self.write_log_line(
                writer,
                LogLevel::Info,
                &format!("The program will wait until: {until}"),
            ),
            ProgressEvent::ManifestLoading => Ok(()),
            ProgressEvent::ManifestParsed { .. } => Ok(()),
            ProgressEvent::StreamSelected { .. } => Ok(()),
            ProgressEvent::StreamTaskCreated { stream_id, label } => {
                self.stream_labels.insert(stream_id.clone(), label.clone());
                if !self.stream_order.contains(stream_id) {
                    self.stream_order.push(stream_id.clone());
                }
                self.stream_snapshots
                    .entry(stream_id.clone())
                    .or_insert_with(|| StreamSnapshot {
                        label: label.clone(),
                        started: false,
                        downloaded_bytes: 0,
                        total_bytes: None,
                        bytes_per_second: 0,
                        low_speed_count: 0,
                        completed_segments: 0,
                        total_segments: Some(100),
                    });
                Ok(())
            }
            ProgressEvent::StreamProgress(progress) => {
                self.update_stream_snapshot(progress);
                self.update_live_snapshot_from_stream_progress(progress);
                let force = progress
                    .total_segments
                    .is_some_and(|total| total == progress.completed_segments);
                self.draw_progress_line(writer, force)
            }
            ProgressEvent::SegmentQueued {
                stream_id: _,
                segment_index: _,
            } => Ok(()),
            ProgressEvent::SegmentStarted {
                stream_id: _,
                segment_index: _,
            } => Ok(()),
            ProgressEvent::SegmentProgress(progress) => {
                self.update_segment_snapshot(progress);
                self.draw_progress_line(writer, false)
            }
            ProgressEvent::SegmentRetry { .. } => Ok(()),
            ProgressEvent::SegmentFinished {
                stream_id: _,
                segment_index: _,
            } => Ok(()),
            ProgressEvent::AggregateProgress(progress) => {
                self.aggregate_snapshot = Some(*progress);
                if self.stream_snapshots.is_empty() {
                    self.draw_progress_line(writer, false)?;
                }
                Ok(())
            }
            ProgressEvent::DecryptProgress { message, .. } => {
                self.write_log_line(writer, decrypt_progress_level(message), message)
            }
            ProgressEvent::MergeProgress { message, .. } => {
                self.write_log_line(writer, LogLevel::Info, message)
            }
            ProgressEvent::SubtitleProgress { message, .. } => {
                self.write_log_line(writer, LogLevel::Warn, message)
            }
            ProgressEvent::MuxProgress { message } => self.write_mux_line(writer, message),
            ProgressEvent::ExternalToolOutput { message } => {
                self.write_external_tool_line(writer, message)
            }
            ProgressEvent::ConsoleLine { message } => self.write_raw_console_line(writer, message),
            ProgressEvent::LiveRefresh {
                stream_id,
                label,
                refreshed_duration,
                recorded_duration,
                recorded_segments,
                total_segments,
                is_waiting,
                recorded_size,
            } => {
                let should_draw = recorded_size.is_some()
                    || *recorded_segments > 0
                    || *recorded_duration > Duration::ZERO
                    || *is_waiting;
                self.update_live_snapshot(
                    stream_id.as_deref(),
                    label.as_deref(),
                    *refreshed_duration,
                    *recorded_duration,
                    *recorded_segments,
                    *total_segments,
                    *is_waiting,
                    *recorded_size,
                );
                if should_draw {
                    self.draw_progress_line(writer, true)
                } else {
                    Ok(())
                }
            }
            ProgressEvent::LiveServiceInfo {
                program_id,
                service_provider,
                service_name,
            } => {
                self.write_log_line(writer, LogLevel::Info, &format!("Program Id: {program_id}"))?;
                if let Some(name) = service_name {
                    self.write_log_line(writer, LogLevel::Info, &format!("Service Name: {name}"))?;
                }
                if let Some(provider) = service_provider {
                    self.write_log_line(
                        writer,
                        LogLevel::Info,
                        &format!("Service Provider: {provider}"),
                    )?;
                }
                Ok(())
            }
            ProgressEvent::Warning { message } => {
                self.write_log_line(writer, LogLevel::Warn, message)
            }
            ProgressEvent::OutputArtifact(_) | ProgressEvent::Cleanup { .. } => Ok(()),
            ProgressEvent::Cancelled => self.write_log_line(writer, LogLevel::Warn, "Cancelled"),
            ProgressEvent::Finished { success } => {
                self.flush_all_pending_media_info_blocks(writer)?;
                self.finish_progress_line(writer)?;
                if *success {
                    self.write_log_line(writer, LogLevel::Info, "Done")
                } else {
                    self.write_log_line(writer, LogLevel::Error, "Failed")
                }
            }
        }
    }

    fn update_stream_snapshot(&mut self, progress: &StreamProgress) {
        let label = self
            .stream_labels
            .get(&progress.stream_id)
            .cloned()
            .unwrap_or_else(|| progress.stream_id.clone());
        self.last_stream_id = Some(progress.stream_id.clone());
        if !self.stream_order.contains(&progress.stream_id) {
            self.stream_order.push(progress.stream_id.clone());
        }
        self.stream_snapshots.insert(
            progress.stream_id.clone(),
            StreamSnapshot {
                label,
                started: true,
                downloaded_bytes: progress.downloaded_bytes,
                total_bytes: progress.total_bytes,
                bytes_per_second: progress.bytes_per_second,
                low_speed_count: progress.low_speed_count,
                completed_segments: progress.completed_segments,
                total_segments: progress.total_segments,
            },
        );
    }

    fn update_live_snapshot_from_stream_progress(&mut self, progress: &StreamProgress) {
        let Some(snapshot) = self.live_snapshots.get_mut(&progress.stream_id) else {
            return;
        };
        snapshot.recorded_segments = progress.completed_segments;
        if let Some(total) = progress.total_segments {
            snapshot.total_segments = total;
        }
        snapshot.bytes_per_second = progress.bytes_per_second;
        snapshot.is_waiting =
            snapshot.total_segments > 0 && snapshot.recorded_segments >= snapshot.total_segments;
    }

    fn update_segment_snapshot(&mut self, progress: &SegmentProgress) {
        let label = self
            .stream_labels
            .get(&progress.stream_id)
            .cloned()
            .unwrap_or_else(|| progress.stream_id.clone());
        self.last_stream_id = Some(progress.stream_id.clone());
        if !self.stream_order.contains(&progress.stream_id) {
            self.stream_order.push(progress.stream_id.clone());
        }
        let snapshot = self
            .stream_snapshots
            .entry(progress.stream_id.clone())
            .or_insert_with(|| StreamSnapshot {
                label,
                started: true,
                downloaded_bytes: 0,
                total_bytes: None,
                bytes_per_second: 0,
                low_speed_count: 0,
                completed_segments: 0,
                total_segments: None,
            });
        snapshot.started = true;
        snapshot.downloaded_bytes = progress.downloaded_bytes;
        if snapshot.total_segments == Some(1) {
            snapshot.total_bytes = progress.total_bytes;
        }
        snapshot.bytes_per_second = progress.bytes_per_second;
        snapshot.low_speed_count = progress.low_speed_count;
    }

    #[allow(clippy::too_many_arguments)]
    fn update_live_snapshot(
        &mut self,
        stream_id: Option<&str>,
        label: Option<&str>,
        refreshed_duration: Duration,
        recorded_duration: Duration,
        recorded_segments: u64,
        total_segments: u64,
        is_waiting: bool,
        recorded_size: Option<u64>,
    ) {
        if stream_id.is_none() && label.is_none() && total_segments == 0 {
            return;
        }
        let key = stream_id
            .or(label)
            .map(str::to_string)
            .unwrap_or_else(|| "live".to_string());
        if !self.live_order.contains(&key) {
            self.live_order.push(key.clone());
        }
        let bytes_per_second = self
            .aggregate_snapshot
            .map(|progress| progress.bytes_per_second)
            .or_else(|| {
                self.current_stream_snapshot()
                    .map(|snapshot| snapshot.bytes_per_second)
            })
            .unwrap_or_default();
        let label = label
            .map(str::to_string)
            .or_else(|| {
                stream_id
                    .and_then(|stream_id| self.stream_labels.get(stream_id))
                    .cloned()
            })
            .or_else(|| {
                self.current_stream_snapshot()
                    .map(|snapshot| snapshot.label.clone())
            })
            .unwrap_or_else(|| "Live".to_string());
        self.live_snapshots.insert(
            key,
            LiveSnapshot {
                label,
                bytes_per_second,
                refreshed_duration,
                recorded_duration,
                recorded_segments,
                total_segments,
                is_waiting,
                recorded_size,
            },
        );
    }

    fn draw_progress_line<W: Write>(&mut self, writer: &mut W, force: bool) -> Result<()> {
        if !self.progress_enabled {
            return Ok(());
        }
        if !force
            && self
                .last_progress_at
                .is_some_and(|last| last.elapsed() < PROGRESS_MIN_INTERVAL)
        {
            return Ok(());
        }
        let lines = self.progress_lines();
        if lines.is_empty() {
            return Ok(());
        }
        if !self.ansi_progress {
            write_non_ansi_progress_block(writer, &lines)?;
            self.non_ansi_progress_block_written = true;
            self.last_progress_at = Some(Instant::now());
            self.note_write();
            return Ok(());
        }
        self.write_progress_update(writer, &lines)?;
        self.progress_active = true;
        self.last_progress_at = Some(Instant::now());
        self.note_write();
        Ok(())
    }

    fn progress_lines(&self) -> Vec<String> {
        if !self.live_snapshots.is_empty() {
            let snapshots = self
                .live_order
                .iter()
                .filter_map(|stream_id| self.live_snapshots.get(stream_id))
                .chain(
                    self.live_snapshots
                        .iter()
                        .filter(|(stream_id, _)| !self.live_order.contains(stream_id))
                        .map(|(_, snapshot)| snapshot),
                )
                .collect::<Vec<_>>();
            let widths = live_progress_widths(&snapshots);
            return snapshots
                .into_iter()
                .map(|snapshot| {
                    format_live_progress_line_with_color(snapshot, widths, self.ansi_color)
                })
                .collect();
        }
        if !self.stream_snapshots.is_empty() {
            return self.stream_progress_lines();
        }
        self.aggregate_snapshot
            .map(|progress| format_aggregate_progress_line_with_color(progress, self.ansi_color))
            .into_iter()
            .collect()
    }

    fn stream_progress_lines(&self) -> Vec<String> {
        let snapshots = self
            .stream_order
            .iter()
            .filter_map(|stream_id| self.stream_snapshots.get(stream_id))
            .chain(
                self.stream_snapshots
                    .iter()
                    .filter(|(stream_id, _)| !self.stream_order.contains(stream_id))
                    .map(|(_, snapshot)| snapshot),
            )
            .collect::<Vec<_>>();
        let label_width = snapshots
            .iter()
            .map(|snapshot| progress_label(&snapshot.label).chars().count())
            .max()
            .unwrap_or_default();
        snapshots
            .into_iter()
            .map(|snapshot| {
                format_stream_progress_line_with_color(snapshot, label_width, self.ansi_color)
            })
            .collect()
    }

    fn write_progress_update<W: Write>(&mut self, writer: &mut W, lines: &[String]) -> Result<()> {
        self.clear_progress_line(writer)?;
        self.progress_width = self
            .progress_width
            .max(
                lines
                    .iter()
                    .map(|line| visible_width(line))
                    .max()
                    .unwrap_or_default(),
            )
            .max(CLEAR_WIDTH);
        if self.ansi_progress {
            for line in lines {
                writeln!(writer, "{line}")?;
            }
            self.progress_line_count = lines.len();
        } else if let Some(line) = lines.last() {
            let padding = self.progress_width.saturating_sub(visible_width(line));
            write!(writer, "\r{line}")?;
            if padding > 0 {
                write!(writer, "{}", " ".repeat(padding))?;
            }
            self.progress_line_count = 1;
        }
        Ok(())
    }

    fn current_stream_snapshot(&self) -> Option<&StreamSnapshot> {
        self.last_stream_id
            .as_ref()
            .and_then(|stream_id| self.stream_snapshots.get(stream_id))
            .or_else(|| self.stream_snapshots.values().next())
    }

    fn write_log_line<W: Write>(
        &mut self,
        writer: &mut W,
        level: LogLevel,
        message: &str,
    ) -> Result<()> {
        if !should_log(self.log_level, level) {
            return Ok(());
        }
        let Some(prefix) = log_level_prefix(level) else {
            return Ok(());
        };
        if level == LogLevel::Debug && is_extra_debug_noise(message) {
            return Ok(());
        }
        self.write_console_title_if_needed(writer, message)?;
        let had_progress = self.progress_active;
        self.clear_progress_line(writer)?;
        if message.is_empty() {
            writeln!(
                writer,
                "{} {}",
                console_timestamp(),
                colorize_log_prefix(level, prefix, self.ansi_color)
            )?;
            if had_progress && self.ansi_progress {
                let lines = self.progress_lines();
                if !lines.is_empty() {
                    self.write_progress_update(writer, &lines)?;
                    self.progress_active = true;
                }
            }
            self.note_write();
            return Ok(());
        }
        let mut wrote = false;
        for line in message.lines() {
            writeln!(
                writer,
                "{} {} {}",
                console_timestamp(),
                colorize_log_prefix(level, prefix, self.ansi_color),
                colorize_log_message(level, line, self.ansi_color)
            )?;
            wrote = true;
        }
        if had_progress && self.ansi_progress {
            let lines = self.progress_lines();
            if !lines.is_empty() {
                self.write_progress_update(writer, &lines)?;
                self.progress_active = true;
            }
        }
        if wrote {
            self.note_write();
        }
        Ok(())
    }

    fn write_mux_line<W: Write>(&mut self, writer: &mut W, message: &str) -> Result<()> {
        self.write_special_warn_line(writer, message, colorize_mux_message)
    }

    fn write_media_info_event<W: Write>(
        &mut self,
        writer: &mut W,
        stream_id: Option<&str>,
        lines: &[String],
    ) -> Result<()> {
        let Some(stream_id) = stream_id else {
            return self.write_media_info_blocks(writer, &[lines.to_vec()]);
        };
        if self.live_order.is_empty() || !self.live_order.iter().any(|value| value == stream_id) {
            return self.write_media_info_blocks(writer, &[lines.to_vec()]);
        }
        self.pending_media_info
            .insert(stream_id.to_string(), lines.to_vec());
        self.flush_ordered_media_info_blocks(writer)
    }

    fn flush_ordered_media_info_blocks<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        if self.next_media_info_index < self.live_order.len()
            && !self.live_order[self.next_media_info_index..]
                .iter()
                .all(|stream_id| self.pending_media_info.contains_key(stream_id))
        {
            return Ok(());
        }
        let mut blocks = Vec::new();
        while self.next_media_info_index < self.live_order.len() {
            let stream_id = self.live_order[self.next_media_info_index].clone();
            let Some(lines) = self.pending_media_info.remove(&stream_id) else {
                break;
            };
            self.next_media_info_index += 1;
            blocks.push(lines);
        }
        if !blocks.is_empty() {
            self.write_media_info_blocks(writer, &blocks)?;
        }
        Ok(())
    }

    fn flush_all_pending_media_info_blocks<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        self.flush_ordered_media_info_blocks(writer)?;
        let mut blocks = Vec::new();
        while self.next_media_info_index < self.live_order.len() {
            let stream_id = self.live_order[self.next_media_info_index].clone();
            self.next_media_info_index += 1;
            if let Some(lines) = self.pending_media_info.remove(&stream_id) {
                blocks.push(lines);
            }
        }
        while let Some(stream_id) = self.pending_media_info.keys().next().cloned() {
            if let Some(lines) = self.pending_media_info.remove(&stream_id) {
                blocks.push(lines);
            }
        }
        if !blocks.is_empty() {
            self.write_media_info_blocks(writer, &blocks)?;
        }
        Ok(())
    }

    fn write_media_info_blocks<W: Write>(
        &mut self,
        writer: &mut W,
        blocks: &[Vec<String>],
    ) -> Result<()> {
        let show_warn = should_log(self.log_level, LogLevel::Warn);
        let show_info = should_log(self.log_level, LogLevel::Info);
        if !show_warn && !show_info {
            return Ok(());
        }
        if blocks.is_empty() {
            return Ok(());
        }
        let had_progress = self.progress_active;
        self.clear_progress_line(writer)?;
        if show_warn {
            for _ in blocks {
                writeln!(
                    writer,
                    "{} {} {}",
                    console_timestamp(),
                    colorize_log_prefix(LogLevel::Warn, "WARN :", self.ansi_color),
                    colorize_log_message(LogLevel::Warn, "Reading media info...", self.ansi_color)
                )?;
            }
        }
        if show_info {
            for lines in blocks {
                for line in lines {
                    writeln!(
                        writer,
                        "{} {} {}",
                        console_timestamp(),
                        colorize_log_prefix(LogLevel::Info, "INFO :", self.ansi_color),
                        colorize_log_message(LogLevel::Info, line, self.ansi_color)
                    )?;
                }
            }
        }
        if had_progress && self.ansi_progress {
            let lines = self.progress_lines();
            if !lines.is_empty() {
                self.write_progress_update(writer, &lines)?;
                self.progress_active = true;
            }
        }
        self.note_write();
        Ok(())
    }

    fn write_external_tool_line<W: Write>(&mut self, writer: &mut W, message: &str) -> Result<()> {
        self.write_special_warn_line(writer, message, |line, enabled| {
            if enabled {
                paint("90", line)
            } else {
                line.to_string()
            }
        })
    }

    fn write_raw_console_line<W: Write>(&mut self, writer: &mut W, message: &str) -> Result<()> {
        let had_progress = self.progress_active;
        self.clear_progress_line(writer)?;
        let mut wrote = false;
        for line in message.lines() {
            writeln!(writer, "{line}")?;
            wrote = true;
        }
        if had_progress && self.ansi_progress {
            let lines = self.progress_lines();
            if !lines.is_empty() {
                self.write_progress_update(writer, &lines)?;
                self.progress_active = true;
            }
        }
        if wrote {
            self.note_write();
        }
        Ok(())
    }

    fn write_special_warn_line<W: Write>(
        &mut self,
        writer: &mut W,
        message: &str,
        colorize: fn(&str, bool) -> String,
    ) -> Result<()> {
        if !should_log(self.log_level, LogLevel::Warn) {
            return Ok(());
        }
        let had_progress = self.progress_active;
        self.clear_progress_line(writer)?;
        let mut wrote = false;
        for line in message.lines() {
            writeln!(
                writer,
                "{} {} {}",
                console_timestamp(),
                colorize_log_prefix(LogLevel::Warn, "WARN :", self.ansi_color),
                colorize(line, self.ansi_color)
            )?;
            wrote = true;
        }
        if had_progress && self.ansi_progress {
            let lines = self.progress_lines();
            if !lines.is_empty() {
                self.write_progress_update(writer, &lines)?;
                self.progress_active = true;
            }
        }
        if wrote {
            self.note_write();
        }
        Ok(())
    }

    fn write_console_title_if_needed<W: Write>(&self, writer: &mut W, message: &str) -> Result<()> {
        if !self.ansi_color {
            return Ok(());
        }
        let Some(latest) = message.strip_prefix("New version detected! ") else {
            return Ok(());
        };
        write!(writer, "\x1b]0;New version detected! {latest}\x07")?;
        Ok(())
    }

    fn clear_progress_line<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        if self.progress_active {
            if self.ansi_progress {
                for _ in 0..self.progress_line_count {
                    write!(writer, "\x1b[1A\r\x1b[2K")?;
                }
            } else {
                write!(writer, "\r{:width$}\r", "", width = self.progress_width)?;
            }
            self.progress_active = false;
            self.progress_line_count = 0;
        }
        Ok(())
    }

    fn finish_progress_line<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        if self.ansi_progress {
            if self.progress_active {
                self.clear_progress_line(writer)?;
            }
            return Ok(());
        }
        let lines = self.progress_lines();
        if !self.non_ansi_progress_block_written {
            write_non_ansi_progress_block(writer, &lines)?;
        }
        self.non_ansi_progress_block_written = false;
        self.progress_active = false;
        self.progress_line_count = 0;
        Ok(())
    }

    fn note_write(&mut self) {
        self.write_generation = self.write_generation.saturating_add(1);
    }
}

fn write_non_ansi_progress_block<W: Write>(writer: &mut W, lines: &[String]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    writeln!(writer)?;
    for line in lines {
        writeln!(writer, "{line}")?;
    }
    writeln!(writer)?;
    Ok(())
}

fn log_level_prefix(level: LogLevel) -> Option<&'static str> {
    match level {
        LogLevel::Debug => Some("DEBUG:"),
        LogLevel::Info => Some("INFO :"),
        LogLevel::Warn => Some("WARN :"),
        LogLevel::Error => Some("ERROR:"),
        LogLevel::Off => None,
    }
}

fn colorize_log_prefix(level: LogLevel, prefix: &str, enabled: bool) -> String {
    if !enabled {
        return prefix.to_string();
    }
    let (level_text, separator) = log_prefix_parts(prefix);
    match level {
        LogLevel::Debug => format!("{}{}", paint("4;38;5;8", level_text), separator),
        LogLevel::Info => format!("{}{}", paint("4;38;2;84;140;38", level_text), separator),
        LogLevel::Warn => format!("{}{}", paint("4;38;2;168;144;34", level_text), separator),
        LogLevel::Error => format!("{}{}", paint("4;38;5;196", level_text), separator),
        LogLevel::Off => prefix.to_string(),
    }
}

fn log_prefix_parts(prefix: &str) -> (&str, &str) {
    let Some(colon_index) = prefix.find(':') else {
        return (prefix, "");
    };
    let level_end = prefix[..colon_index].trim_end().len();
    prefix.split_at(level_end)
}

fn colorize_log_message(level: LogLevel, line: &str, enabled: bool) -> String {
    if !enabled {
        return line.to_string();
    }
    if line == "Done" {
        return paint(ANSI_WHITE_ON_GREEN, line);
    }
    if line == "Failed" {
        return paint(ANSI_WHITE_ON_RED, line);
    }
    if line == "Force Exit..." {
        return paint(ANSI_DARK_ORANGE_3_1, line);
    }
    if let Some(value) = line.strip_prefix("User customed range: ") {
        return format!("User customed range: {}", paint("4;36", value));
    }
    if let Some(value) = line.strip_prefix("User customed Ad keyword: ") {
        return format!("User customed Ad keyword: {}", paint("4;36", value));
    }
    if let Some(value) = line.strip_prefix("OK ") {
        return format!("{} {}", paint(ANSI_GREEN, "OK"), paint(ANSI_GREY, value));
    }
    if let Some(value) = line.strip_prefix("Save Name: ") {
        return format!("Save Name: {}", paint(ANSI_DEEPSKYBLUE1, value));
    }
    if let Some(value) = line.strip_prefix("Content Matched: ") {
        return format!("Content Matched: {}", colorize_content_match_value(value));
    }
    if let Some(value) = line.strip_prefix("New version detected! ") {
        return format!(
            "{} {}",
            paint(ANSI_CYAN, "New version detected!"),
            paint(ANSI_ENCRYPTION_METHOD, value)
        );
    }
    if line.starts_with("Decrypting using ") && line.ends_with("...") {
        return paint(ANSI_GREY, line);
    }
    if line == "Namespace missing, try fix..." {
        return paint(ANSI_GREY, line);
    }
    if line == "Live stream found" {
        return paint(ANSI_WHITE_ON_DARK_ORANGE_3_1, line);
    }
    if let Some(value) = line.strip_prefix("Named pipe created: ") {
        return format!("Named pipe created: {}", paint(ANSI_CYAN, value));
    }
    if let Some(value) = line.strip_prefix("Program Id: ") {
        return format!("Program Id: {}", paint(ANSI_CYAN, value));
    }
    if let Some(value) = line.strip_prefix("Service Name: ") {
        return format!("Service Name: {}", paint(ANSI_CYAN, value));
    }
    if let Some(value) = line.strip_prefix("Service Provider: ") {
        return format!("Service Provider: {}", paint(ANSI_CYAN, value));
    }
    if is_media_info_line(line) {
        return colorize_media_info_line(line);
    }
    if level == LogLevel::Warn
        && (line.starts_with("Type:") || line.starts_with("PSSH(") || line.starts_with("KID:"))
    {
        return paint(ANSI_GREY, line);
    }
    if level == LogLevel::Warn && is_retry_warning_line(line) {
        return paint(ANSI_GREY, line);
    }
    if level == LogLevel::Warn
        && let Some(four_cc) = line.strip_suffix(" not supported! Skiped.")
    {
        return format!("{} not supported! Skiped.", paint(ANSI_GREEN, four_cc));
    }
    if level == LogLevel::Warn && is_auto_binary_merge_warning(line) {
        return paint(ANSI_DARK_ORANGE_3_1, line);
    }
    if level == LogLevel::Warn && is_source_warning_line(line) {
        return paint(ANSI_DARK_ORANGE_3_1, line);
    }
    if level == LogLevel::Warn && is_ad_cleanup_count_line(line) {
        return paint(ANSI_GREY, line);
    }
    if level == LogLevel::Warn && is_media_tool_stderr_line(line) {
        return paint(ANSI_GREY, line);
    }
    colorize_stream_label_text(line)
}

fn colorize_mux_message(line: &str, enabled: bool) -> String {
    if !enabled {
        return line.to_string();
    }
    if line == "Cleaning files..." {
        return paint(ANSI_GREY, line);
    }
    if let Some(value) = line.strip_prefix("Muxing to ") {
        return format!("Muxing to {}", paint(ANSI_GREY, value));
    }
    if let Some(value) = line.strip_prefix("Rename to ") {
        return format!("Rename to {}", paint(ANSI_GREY, value));
    }
    if let Some(value) = line.strip_prefix("Mux with named pipe, to ") {
        return format!(
            "Mux with named pipe, to {}",
            paint(ANSI_DEEPSKYBLUE1, value)
        );
    }
    if line.starts_with("-y -fflags ") {
        return paint(ANSI_DEEPSKYBLUE1, line);
    }
    if looks_like_mux_input_file(line) {
        return paint(ANSI_GREY, line);
    }
    colorize_log_message(LogLevel::Warn, line, enabled)
}

fn colorize_content_match_value(value: &str) -> String {
    match value {
        "HTTP Live MPEG2-TS" => paint("37;48;5;40", value),
        "Dynamic Adaptive Streaming over HTTP" => paint("37;48;5;171", value),
        "Microsoft Smooth Streaming" => paint("37;48;5;75", value),
        "HTTP Live Streaming" | "Binary Data" => paint("37;48;5;39", value),
        _ => value.to_string(),
    }
}

fn is_media_info_line(line: &str) -> bool {
    let Some((_, rest)) = line.split_once(": ") else {
        return false;
    };
    (line.starts_with('[') || line.starts_with("NaN:"))
        && matches!(
            rest.split(',').next().map(str::trim),
            Some("Audio" | "Video" | "Subtitle" | "Unknown")
        )
}

fn colorize_media_info_line(line: &str) -> String {
    let (base, suffix) = split_media_info_suffix(line);
    let mut output = paint(ANSI_STEELBLUE, base);
    if !suffix.is_empty() {
        output.push(' ');
        output.push_str(&paint(ANSI_DARK_ORANGE_3_1, suffix));
    }
    output
}

fn is_media_tool_stderr_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("ffmpeg version")
        || trimmed.starts_with("Input #")
        || trimmed.starts_with("Output #")
        || trimmed.starts_with("Stream #")
        || trimmed.starts_with("Stream mapping:")
        || trimmed.starts_with("Press [q]")
        || trimmed.starts_with("frame=")
        || trimmed.starts_with("size=")
        || trimmed.starts_with("video:")
        || trimmed.starts_with("audio:")
        || trimmed.starts_with("subtitle:")
        || trimmed.starts_with("muxing overhead:")
        || trimmed.starts_with('[')
}

fn is_auto_binary_merge_warning(line: &str) -> bool {
    matches!(
        line,
        "fMP4 is detected, binary merging is automatically enabled"
            | "Dolby Vision content is detected, binary merging is automatically enabled"
            | "An unrecognized encryption method is detected, binary merging is automatically enabled"
            | "When CENC encryption is detected, binary merging is automatically enabled"
            | "Live streams are detected, binary merging is automatically enabled"
            | "MuxAfterDone is detected, binary merging is automatically enabled"
            | "Dolby Vision content is detected, mux after done is automatically disabled"
    )
}

fn is_source_warning_line(line: &str) -> bool {
    matches!(
        line,
        "Please note that custom range may sometimes result in audio and video being out of sync"
            | "The entire file has been cut into small segments to accelerate"
            | "Real-time decryption has been disabled"
            | "When enabling real-time decryption, it is recommended to use shaka-packager instead of mp4decrypt/ffmpeg"
            | "Multiple #EXT-X-MAP tags are now allowed for detection. However, this software may not handle them correctly. Please manually verify the content's integrity"
            | "Live stream found"
            | "Live recording limit reached, will stop recording soon"
            | "LivePipeMux detected, forced enable LiveRealTimeMerge"
    ) || line.starts_with("Live recording duration limit: ")
}

fn is_retry_warning_line(line: &str) -> bool {
    let Some((left, right)) = line.rsplit_once(" (") else {
        return false;
    };
    if left.is_empty() {
        return false;
    }
    let Some(counter) = right.strip_suffix(')') else {
        return false;
    };
    let Some((attempt, max)) = counter.split_once('/') else {
        return false;
    };
    !attempt.is_empty()
        && !max.is_empty()
        && attempt.chars().all(|ch| ch.is_ascii_digit())
        && max.chars().all(|ch| ch.is_ascii_digit())
}

fn is_ad_cleanup_count_line(line: &str) -> bool {
    let Some((left, right)) = line.split_once(" segments => ") else {
        return false;
    };
    left.chars().all(|ch| ch.is_ascii_digit())
        && right
            .strip_suffix(" segments")
            .is_some_and(|value| value.chars().all(|ch| ch.is_ascii_digit()))
}

fn looks_like_mux_input_file(line: &str) -> bool {
    if line.contains(' ') || line.contains(':') || line.contains('/') || line.contains('\\') {
        return false;
    }
    let Some((_, extension)) = line.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "mp4"
            | "m4a"
            | "m4v"
            | "mkv"
            | "ts"
            | "aac"
            | "ac3"
            | "eac3"
            | "srt"
            | "vtt"
            | "ass"
            | "ssa"
            | "h264"
            | "h265"
            | "hevc"
    )
}

fn split_media_info_suffix(line: &str) -> (&str, &str) {
    for suffix in [" [HDR]", " [DOVI]"] {
        if let Some(base) = line.strip_suffix(suffix) {
            return (base, suffix.trim_start());
        }
    }
    (line, "")
}

fn colorize_stream_label_text(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("Start downloading...") {
        return format!("Start downloading...{}", colorize_stream_label_text(rest));
    }
    for (prefix, code) in [
        ("Vid", ANSI_AQUA),
        ("Aud", ANSI_DEEPSKYBLUE3),
        ("Sub", ANSI_DEEPSKYBLUE3_1),
    ] {
        if line == prefix {
            return paint(code, prefix);
        }
        if let Some(rest) = line.strip_prefix(prefix)
            && rest.starts_with(' ')
        {
            return format!(
                "{}{}",
                paint(code, prefix),
                colorize_encryption_tokens(rest)
            );
        }
    }
    colorize_encryption_tokens(line)
}

fn colorize_encryption_tokens(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(index) = rest.find('*') {
        output.push_str(&rest[..index]);
        rest = &rest[index..];
        let token_len = encryption_token_len(rest);
        if token_len == 0 {
            output.push('*');
            rest = &rest[1..];
            continue;
        }
        output.push_str(&paint(ANSI_ENCRYPTION_METHOD, &rest[..token_len]));
        rest = &rest[token_len..];
    }
    output.push_str(rest);
    output
}

fn encryption_token_len(value: &str) -> usize {
    let mut chars = value.char_indices();
    match chars.next() {
        Some((_, '*')) => {}
        _ => return 0,
    }
    let mut end = 1;
    let mut has_method_char = false;
    for (index, ch) in chars {
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, '_' | ',' | '-') {
            has_method_char = true;
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if has_method_char { end } else { 0 }
}

fn paint(code: &str, value: &str) -> String {
    format!("\x1b[{code}m{value}\x1b[0m")
}

fn visible_width(value: &str) -> usize {
    let mut width = 0_usize;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

fn decrypt_progress_level(message: &str) -> LogLevel {
    if matches!(
        message.trim_start(),
        value if value.starts_with("Type:")
            || value.starts_with("PSSH(")
            || value.starts_with("KID:")
    ) {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

fn is_extra_debug_noise(message: &str) -> bool {
    let value = message.trim_start();
    value.starts_with("Log file:")
        || value.starts_with("Update check started")
        || value.starts_with("ffmpeg =>")
        || value.starts_with("mkvmerge =>")
        || value.starts_with("mp4forge =>")
        || value.starts_with("mp4decrypt =>")
        || value.starts_with("shaka-packager =>")
        || value.starts_with("User-Defined Header =>")
        || value.starts_with("#EXTM3U")
        || value.starts_with("<MPD")
        || value.starts_with("<SmoothStreamingMedia")
        || value.starts_with("DropVideoFilter =>")
        || value.starts_with("DropAudioFilter =>")
        || value.starts_with("DropSubtitleFilter =>")
        || value.starts_with("VideoFilter =>")
        || value.starts_with("AudioFilter =>")
        || value.starts_with("SubtitleFilter =>")
        || value.starts_with("Output:")
        || value.starts_with("Cleaning ")
        || value.contains(": queued segment ")
        || value.contains(": downloading segment ")
        || value.contains(": finished segment ")
}

fn console_timestamp() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000
    )
}

fn format_stream_progress_line_with_color(
    snapshot: &StreamSnapshot,
    label_width: usize,
    ansi_color: bool,
) -> String {
    let display_total_bytes = display_total_bytes(snapshot);
    let percent = progress_percent(
        snapshot.completed_segments,
        snapshot.total_segments,
        snapshot.downloaded_bytes,
        display_total_bytes,
    );
    let label = progress_label(&snapshot.label);
    let label = format!("{label:<label_width$}");
    let segment_status = segment_status(snapshot.completed_segments, snapshot.total_segments);
    let byte_status = byte_status(
        snapshot.started,
        snapshot.downloaded_bytes,
        display_total_bytes,
    );
    let speed_status = if percent >= 100.0 {
        "-".to_string()
    } else {
        speed_status(
            snapshot.started,
            snapshot.bytes_per_second,
            snapshot.low_speed_count,
        )
    };
    let mut remaining_status = remaining_status(
        snapshot.started,
        snapshot.downloaded_bytes,
        display_total_bytes,
        snapshot.bytes_per_second,
    );
    if percent >= 100.0 {
        remaining_status = "00:00:00".to_string();
    }
    let percent_text = format!("{percent:.2}%");
    let (label, bar, segment_status, percent_text, byte_status, speed_status, remaining_status) =
        if ansi_color {
            (
                colorize_stream_label_text(&label),
                progress_bar_with_color(percent, true),
                colorize_finished_value(&segment_status, percent),
                colorize_finished_value(&percent_text, percent),
                paint(ANSI_DARK_CYAN, &byte_status),
                if speed_status == "-" {
                    speed_status
                } else {
                    paint(ANSI_GREEN, &speed_status)
                },
                paint(ANSI_REMAINING_TIME, &remaining_status),
            )
        } else {
            (
                label,
                progress_bar(percent),
                segment_status,
                percent_text,
                byte_status,
                speed_status,
                remaining_status,
            )
        };
    format!(
        "{label} {bar} {segment_status} {percent_text} {byte_status} {speed_status} {remaining_status}"
    )
}

fn display_total_bytes(snapshot: &StreamSnapshot) -> Option<u64> {
    if snapshot.total_bytes.is_some() {
        return snapshot.total_bytes;
    }
    let total_segments = snapshot.total_segments?;
    if total_segments == 0 || snapshot.completed_segments == 0 {
        return None;
    }
    if snapshot.completed_segments >= total_segments {
        return None;
    }
    Some(
        snapshot
            .downloaded_bytes
            .saturating_mul(total_segments)
            .saturating_div(snapshot.completed_segments),
    )
}

fn format_aggregate_progress_line_with_color(
    progress: AggregateProgress,
    ansi_color: bool,
) -> String {
    let percent = progress_percent(0, None, progress.downloaded_bytes, progress.total_bytes);
    let byte_status = byte_status(true, progress.downloaded_bytes, progress.total_bytes);
    let speed_status = if percent >= 100.0 {
        "-".to_string()
    } else {
        speed_status(true, progress.bytes_per_second, 0)
    };
    let mut remaining_status = remaining_status(
        true,
        progress.downloaded_bytes,
        progress.total_bytes,
        progress.bytes_per_second,
    );
    if percent >= 100.0 {
        remaining_status = "00:00:00".to_string();
    }
    let percent_text = format!("{percent:.2}%");
    if ansi_color {
        format!(
            "{} {} - {} {} {} {}",
            colorize_stream_label_text("Total"),
            progress_bar_with_color(percent, true),
            colorize_finished_value(&percent_text, percent),
            paint(ANSI_DARK_CYAN, &byte_status),
            if speed_status == "-" {
                speed_status
            } else {
                paint(ANSI_GREEN, &speed_status)
            },
            paint(ANSI_REMAINING_TIME, &remaining_status)
        )
    } else {
        format!(
            "Total {} - {} {} {} {}",
            progress_bar(percent),
            percent_text,
            byte_status,
            speed_status,
            remaining_status
        )
    }
}

fn live_progress_widths(snapshots: &[&LiveSnapshot]) -> LiveProgressWidths {
    let mut widths = LiveProgressWidths {
        percent: 4,
        ..LiveProgressWidths::default()
    };
    for snapshot in snapshots {
        let label = progress_label(&snapshot.label);
        widths.label = widths.label.max(label.chars().count());
        if snapshot.recorded_size.is_some() {
            widths.duration = widths.duration.max(
                format_duration_short(snapshot.recorded_duration)
                    .chars()
                    .count(),
            );
            if let Some(size) = snapshot.recorded_size {
                widths.size = widths
                    .size
                    .max(format_file_size_display(size).chars().count());
            }
        } else {
            widths.duration = widths
                .duration
                .max(segmented_live_duration_text(snapshot).chars().count());
        }
        widths.count = widths.count.max(live_count_text(snapshot).chars().count());
    }
    widths
}

fn format_live_progress_line_with_color(
    snapshot: &LiveSnapshot,
    widths: LiveProgressWidths,
    ansi_color: bool,
) -> String {
    let status = if snapshot.is_waiting {
        "Waiting  "
    } else {
        "Recording"
    };
    if let Some(size) = snapshot.recorded_size {
        let label = pad_right(&progress_label(&snapshot.label), widths.label);
        let duration = pad_left(
            &format_duration_short(snapshot.recorded_duration),
            widths.duration,
        );
        let size = pad_left(&format_file_size_display(size), widths.size);
        let count = pad_left(&live_count_text_with_status(snapshot, status), widths.count);
        let speed = live_speed_status(snapshot.bytes_per_second, snapshot.is_waiting);
        let (label, duration, size, count, speed) = if ansi_color {
            (
                colorize_stream_label_text(&label),
                paint(ANSI_DARK_GREEN, &duration),
                paint(ANSI_DARK_CYAN, &size),
                colorize_recording_status(&count, snapshot.is_waiting),
                if speed == "-" {
                    speed
                } else {
                    paint(ANSI_GREEN, &speed)
                },
            )
        } else {
            (label, duration, size, count, speed)
        };
        return format!("{} {} {} {} {}", label, duration, size, count, speed);
    }
    let percent = if snapshot.total_segments == 0 {
        0.0
    } else {
        ((snapshot.recorded_segments as f64 / snapshot.total_segments as f64) * 100.0)
            .clamp(0.0, 100.0)
    };
    let label = pad_right(&progress_label(&snapshot.label), widths.label);
    let speed = live_speed_status(snapshot.bytes_per_second, snapshot.is_waiting);
    let duration = pad_left(&segmented_live_duration_text(snapshot), widths.duration);
    let count = pad_left(&live_count_text_with_status(snapshot, status), widths.count);
    let percent_text = pad_left(&format!("{percent:.0}%"), widths.percent);
    let (label, duration, count, percent_text, speed) = if ansi_color {
        (
            colorize_stream_label_text(&label),
            paint(ANSI_GREY, &duration),
            colorize_recording_status(&count, snapshot.is_waiting),
            colorize_finished_value(&percent_text, percent),
            if speed == "-" {
                speed
            } else {
                paint(ANSI_GREEN, &speed)
            },
        )
    } else {
        (label, duration, count, percent_text, speed)
    };
    format!("{label} {duration} {count} {percent_text} {speed}")
}

fn segmented_live_duration_text(snapshot: &LiveSnapshot) -> String {
    format!(
        "{}/{}",
        format_duration_short(segmented_live_display_recorded_duration(snapshot)),
        format_duration_short(snapshot.refreshed_duration)
    )
}

fn segmented_live_display_recorded_duration(snapshot: &LiveSnapshot) -> Duration {
    if snapshot.is_waiting
        && snapshot.total_segments > 0
        && snapshot.recorded_segments >= snapshot.total_segments
        && snapshot.refreshed_duration > snapshot.recorded_duration
    {
        snapshot.refreshed_duration
    } else {
        snapshot.recorded_duration
    }
}

fn live_count_text(snapshot: &LiveSnapshot) -> String {
    let status = if snapshot.is_waiting {
        "Waiting  "
    } else {
        "Recording"
    };
    live_count_text_with_status(snapshot, status)
}

fn live_count_text_with_status(snapshot: &LiveSnapshot, status: &str) -> String {
    format!(
        "{}/{} {status}",
        snapshot.recorded_segments, snapshot.total_segments
    )
}

fn pad_right(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.chars().count());
    if padding == 0 {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(padding))
    }
}

fn pad_left(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(value.chars().count());
    if padding == 0 {
        value.to_string()
    } else {
        format!("{}{value}", " ".repeat(padding))
    }
}

fn colorize_recording_status(value: &str, is_waiting: bool) -> String {
    if is_waiting {
        paint(ANSI_PROGRESS_COMPLETE, value)
    } else {
        value.to_string()
    }
}

fn progress_percent(
    completed_segments: u64,
    total_segments: Option<u64>,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
) -> f64 {
    if let Some(total) = total_segments
        && total > 0
    {
        if total == 1
            && completed_segments == 0
            && let Some(total_bytes) = total_bytes
            && total_bytes > 0
        {
            return ((downloaded_bytes as f64 / total_bytes as f64) * 100.0).clamp(0.0, 100.0);
        }
        return ((completed_segments as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
    }
    if let Some(total) = total_bytes
        && total > 0
    {
        return ((downloaded_bytes as f64 / total as f64) * 100.0).clamp(0.0, 100.0);
    }
    0.0
}

fn progress_bar(percent: f64) -> String {
    progress_bar_with_color(percent, false)
}

fn progress_bar_with_color(percent: f64, ansi_color: bool) -> String {
    let percent = percent.clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * BAR_WIDTH as f64) as usize;
    let filled = filled.min(BAR_WIDTH);
    let empty = BAR_WIDTH.saturating_sub(filled);
    if ansi_color {
        let filled_bar = PROGRESS_ASCII_BAR.repeat(filled);
        let empty_bar = PROGRESS_ASCII_BAR.repeat(empty);
        if percent >= 100.0 {
            return paint(
                ANSI_PROGRESS_FINISHED,
                &PROGRESS_ASCII_BAR.repeat(BAR_WIDTH),
            );
        }
        format!(
            "{}{}",
            paint(ANSI_PROGRESS_COMPLETE, &filled_bar),
            paint(ANSI_PROGRESS_REMAINING, &empty_bar)
        )
    } else {
        PROGRESS_ASCII_BAR.repeat(BAR_WIDTH)
    }
}

fn colorize_finished_value(value: &str, percent: f64) -> String {
    if percent >= 100.0 {
        paint(ANSI_GREEN, value)
    } else {
        value.to_string()
    }
}

fn segment_status(completed_segments: u64, total_segments: Option<u64>) -> String {
    total_segments
        .map(|total| format!("{completed_segments}/{total}"))
        .unwrap_or_else(|| completed_segments.to_string())
}

fn byte_status(started: bool, downloaded_bytes: u64, total_bytes: Option<u64>) -> String {
    if !started || downloaded_bytes == 0 {
        return "-".to_string();
    }
    match total_bytes {
        Some(total) => format!(
            "{}/{}",
            format_file_size_display(downloaded_bytes),
            format_file_size_display(total)
        ),
        None => format_file_size_display(downloaded_bytes),
    }
}

fn speed_status(started: bool, bytes_per_second: u64, low_speed_count: u32) -> String {
    if !started || bytes_per_second == 0 {
        return "-".to_string();
    }
    let suffix = if low_speed_count > 0 {
        format!("({low_speed_count})")
    } else {
        String::new()
    };
    format!("{}ps{suffix}", format_file_size_display(bytes_per_second))
}

fn live_speed_status(bytes_per_second: u64, is_waiting: bool) -> String {
    if is_waiting {
        return "-".to_string();
    }
    format!("{}ps", format_file_size_display(bytes_per_second))
}

fn format_duration_short(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}h{minutes:02}m{seconds:02}s")
    } else {
        format!("{minutes:02}m{seconds:02}s")
    }
}

fn remaining_status(
    started: bool,
    downloaded_bytes: u64,
    total_bytes: Option<u64>,
    bytes_per_second: u64,
) -> String {
    if !started {
        return "--:--:--".to_string();
    }
    let Some(total) = total_bytes else {
        return "--:--:--".to_string();
    };
    if total <= downloaded_bytes || bytes_per_second == 0 {
        return "00:00:00".to_string();
    }
    let remaining = total.saturating_sub(downloaded_bytes) / bytes_per_second;
    format_duration(Duration::from_secs(remaining))
}

fn format_file_size_display(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_index = 0_usize;
    while value >= 1024.0 && unit_index + 1 < UNITS.len() {
        value /= 1024.0;
        unit_index += 1;
    }
    format!("{value:.2}{}", UNITS[unit_index])
}

fn trimmed_label(label: &str, width: usize) -> String {
    let mut trimmed = String::new();
    for ch in label.chars().take(width) {
        trimmed.push(ch);
    }
    trimmed
}

fn progress_label(label: &str) -> String {
    trimmed_label(label, PROGRESS_LABEL_WIDTH)
}
