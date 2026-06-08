//! CLI option parsing.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::api::{DownloadClient, DownloadRequest, ProgressCallback};
use crate::cancellation::CancellationToken;
use crate::config::{
    CustomKey, CustomRange, DecryptionEngine, DownloadOptions, HlsMethod, LogLevel,
    MuxAfterDoneOptions, MuxerKind, StreamFilter, SubtitleFormat, TaskStartAt, UiLanguage,
};
use crate::console::ConsoleRenderer;
use crate::error::{Error, Result};
use crate::event::ProgressEvent;
use crate::manifest::{RoleType, StreamSelector};
use crate::mux::{MuxFormat, MuxImport};

static CTRL_C_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);
static CTRL_C_STATE: OnceLock<Mutex<Option<ConsoleSignalState>>> = OnceLock::new();

#[derive(Clone)]
struct ConsoleSignalState {
    cancellation_token: CancellationToken,
    renderer: Arc<Mutex<ConsoleRenderer>>,
}

/// Value shape accepted by a CLI option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliSchemaValueKind {
    /// Positional argument.
    Positional,
    /// Boolean flag with optional `true` or `false` value.
    Flag,
    /// Single scalar value.
    Scalar,
    /// Option may be supplied more than once or with adjacent values.
    Repeatable,
    /// Structured value parsed from `key=value` pairs or a compact grammar.
    Complex,
}

/// How a CLI option is represented in the public Rust API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliApiBinding {
    /// Exposed as a field on [`DownloadRequest`].
    RequestField,
    /// Exposed as a field on [`DownloadOptions`].
    OptionField,
    /// CLI-only behavior with no typed request option.
    CliOnly,
}

/// Product-facing schema for one CLI option.
///
/// This schema documents the public command-line surface and the matching Rust
/// API member. It describes only haki-dl's own CLI/API shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliOptionSchema {
    /// Canonical snake_case name used by the Rust API.
    pub canonical: &'static str,
    /// Accepted CLI flags or aliases. Positional arguments have no aliases.
    pub aliases: &'static [&'static str],
    /// Accepted value shape.
    pub value_kind: CliSchemaValueKind,
    /// Documented default value.
    pub default: &'static str,
    /// Typed Rust API member that represents this option.
    pub api_member: &'static str,
    /// Binding category for the API member.
    pub api_binding: CliApiBinding,
    /// Whether this row is shown in generated CLI help text.
    pub show_in_help: bool,
}

/// Public CLI schema for documentation and programmatic inspection.
pub const CLI_SCHEMA: &[CliOptionSchema] = &[
    schema_row(
        "input",
        &[],
        CliSchemaValueKind::Positional,
        "required",
        "DownloadRequest::input",
        CliApiBinding::RequestField,
    ),
    schema_row(
        "tmp_dir",
        &["--tmp-dir"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::tmp_dir",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "save_dir",
        &["--save-dir"],
        CliSchemaValueKind::Scalar,
        "current working directory",
        "DownloadOptions::save_dir",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "save_name",
        &["--save-name"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::save_name",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "save_pattern",
        &["--save-pattern"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::save_pattern",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "log_file_path",
        &["--log-file-path"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::log_file_path",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "ui_language",
        &["--ui-language"],
        CliSchemaValueKind::Scalar,
        "auto",
        "DownloadOptions::ui_language",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "urlprocessor_args",
        &["--urlprocessor-args"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::urlprocessor_args",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "key_text_file",
        &["--key-text-file"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::key_text_file",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "key",
        &["--key"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::keys",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "decryption_engine",
        &["--decryption-engine"],
        CliSchemaValueKind::Scalar,
        "mp4forge",
        "DownloadOptions::decryption_engine",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "decryption_binary_path",
        &["--decryption-binary-path"],
        CliSchemaValueKind::Scalar,
        "auto",
        "DownloadOptions::decryption_binary_path",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "mp4_real_time_decryption",
        &["--mp4-real-time-decryption"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::mp4_real_time_decryption",
        CliApiBinding::OptionField,
    ),
    hidden_schema_row(
        "use_shaka_packager",
        &["--use-shaka-packager"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::use_shaka_packager",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "header",
        &["-H", "--header"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::headers",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "base_url",
        &["--base-url"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::base_url",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "append_url_params",
        &["--append-url-params"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::append_url_params",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "use_system_proxy",
        &["--use-system-proxy"],
        CliSchemaValueKind::Flag,
        "true",
        "DownloadOptions::use_system_proxy",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "custom_proxy",
        &["--custom-proxy"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::custom_proxy",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "http_request_timeout",
        &["--http-request-timeout"],
        CliSchemaValueKind::Scalar,
        "100",
        "DownloadOptions::http_request_timeout",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "log_level",
        &["--log-level"],
        CliSchemaValueKind::Scalar,
        "INFO",
        "DownloadOptions::log_level",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "no_log",
        &["--no-log"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::no_log",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "force_ansi_console",
        &["--force-ansi-console"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::force_ansi_console",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "no_ansi_color",
        &["--no-ansi-color"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::no_ansi_color",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "disable_update_check",
        &["--disable-update-check"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::disable_update_check",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "auto_select",
        &["--auto-select"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::auto_select",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "sub_only",
        &["--sub-only"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::sub_only",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "select_video",
        &["-sv", "--select-video"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::select_video",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "select_audio",
        &["-sa", "--select-audio"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::select_audio",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "select_subtitle",
        &["-ss", "--select-subtitle"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::select_subtitle",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "drop_video",
        &["-dv", "--drop-video"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::drop_video",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "drop_audio",
        &["-da", "--drop-audio"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::drop_audio",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "drop_subtitle",
        &["-ds", "--drop-subtitle"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::drop_subtitle",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "custom_range",
        &["--custom-range"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::custom_range",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "ad_keyword",
        &["--ad-keyword"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::ad_keywords",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "thread_count",
        &["--thread-count"],
        CliSchemaValueKind::Scalar,
        "logical processor count",
        "DownloadOptions::thread_count",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "download_retry_count",
        &["--download-retry-count"],
        CliSchemaValueKind::Scalar,
        "3",
        "DownloadOptions::download_retry_count",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "max_speed",
        &["-R", "--max-speed"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::max_speed",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "concurrent_download",
        &["-mt", "--concurrent-download"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::concurrent_download",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "skip_download",
        &["--skip-download"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::skip_download",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "skip_merge",
        &["--skip-merge"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::skip_merge",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "binary_merge",
        &["--binary-merge"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::binary_merge",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "use_ffmpeg_concat_demuxer",
        &["--use-ffmpeg-concat-demuxer"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::use_ffmpeg_concat_demuxer",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "del_after_done",
        &["--del-after-done"],
        CliSchemaValueKind::Flag,
        "true",
        "DownloadOptions::del_after_done",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "check_segments_count",
        &["--check-segments-count"],
        CliSchemaValueKind::Flag,
        "true",
        "DownloadOptions::check_segments_count",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "write_meta_json",
        &["--write-meta-json"],
        CliSchemaValueKind::Flag,
        "true",
        "DownloadOptions::write_meta_json",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "no_date_info",
        &["--no-date-info"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::no_date_info",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "auto_subtitle_fix",
        &["--auto-subtitle-fix"],
        CliSchemaValueKind::Flag,
        "true",
        "DownloadOptions::auto_subtitle_fix",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "sub_format",
        &["--sub-format"],
        CliSchemaValueKind::Scalar,
        "SRT",
        "DownloadOptions::sub_format",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "ffmpeg_binary_path",
        &["--ffmpeg-binary-path"],
        CliSchemaValueKind::Scalar,
        "auto",
        "DownloadOptions::ffmpeg_binary_path",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "mux_after_done",
        &["-M", "--mux-after-done"],
        CliSchemaValueKind::Complex,
        "none",
        "DownloadOptions::mux_after_done",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "mux_import",
        &["--mux-import"],
        CliSchemaValueKind::Repeatable,
        "none",
        "DownloadOptions::mux_imports",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "custom_hls_method",
        &["--custom-hls-method"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::custom_hls_method",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "custom_hls_key",
        &["--custom-hls-key"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::custom_hls_key",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "custom_hls_iv",
        &["--custom-hls-iv"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::custom_hls_iv",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "allow_hls_multi_ext_map",
        &["--allow-hls-multi-ext-map"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::allow_hls_multi_ext_map",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "task_start_at",
        &["--task-start-at"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::task_start_at",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_perform_as_vod",
        &["--live-perform-as-vod"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::live_perform_as_vod",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_real_time_merge",
        &["--live-real-time-merge"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::live_real_time_merge",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_keep_segments",
        &["--live-keep-segments"],
        CliSchemaValueKind::Flag,
        "true",
        "DownloadOptions::live_keep_segments",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_pipe_mux",
        &["--live-pipe-mux"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::live_pipe_mux",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_fix_vtt_by_audio",
        &["--live-fix-vtt-by-audio"],
        CliSchemaValueKind::Flag,
        "false",
        "DownloadOptions::live_fix_vtt_by_audio",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_record_limit",
        &["--live-record-limit"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::live_record_limit",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_wait_time",
        &["--live-wait-time"],
        CliSchemaValueKind::Scalar,
        "none",
        "DownloadOptions::live_wait_time",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "live_take_count",
        &["--live-take-count"],
        CliSchemaValueKind::Scalar,
        "16",
        "DownloadOptions::live_take_count",
        CliApiBinding::OptionField,
    ),
    schema_row(
        "help",
        &["-?", "-h", "--help"],
        CliSchemaValueKind::Flag,
        "false",
        "CliParseResult::Help",
        CliApiBinding::CliOnly,
    ),
    schema_row(
        "version",
        &["--version"],
        CliSchemaValueKind::Flag,
        "false",
        "CliParseResult::Version",
        CliApiBinding::CliOnly,
    ),
    schema_row(
        "morehelp",
        &["--morehelp"],
        CliSchemaValueKind::Scalar,
        "none",
        "CliParseResult::MoreHelp",
        CliApiBinding::CliOnly,
    ),
];

const fn schema_row(
    canonical: &'static str,
    aliases: &'static [&'static str],
    value_kind: CliSchemaValueKind,
    default: &'static str,
    api_member: &'static str,
    api_binding: CliApiBinding,
) -> CliOptionSchema {
    CliOptionSchema {
        canonical,
        aliases,
        value_kind,
        default,
        api_member,
        api_binding,
        show_in_help: true,
    }
}

const fn hidden_schema_row(
    canonical: &'static str,
    aliases: &'static [&'static str],
    value_kind: CliSchemaValueKind,
    default: &'static str,
    api_member: &'static str,
    api_binding: CliApiBinding,
) -> CliOptionSchema {
    CliOptionSchema {
        canonical,
        aliases,
        value_kind,
        default,
        api_member,
        api_binding,
        show_in_help: false,
    }
}

/// Parsed CLI result.
#[derive(Clone, Debug)]
pub enum CliParseResult {
    Request(Box<DownloadRequest>),
    Help { text: String },
    MoreHelp { topic: Option<String>, text: String },
    Version { text: String },
}

/// Parses CLI arguments after the executable name.
pub async fn parse_args<I, S>(args: I) -> Result<CliParseResult>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Parser::new(args.into_iter().map(Into::into).collect())
        .parse()
        .await
}

/// Runs the current CLI shell.
pub async fn run_cli<I, S>(args: I) -> ExitCode
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let tokens = args.into_iter().map(Into::into).collect::<Vec<_>>();
    let detected_language = pre_scan_ui_language(&tokens);
    match parse_args(tokens).await {
        Ok(CliParseResult::Request(request)) => {
            let mut request = *request;
            let language = request.options.ui_language.or(detected_language);
            let stdout_redirected = !std::io::stdout().is_terminal();
            let stderr_redirected = !std::io::stderr().is_terminal();
            apply_terminal_defaults(&mut request.options, stdout_redirected, stderr_redirected);
            let renderer = Arc::new(Mutex::new(ConsoleRenderer::new(&request.options)));
            install_console_cancel_handler(&request.cancellation_token, Arc::clone(&renderer));
            if stdout_redirected || stderr_redirected {
                render_cli_notice(
                    &renderer,
                    LogLevel::Info,
                    "Output is redirected, ANSI colors are cleared.",
                );
            }
            attach_console_progress_callback(&mut request, Arc::clone(&renderer));
            let log_level = request.options.log_level;
            match DownloadClient::new().prepare(request) {
                Ok(session) => match Box::pin(session.start()).await {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(error) => {
                        render_cli_error(&renderer, &error, language, log_level);
                        ExitCode::from(2)
                    }
                },
                Err(error) => {
                    render_cli_error(&renderer, &error, language, log_level);
                    ExitCode::from(2)
                }
            }
        }
        Ok(CliParseResult::MoreHelp { text, .. }) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(CliParseResult::Help { text }) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(CliParseResult::Version { text }) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            render_standalone_cli_error(&error, detected_language);
            ExitCode::from(2)
        }
    }
}

fn install_console_cancel_handler(
    cancellation_token: &CancellationToken,
    renderer: Arc<Mutex<ConsoleRenderer>>,
) {
    let state_lock = CTRL_C_STATE.get_or_init(|| Mutex::new(None));
    if let Ok(mut state) = state_lock.lock() {
        *state = Some(ConsoleSignalState {
            cancellation_token: cancellation_token.clone(),
            renderer,
        });
    }
    if CTRL_C_HANDLER_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    if ctrlc::set_handler(handle_console_cancel).is_err() {
        CTRL_C_HANDLER_INSTALLED.store(false, Ordering::SeqCst);
    }
}

fn handle_console_cancel() {
    if let Some(state_lock) = CTRL_C_STATE.get()
        && let Ok(state) = state_lock.lock()
        && let Some(state) = state.as_ref()
    {
        state.cancellation_token.cancel();
        if let Ok(mut renderer) = state.renderer.lock() {
            let _ = renderer.render_stdout(&ProgressEvent::Warning {
                message: "Force Exit...".to_string(),
            });
        }
    }
    restore_console_cursor();
    std::process::exit(0);
}

fn attach_console_progress_callback(
    request: &mut DownloadRequest,
    renderer: Arc<Mutex<ConsoleRenderer>>,
) {
    let existing_callback = request.progress_callback.take();
    request.progress_callback = Some(ProgressCallback::new(move |event: &ProgressEvent| {
        if let Some(callback) = &existing_callback {
            callback.emit(event)?;
        }
        renderer
            .lock()
            .map_err(|_| Error::config("console renderer state is unavailable"))?
            .render_stdout(event)
    }));
}

fn render_cli_error(
    renderer: &Arc<Mutex<ConsoleRenderer>>,
    error: &Error,
    language: Option<UiLanguage>,
    log_level: LogLevel,
) {
    let error_text = cli_error_text_for_log_level(error, language, log_level);
    match renderer.lock() {
        Ok(mut renderer) => {
            if renderer.render_error_stdout(&error_text).is_err() {
                eprintln!("{error_text}");
            }
        }
        Err(_) => eprintln!("{error_text}"),
    }
}

fn restore_console_cursor() {
    print!("\x1b[?25h");
    let _ = std::io::stdout().flush();
}

fn render_cli_notice(renderer: &Arc<Mutex<ConsoleRenderer>>, level: LogLevel, message: &str) {
    let event = ProgressEvent::Log {
        level,
        message: message.to_string(),
    };
    if let Ok(mut renderer) = renderer.lock() {
        let _ = renderer.render_stdout(&event);
    }
}

fn render_standalone_cli_error(error: &Error, language: Option<UiLanguage>) {
    let error_text = cli_error_text(error, language);
    let mut renderer = ConsoleRenderer::with_log_level(LogLevel::Info);
    if renderer.render_error_stdout(&error_text).is_err() {
        eprintln!("{error_text}");
    }
}

fn apply_terminal_defaults(
    options: &mut DownloadOptions,
    stdout_redirected: bool,
    stderr_redirected: bool,
) {
    if stdout_redirected || stderr_redirected {
        options.force_ansi_console = true;
        options.no_ansi_color = true;
    }
}

struct Parser {
    tokens: Vec<String>,
    index: usize,
    input: Option<String>,
    options: DownloadOptions,
}

impl Parser {
    fn new(tokens: Vec<String>) -> Self {
        let ui_language = pre_scan_ui_language(&tokens);
        let options = DownloadOptions {
            ui_language,
            ..DownloadOptions::default()
        };
        Self {
            tokens,
            index: 0,
            input: None,
            options,
        }
    }

    async fn parse(mut self) -> Result<CliParseResult> {
        while let Some(raw_token) = self.take_token() {
            let (token, inline_value) = split_inline_option(raw_token);
            let option_name = token.clone();
            match option_name.as_str() {
                "-?" | "-h" | "--help" => {
                    if inline_value.is_some() {
                        return Err(Error::config("--help does not accept a value"));
                    }
                    return Ok(CliParseResult::Help { text: help_text() });
                }
                "--version" => {
                    if inline_value.is_some() {
                        return Err(Error::config("--version does not accept a value"));
                    }
                    return Ok(CliParseResult::Version {
                        text: version_text(),
                    });
                }
                "--morehelp" => {
                    if inline_value.is_some() {
                        return Err(Error::config("--morehelp requires a separate option name"));
                    }
                    let topic = self
                        .take_token()
                        .ok_or_else(|| Error::config("--morehelp requires a value"))?;
                    return Ok(CliParseResult::MoreHelp {
                        text: morehelp_text(&topic, self.options.ui_language),
                        topic: Some(topic),
                    });
                }
                "--tmp-dir" => {
                    self.options.tmp_dir = Some(PathBuf::from(self.value(token, inline_value)?))
                }
                "--save-dir" => {
                    self.options.save_dir = Some(PathBuf::from(self.value(token, inline_value)?))
                }
                "--save-name" => {
                    self.options.save_name = Some(sanitize_file_name(
                        self.value(token, inline_value)?,
                        "--save-name",
                    )?)
                }
                "--save-pattern" => {
                    self.options.save_pattern = Some(self.value(token, inline_value)?)
                }
                "--log-file-path" => {
                    self.options.log_file_path = Some(sanitize_path_file_name(
                        self.value(token, inline_value)?,
                        "--log-file-path",
                    )?);
                }
                "--ui-language" => {
                    self.options.ui_language =
                        Some(parse_ui_language(&self.value(token, inline_value)?)?)
                }
                "--urlprocessor-args" => {
                    self.options.urlprocessor_args = Some(self.value(token, inline_value)?)
                }
                "--key-text-file" => {
                    self.options.key_text_file =
                        Some(PathBuf::from(self.value(token, inline_value)?))
                }
                "--key" => {
                    for value in self.values_one_or_more(token, inline_value)? {
                        self.options.keys.push(parse_custom_key(&value)?);
                    }
                }
                "--decryption-engine" => {
                    self.options.decryption_engine =
                        parse_decryption_engine(&self.value(token, inline_value)?)?
                }
                "--decryption-binary-path" => {
                    self.options.decryption_binary_path =
                        Some(PathBuf::from(self.value(token, inline_value)?))
                }
                "--mp4-real-time-decryption" => {
                    self.options.mp4_real_time_decryption = self.bool_value(inline_value)?
                }
                "--use-shaka-packager" => {
                    self.options.use_shaka_packager = self.bool_value(inline_value)?;
                }
                "-H" | "--header" => {
                    for value in self.values_one_or_more(token, inline_value)? {
                        if let Some((name, value)) = parse_header(&value) {
                            self.options.headers.insert(name, value);
                        }
                    }
                }
                "--base-url" => self.options.base_url = Some(self.value(token, inline_value)?),
                "--append-url-params" => {
                    self.options.append_url_params = self.bool_value(inline_value)?
                }
                "--use-system-proxy" => {
                    self.options.use_system_proxy = self.bool_value(inline_value)?
                }
                "--custom-proxy" => {
                    self.options.custom_proxy =
                        parse_custom_proxy(&self.value(token, inline_value)?)?
                }
                "--http-request-timeout" => {
                    self.options.http_request_timeout = parse_seconds_duration(
                        &self.value(token, inline_value)?,
                        "--http-request-timeout",
                    )?;
                }
                "--log-level" => {
                    self.options.log_level = parse_log_level(&self.value(token, inline_value)?)?
                }
                "--no-log" => self.options.no_log = self.bool_value(inline_value)?,
                "--force-ansi-console" => {
                    self.options.force_ansi_console = self.bool_value(inline_value)?
                }
                "--no-ansi-color" => self.options.no_ansi_color = self.bool_value(inline_value)?,
                "--disable-update-check" => {
                    self.options.disable_update_check = self.bool_value(inline_value)?
                }
                "--auto-select" => self.options.auto_select = self.bool_value(inline_value)?,
                "--sub-only" => self.options.sub_only = self.bool_value(inline_value)?,
                "-sv" | "--select-video" => {
                    ensure_single_filter(&self.options.select_video, &token)?;
                    let filter = parse_stream_filter(&self.value(token, inline_value)?)?;
                    self.options.select_video.push(filter);
                }
                "-sa" | "--select-audio" => {
                    ensure_single_filter(&self.options.select_audio, &token)?;
                    let filter = parse_stream_filter(&self.value(token, inline_value)?)?;
                    self.options.select_audio.push(filter);
                }
                "-ss" | "--select-subtitle" => {
                    ensure_single_filter(&self.options.select_subtitle, &token)?;
                    let filter = parse_stream_filter(&self.value(token, inline_value)?)?;
                    self.options.select_subtitle.push(filter);
                }
                "-dv" | "--drop-video" => {
                    ensure_single_filter(&self.options.drop_video, &token)?;
                    let filter = parse_stream_filter(&self.value(token, inline_value)?)?;
                    self.options.drop_video.push(filter);
                }
                "-da" | "--drop-audio" => {
                    ensure_single_filter(&self.options.drop_audio, &token)?;
                    let filter = parse_stream_filter(&self.value(token, inline_value)?)?;
                    self.options.drop_audio.push(filter);
                }
                "-ds" | "--drop-subtitle" => {
                    ensure_single_filter(&self.options.drop_subtitle, &token)?;
                    let filter = parse_stream_filter(&self.value(token, inline_value)?)?;
                    self.options.drop_subtitle.push(filter);
                }
                "--custom-range" => {
                    self.options.custom_range =
                        parse_custom_range(&self.value(token, inline_value)?)?
                }
                "--ad-keyword" => {
                    let values = self.values_one_or_more(token, inline_value)?;
                    self.options.ad_keywords.extend(values);
                }
                "--thread-count" => {
                    self.options.thread_count =
                        parse_i32(&self.value(token, inline_value)?, "--thread-count")?
                }
                "--download-retry-count" => {
                    self.options.download_retry_count =
                        parse_i32(&self.value(token, inline_value)?, "--download-retry-count")?
                }
                "-R" | "--max-speed" => {
                    self.options.max_speed =
                        Some(parse_speed_limit(&self.value(token, inline_value)?)?)
                }
                "-mt" | "--concurrent-download" => {
                    self.options.concurrent_download = self.bool_value(inline_value)?
                }
                "--skip-download" => self.options.skip_download = self.bool_value(inline_value)?,
                "--skip-merge" => self.options.skip_merge = self.bool_value(inline_value)?,
                "--binary-merge" => self.options.binary_merge = self.bool_value(inline_value)?,
                "--use-ffmpeg-concat-demuxer" => {
                    self.options.use_ffmpeg_concat_demuxer = self.bool_value(inline_value)?
                }
                "--del-after-done" => {
                    self.options.del_after_done = self.bool_value(inline_value)?
                }
                "--check-segments-count" => {
                    self.options.check_segments_count = self.bool_value(inline_value)?
                }
                "--write-meta-json" => {
                    self.options.write_meta_json = self.bool_value(inline_value)?
                }
                "--no-date-info" => self.options.no_date_info = self.bool_value(inline_value)?,
                "--auto-subtitle-fix" => {
                    self.options.auto_subtitle_fix = self.bool_value(inline_value)?
                }
                "--sub-format" => {
                    self.options.sub_format =
                        parse_subtitle_format(&self.value(token, inline_value)?)?
                }
                "--ffmpeg-binary-path" => {
                    self.options.ffmpeg_binary_path =
                        Some(PathBuf::from(self.value(token, inline_value)?))
                }
                "-M" | "--mux-after-done" => {
                    self.options.mux_after_done =
                        Some(parse_mux_after_done(&self.value(token, inline_value)?)?)
                }
                "--mux-import" => {
                    for value in self.values_one_or_more(token, inline_value)? {
                        self.options
                            .mux_imports
                            .push(parse_mux_import(&value).await?);
                    }
                }
                "--custom-hls-method" => {
                    self.options.custom_hls_method =
                        Some(parse_hls_method(&self.value(token, inline_value)?)?)
                }
                "--custom-hls-key" => {
                    self.options.custom_hls_key =
                        parse_hls_bytes(&self.value(token, inline_value)?, "--custom-hls-key")
                            .await?
                }
                "--custom-hls-iv" => {
                    self.options.custom_hls_iv =
                        parse_hls_bytes(&self.value(token, inline_value)?, "--custom-hls-iv")
                            .await?
                }
                "--allow-hls-multi-ext-map" => {
                    self.options.allow_hls_multi_ext_map = self.bool_value(inline_value)?
                }
                "--task-start-at" => {
                    self.options.task_start_at =
                        Some(parse_task_start_at(&self.value(token, inline_value)?)?)
                }
                "--live-perform-as-vod" => {
                    self.options.live_perform_as_vod = self.bool_value(inline_value)?
                }
                "--live-real-time-merge" => {
                    self.options.live_real_time_merge = self.bool_value(inline_value)?
                }
                "--live-keep-segments" => {
                    self.options.live_keep_segments = self.bool_value(inline_value)?
                }
                "--live-pipe-mux" => self.options.live_pipe_mux = self.bool_value(inline_value)?,
                "--live-fix-vtt-by-audio" => {
                    self.options.live_fix_vtt_by_audio = self.bool_value(inline_value)?
                }
                "--live-record-limit" => {
                    self.options.live_record_limit = Some(parse_duration(
                        &self.value(token, inline_value)?,
                        "--live-record-limit",
                    )?)
                }
                "--live-wait-time" => {
                    self.options.live_wait_time = Some(parse_i32(
                        &self.value(token, inline_value)?,
                        "--live-wait-time",
                    )?)
                }
                "--live-take-count" => {
                    self.options.live_take_count =
                        parse_i32(&self.value(token, inline_value)?, "--live-take-count")?
                }
                value if value.starts_with('-') => {
                    return Err(Error::config(format!("unknown option {value}")));
                }
                value => self.set_input(value.to_string(), inline_value)?,
            }
        }

        let input = match self.input {
            Some(value) => value,
            None => return Err(Error::config("input is required")),
        };
        let mut options = self.options;
        if !options.mux_imports.is_empty() && options.mux_after_done.is_none() {
            return Err(Error::config("--mux-import requires --mux-after-done"));
        }
        if options.use_shaka_packager {
            options.decryption_engine = DecryptionEngine::ShakaPackager;
        }
        if options.live_pipe_mux {
            options.live_real_time_merge = true;
        }
        if options.mux_after_done.is_some() {
            options.binary_merge = true;
        }

        let stream_selector = if options.sub_only {
            StreamSelector::SubtitlesOnly
        } else if options.auto_select {
            StreamSelector::Auto
        } else {
            StreamSelector::default()
        };
        Ok(CliParseResult::Request(Box::new(
            DownloadRequest::new(input)
                .with_options(options)
                .with_stream_selector(stream_selector),
        )))
    }

    fn take_token(&mut self) -> Option<String> {
        let value = self.tokens.get(self.index).cloned();
        if value.is_some() {
            self.index += 1;
        }
        value
    }

    fn peek_token(&self) -> Option<&str> {
        self.tokens.get(self.index).map(String::as_str)
    }

    fn value(&mut self, option: String, inline_value: Option<String>) -> Result<String> {
        if let Some(value) = inline_value {
            return Ok(value);
        }
        match self.take_token() {
            Some(value) => Ok(value),
            None => Err(Error::config(format!("{option} requires a value"))),
        }
    }

    fn values_one_or_more(
        &mut self,
        option: String,
        inline_value: Option<String>,
    ) -> Result<Vec<String>> {
        let mut values = vec![self.value(option, inline_value)?];
        while let Some(value) = self.peek_token() {
            if is_option_boundary(value) {
                break;
            }
            if let Some(value) = self.take_token() {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn bool_value(&mut self, inline_value: Option<String>) -> Result<bool> {
        if let Some(value) = inline_value {
            return parse_bool(&value);
        }
        match self.peek_token() {
            Some(value) if is_bool_text(value) => {
                let value = self
                    .take_token()
                    .ok_or_else(|| Error::config("boolean option value disappeared"))?;
                parse_bool(&value)
            }
            _ => Ok(true),
        }
    }

    fn set_input(&mut self, value: String, inline_value: Option<String>) -> Result<()> {
        if inline_value.is_some() {
            return Err(Error::config(format!(
                "unexpected inline value on input {value}"
            )));
        }
        if self.input.is_some() {
            return Err(Error::config(format!(
                "unexpected positional argument {value}"
            )));
        }
        self.input = Some(value);
        Ok(())
    }
}

fn split_inline_option(token: String) -> (String, Option<String>) {
    if token.starts_with("--")
        && let Some(index) = token.find('=')
    {
        let option = token.get(..index).map(str::to_string);
        let value = token.get(index + 1..).map(str::to_string);
        if let (Some(option), Some(value)) = (option, value) {
            return (option, Some(value));
        }
    }
    (token, None)
}

fn is_option_boundary(value: &str) -> bool {
    value.starts_with('-') && value != "-"
}

fn ensure_single_filter(existing: &[StreamFilter], option: &str) -> Result<()> {
    if existing.is_empty() {
        Ok(())
    } else {
        Err(Error::config(format!("{option} expects a single value")))
    }
}

fn is_bool_text(value: &str) -> bool {
    value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false")
}

fn parse_bool(value: &str) -> Result<bool> {
    if value.eq_ignore_ascii_case("true") {
        Ok(true)
    } else if value.eq_ignore_ascii_case("false") {
        Ok(false)
    } else {
        Err(Error::config(format!("invalid boolean value {value}")))
    }
}

fn parse_i32(value: &str, option: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| Error::config(format!("{option} must be an integer")))
}

fn parse_i64(value: &str, option: &str) -> Result<i64> {
    value
        .parse::<i64>()
        .map_err(|_| Error::config(format!("{option} must be an integer")))
}

fn parse_f64(value: &str, option: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .map_err(|_| Error::config(format!("{option} must be numeric")))
}

fn parse_log_level(value: &str) -> Result<LogLevel> {
    match value.to_ascii_lowercase().as_str() {
        "debug" => Ok(LogLevel::Debug),
        "info" => Ok(LogLevel::Info),
        "warn" => Ok(LogLevel::Warn),
        "error" => Ok(LogLevel::Error),
        "off" => Ok(LogLevel::Off),
        _ => Err(Error::config(format!("invalid log level {value}"))),
    }
}

fn parse_decryption_engine(value: &str) -> Result<DecryptionEngine> {
    match normalize_token(value).as_str() {
        "mp4forge" => Ok(DecryptionEngine::Mp4forge),
        "mp4decrypt" => Ok(DecryptionEngine::Mp4decrypt),
        "shakapackager" => Ok(DecryptionEngine::ShakaPackager),
        "ffmpeg" => Ok(DecryptionEngine::Ffmpeg),
        _ => Err(Error::config(format!("invalid decryption engine {value}"))),
    }
}

fn parse_subtitle_format(value: &str) -> Result<SubtitleFormat> {
    match value.to_ascii_lowercase().as_str() {
        "srt" => Ok(SubtitleFormat::Srt),
        "vtt" => Ok(SubtitleFormat::Vtt),
        _ => Err(Error::config(format!("invalid subtitle format {value}"))),
    }
}

fn parse_ui_language(value: &str) -> Result<UiLanguage> {
    match value {
        "auto" => Ok(UiLanguage::EnUs),
        "en-US" => Ok(UiLanguage::EnUs),
        _ => Err(Error::config(format!("invalid UI language {value}"))),
    }
}

fn parse_hls_method(value: &str) -> Result<HlsMethod> {
    let method = match value.to_ascii_uppercase().as_str() {
        "NONE" => HlsMethod::None,
        "AES_128" => HlsMethod::Aes128,
        "AES_128_ECB" => HlsMethod::Aes128Ecb,
        "CENC" => HlsMethod::Cenc,
        "SAMPLE_AES" => HlsMethod::SampleAes,
        "SAMPLE_AES_CTR" => HlsMethod::SampleAesCtr,
        "CHACHA20" => HlsMethod::Chacha20,
        "UNKNOWN" => HlsMethod::Unknown,
        _ if value.trim().is_empty() => {
            return Err(Error::config("--custom-hls-method must not be empty"));
        }
        _ => return Err(Error::config(format!("invalid HLS method {value}"))),
    };
    Ok(method)
}

fn normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && *ch != ' ')
        .flat_map(char::to_lowercase)
        .collect()
}

fn parse_header(value: &str) -> Option<(String, String)> {
    let index = value.find(':')?;
    let name = value.get(..index)?.trim().to_ascii_lowercase();
    let header_value = value.get(index + 1..)?.trim().to_string();
    Some((name, header_value))
}

fn parse_custom_proxy(value: &str) -> Result<Option<String>> {
    if value.is_empty() {
        return Ok(None);
    }
    let _ = reqwest::Url::parse(value)
        .map_err(|error| Error::config(format!("--custom-proxy URI is invalid: {error}")))?;
    Ok(Some(value.to_string()))
}

fn sanitize_file_name(value: String, option: &str) -> Result<String> {
    let mut cleaned = String::with_capacity(value.len());
    for ch in value.chars() {
        if is_invalid_file_name_char(ch) {
            cleaned.push('_');
        } else {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.trim().is_empty() {
        return Err(Error::config(format!(
            "{option} produced an empty file name"
        )));
    }
    Ok(cleaned)
}

fn sanitize_path_file_name(value: String, option: &str) -> Result<PathBuf> {
    let path = std::path::absolute(Path::new(&value))
        .map_err(|_| Error::config(format!("{option} path is invalid")))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::config(format!("{option} must include a file name")))?;
    let cleaned = sanitize_file_name(file_name.to_string(), option)?;
    let result = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(cleaned),
        _ => PathBuf::from(cleaned),
    };
    Ok(result)
}

fn is_invalid_file_name_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{0}'..='\u{1f}' | '"' | '<' | '>' | '|' | ':' | '*' | '?' | '\\' | '/'
    )
}

fn parse_speed_limit(value: &str) -> Result<u64> {
    let (number, unit) = find_speed_limit_match(value)
        .ok_or_else(|| Error::config("--max-speed must contain a number ending with M or K"))?;
    let multiplier = match unit {
        'M' => 1024.0 * 1024.0,
        'K' => 1024.0,
        _ => return Err(Error::config("--max-speed unit is invalid")),
    };
    if number.is_empty() {
        return Err(Error::config("--max-speed requires a numeric value"));
    }
    let parsed = parse_f64(number, "--max-speed")?;
    if parsed < 0.0 {
        return Err(Error::config("--max-speed must not be negative"));
    }
    Ok((parsed * multiplier) as u64)
}

fn find_speed_limit_match(value: &str) -> Option<(&str, char)> {
    let mut start = None;
    for (index, ch) in value.char_indices() {
        if ch.is_ascii_digit() || ch == '.' {
            if start.is_none() {
                start = Some(index);
            }
            continue;
        }
        if let Some(number_start) = start {
            let unit = ch.to_ascii_uppercase();
            if unit == 'M' || unit == 'K' {
                return value.get(number_start..index).map(|number| (number, unit));
            }
            start = None;
        }
    }
    None
}

fn parse_custom_range(value: &str) -> Result<Option<CustomRange>> {
    if value.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 2 {
        return Err(Error::config("--custom-range must use start-end syntax"));
    }
    let start = parts[0].trim();
    let end = parts[1].trim();
    if value.contains(':') {
        let start_seconds = if start.is_empty() {
            0.0
        } else {
            parse_duration_seconds(start, "--custom-range")?
        };
        let end_seconds = if end.is_empty() {
            f64::MAX
        } else {
            parse_duration_seconds(end, "--custom-range")?
        };
        Ok(Some(CustomRange::Time {
            input: value.to_string(),
            start_seconds,
            end_seconds,
        }))
    } else {
        let (start_index, end_index) = parse_segment_range(value)?;
        Ok(Some(CustomRange::Segment {
            input: value.to_string(),
            start_index,
            end_index,
        }))
    }
}

fn parse_segment_range(value: &str) -> Result<(i64, i64)> {
    let bytes = value.as_bytes();
    let Some(hyphen) = bytes.iter().position(|byte| *byte == b'-') else {
        return Err(Error::config("--custom-range must use start-end syntax"));
    };
    let mut left_start = hyphen;
    while left_start > 0 && bytes[left_start - 1].is_ascii_digit() {
        left_start -= 1;
    }
    let mut right_end = hyphen + 1;
    while right_end < bytes.len() && bytes[right_end].is_ascii_digit() {
        right_end += 1;
    }
    let left = value
        .get(left_start..hyphen)
        .ok_or_else(|| Error::config("--custom-range segment range is invalid"))?;
    let right = value
        .get(hyphen + 1..right_end)
        .ok_or_else(|| Error::config("--custom-range segment range is invalid"))?;
    let start_index = if left.is_empty() {
        0_i64
    } else {
        parse_i64(left, "--custom-range")?
    };
    let end_index = if right.is_empty() {
        i64::MAX
    } else {
        parse_i64(right, "--custom-range")?
    };
    Ok((start_index, end_index))
}

fn parse_duration(value: &str, option: &str) -> Result<Duration> {
    let seconds = parse_duration_seconds(value, option)?;
    if !seconds.is_finite() {
        return Err(Error::config(format!("{option} duration is invalid")));
    }
    Ok(Duration::from_secs_f64(seconds.max(0.0)))
}

fn parse_seconds_duration(value: &str, option: &str) -> Result<Duration> {
    let seconds = parse_f64(value, option)?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(Error::config(format!("{option} duration is invalid")));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_duration_seconds(value: &str, option: &str) -> Result<f64> {
    if value.trim().is_empty() {
        return Err(Error::config(format!(
            "{option} duration must not be empty"
        )));
    }
    let normalized = value.replace('\u{ff1a}', ":");
    let parts = normalized
        .split(':')
        .map(|part| parse_i32(part.trim(), option))
        .collect::<Result<Vec<_>>>()?;
    let mut total = 0.0;
    for (index, parsed) in parts.into_iter().rev().take(4).enumerate() {
        let multiplier = match index {
            0 => 1_i64,
            1 => 60_i64,
            2 => 60_i64 * 60_i64,
            3 => 24_i64 * 60_i64 * 60_i64,
            _ => 1_i64,
        };
        total += i64::from(parsed).saturating_mul(multiplier) as f64;
    }
    Ok(total)
}

fn parse_hms_duration(value: &str, option: &str) -> Result<f64> {
    if value.trim().is_empty() {
        return Err(Error::config(format!(
            "{option} duration must not be empty"
        )));
    }
    let bytes = value.as_bytes();
    let mut index = 0_usize;
    let mut total = 0_i64;
    let mut saw_unit = false;
    let mut last_rank = 0_u8;
    while index < bytes.len() {
        let number_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if number_start == index || index >= bytes.len() {
            return Err(Error::config(format!(
                "{option} duration must use h/m/s units"
            )));
        }
        let unit = bytes[index] as char;
        index += 1;
        let (rank, multiplier) = match unit {
            'h' => (1_u8, 60_i64 * 60_i64),
            'm' => (2_u8, 60_i64),
            's' => (3_u8, 1_i64),
            _ => return Err(Error::config(format!("{option} has invalid duration unit"))),
        };
        if rank <= last_rank {
            return Err(Error::config(format!(
                "{option} duration units must be ordered h/m/s"
            )));
        }
        last_rank = rank;
        let number = value
            .get(number_start..index - 1)
            .ok_or_else(|| Error::config(format!("{option} duration is invalid")))?;
        let parsed = parse_i32(number, option)?;
        total = total.saturating_add(i64::from(parsed).saturating_mul(multiplier));
        saw_unit = true;
    }
    if !saw_unit {
        return Err(Error::config(format!(
            "{option} duration must use h/m/s units"
        )));
    }
    Ok(total as f64)
}

fn parse_task_start_at(value: &str) -> Result<TaskStartAt> {
    if value.len() != 14 || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(Error::config("--task-start-at must use yyyyMMddHHmmss"));
    }
    let month = parse_date_part(value, 4, 6, "--task-start-at")?;
    let day = parse_date_part(value, 6, 8, "--task-start-at")?;
    let hour = parse_date_part(value, 8, 10, "--task-start-at")?;
    let minute = parse_date_part(value, 10, 12, "--task-start-at")?;
    let second = parse_date_part(value, 12, 14, "--task-start-at")?;
    let year = parse_date_part(value, 0, 4, "--task-start-at")?;
    if !valid_calendar_time(year, month, day, hour, minute, second) {
        return Err(Error::config(
            "--task-start-at contains an out-of-range field",
        ));
    }
    Ok(TaskStartAt::new(value.to_string()))
}

fn valid_calendar_time(
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> bool {
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return false;
    }
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

#[allow(clippy::manual_is_multiple_of)]
fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn parse_date_part(value: &str, start: usize, end: usize, option: &str) -> Result<u32> {
    value
        .get(start..end)
        .ok_or_else(|| Error::config(format!("{option} date field is invalid")))?
        .parse::<u32>()
        .map_err(|_| Error::config(format!("{option} date field is invalid")))
}

fn parse_stream_filter(value: &str) -> Result<StreamFilter> {
    let params = ComplexParams::parse(value)?;
    let for_choice = if is_direct_for_choice(value) {
        value.to_string()
    } else {
        params.get("for")?.unwrap_or_else(|| "best".to_string())
    };
    if !for_choice.is_empty() && !is_direct_for_choice(&for_choice) {
        return Err(Error::config(format!("for={for_choice} is invalid")));
    }
    let id = validate_filter_regex(params.get("id")?, "id")?;
    let language = validate_filter_regex(params.get("lang")?, "lang")?;
    let name = validate_filter_regex(params.get("name")?, "name")?;
    let codecs = validate_filter_regex(params.get("codecs")?, "codecs")?;
    let resolution = validate_filter_regex(params.get("res")?, "res")?;
    let frame_rate = validate_filter_regex(params.get("frame")?, "frame")?;
    let channels = validate_filter_regex(params.get("channel")?, "channel")?;
    let range = validate_filter_regex(params.get("range")?, "range")?;
    let url = validate_filter_regex(params.get("url")?, "url")?;
    Ok(StreamFilter {
        for_choice,
        id,
        language,
        name,
        codecs,
        resolution,
        frame_rate,
        channels,
        range,
        url,
        segment_count_min: parse_optional_i64(params.get("segsMin")?, "segsMin")?,
        segment_count_max: parse_optional_i64(params.get("segsMax")?, "segsMax")?,
        playlist_duration_min: parse_optional_hms(params.get("plistDurMin")?, "plistDurMin")?,
        playlist_duration_max: parse_optional_hms(params.get("plistDurMax")?, "plistDurMax")?,
        bandwidth_min: parse_optional_bandwidth(params.get("bwMin")?, "bwMin")?,
        bandwidth_max: parse_optional_bandwidth(params.get("bwMax")?, "bwMax")?,
        role: parse_optional_role(params.get("role")?),
    })
}

fn validate_filter_regex(value: Option<String>, name: &str) -> Result<Option<String>> {
    if let Some(value) = value {
        if value.is_empty() {
            return Ok(None);
        }
        let _ = regex::Regex::new(&value)
            .map_err(|error| Error::config(format!("{name} regex is invalid: {error}")))?;
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

fn is_direct_for_choice(value: &str) -> bool {
    if value == "all" {
        return true;
    }
    for prefix in ["best", "worst"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return rest.chars().all(|ch| ch.is_ascii_digit());
        }
    }
    false
}

fn parse_optional_i64(value: Option<String>, option: &str) -> Result<Option<i64>> {
    match value {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => Ok(Some(parse_i64(&value, option)?)),
        None => Ok(None),
    }
}

fn parse_optional_hms(value: Option<String>, option: &str) -> Result<Option<f64>> {
    match value {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => Ok(Some(parse_hms_duration(&value, option)?)),
        None => Ok(None),
    }
}

fn parse_optional_bandwidth(value: Option<String>, option: &str) -> Result<Option<i64>> {
    match value {
        Some(value) if value.is_empty() => Ok(None),
        Some(value) => {
            let parsed = parse_i32(&value, option)?;
            Ok(Some(i64::from(parsed.wrapping_mul(1000))))
        }
        None => Ok(None),
    }
}

fn parse_optional_role(value: Option<String>) -> Option<RoleType> {
    RoleType::parse_enum_token(&value?)
}

fn parse_mux_after_done(value: &str) -> Result<MuxAfterDoneOptions> {
    let params = ComplexParams::parse(value)?;
    let format_value = params
        .get("format")?
        .unwrap_or_else(|| first_colon_token(value));
    let muxer_value = params.get("muxer")?.unwrap_or_else(|| "ffmpeg".to_string());
    let format = parse_mux_format(&format_value)?;
    let muxer = parse_muxer(&muxer_value)?;
    if muxer == MuxerKind::Mkvmerge && format_value == "mp4" {
        return Err(Error::config(
            "mkvmerge cannot be used for mp4 mux-after-done",
        ));
    }
    if muxer == MuxerKind::Mp4forge && format != MuxFormat::Mp4 {
        return Err(Error::config(
            "mp4forge mux-after-done is only valid for mp4 output",
        ));
    }
    let fallback_muxer = parse_optional_fallback_muxer(params.get("fallback_muxer")?, muxer)?;
    if fallback_muxer == Some(MuxerKind::Mkvmerge) {
        return Err(Error::config(
            "mkvmerge cannot be used as an mp4forge fallback for mp4 mux-after-done",
        ));
    }
    let bin_path = match params.get("bin_path")? {
        Some(value) if value.is_empty() => {
            return Err(Error::config("bin_path must not be empty"));
        }
        Some(value) if value == "auto" => None,
        Some(value) => Some(PathBuf::from(value)),
        None => None,
    };
    Ok(MuxAfterDoneOptions {
        format,
        muxer,
        fallback_muxer,
        bin_path,
        keep: parse_complex_bool(params.get("keep")?, false, "keep")?,
        skip_sub: parse_complex_bool(params.get("skip_sub")?, false, "skip_sub")?,
    })
}

fn parse_optional_fallback_muxer(
    value: Option<String>,
    primary_muxer: MuxerKind,
) -> Result<Option<MuxerKind>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(Error::config("fallback_muxer must not be empty"));
    }
    if value == "none" {
        return Ok(None);
    }
    if primary_muxer != MuxerKind::Mp4forge {
        return Err(Error::config(
            "fallback_muxer is only valid with muxer=mp4forge",
        ));
    }
    let muxer = parse_muxer(&value)?;
    if muxer == MuxerKind::Mp4forge {
        return Err(Error::config("fallback_muxer must be ffmpeg"));
    }
    Ok(Some(muxer))
}

fn parse_mux_format(value: &str) -> Result<MuxFormat> {
    match value.to_ascii_lowercase().as_str() {
        "mp4" => Ok(MuxFormat::Mp4),
        "mkv" => Ok(MuxFormat::Mkv),
        "ts" => Ok(MuxFormat::Ts),
        _ => Err(Error::config(format!("invalid mux format {value}"))),
    }
}

fn parse_muxer(value: &str) -> Result<MuxerKind> {
    match value {
        "ffmpeg" => Ok(MuxerKind::Ffmpeg),
        "mkvmerge" => Ok(MuxerKind::Mkvmerge),
        "mp4forge" => Ok(MuxerKind::Mp4forge),
        _ => Err(Error::config(format!("invalid muxer {value}"))),
    }
}

fn parse_complex_bool(value: Option<String>, default: bool, name: &str) -> Result<bool> {
    match value {
        Some(value) => match value.as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(Error::config(format!("{name} must be true or false"))),
        },
        None => Ok(default),
    }
}

fn first_colon_token(value: &str) -> String {
    for (index, ch) in value.char_indices() {
        if ch == ':' {
            return value
                .get(..index)
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
        }
    }
    value.to_string()
}

async fn parse_mux_import(value: &str) -> Result<MuxImport> {
    let params = ComplexParams::parse(value)?;
    let path = params.get("path")?.unwrap_or_else(|| value.to_string());
    let path = PathBuf::from(path);
    if !tokio::fs::metadata(&path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Err(Error::config("--mux-import path must be an existing file"));
    }
    let mut import = MuxImport::new(path);
    import.language = params.get("lang")?;
    import.name = params.get("name")?;
    Ok(import)
}

async fn parse_hls_bytes(value: &str, option: &str) -> Result<Option<Vec<u8>>> {
    if value.is_empty() {
        return Ok(None);
    }
    let path = Path::new(value);
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Ok(Some(tokio::fs::read(path).await?));
    }
    if let Some(hex) = strip_hex_prefix(value)
        && (hex.is_empty() || is_hex(hex))
    {
        return hex_to_bytes(hex, option).map(Some);
    }
    if is_hex(value) {
        return hex_to_bytes(value, option).map(Some);
    }
    base64_decode(value)
        .map(Some)
        .map_err(|message| Error::config(format!("{option}: {message}")))
}

fn strip_hex_prefix(value: &str) -> Option<&str> {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
}

fn parse_custom_key(value: &str) -> Result<CustomKey> {
    if is_cli_key_hex_16_literal(value) {
        return Ok(CustomKey::Key {
            key_hex: value.to_string(),
        });
    }

    let raw_parts: Vec<&str> = value.split(':').collect();
    if let [left, right] = raw_parts.as_slice() {
        if is_cli_key_hex_16_literal(left) && is_cli_key_hex_16_literal(right) {
            return Ok(CustomKey::Kid {
                kid_hex: (*left).to_string(),
                key_hex: (*right).to_string(),
            });
        }
        if is_digit_literal(left) && is_cli_key_hex_16_literal(right) {
            let track_id = left
                .parse::<u32>()
                .map_err(|_| Error::config("--key track id is invalid"))?;
            return Ok(CustomKey::Track {
                track_id,
                key_hex: (*right).to_string(),
            });
        }
    }

    let parts: Vec<&str> = value
        .split(':')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    match parts.as_slice() {
        [] => Err(Error::config("--key must be key, KID:key, or trackId:key")),
        [single] => Ok(CustomKey::Key {
            key_hex: parse_key_part(single, "--key")?,
        }),
        [kid, key] => Ok(CustomKey::Kid {
            kid_hex: parse_key_part(kid, "--key KID")?,
            key_hex: parse_key_part(key, "--key value")?,
        }),
        _ => Err(Error::config("--key must be key, KID:key, or trackId:key")),
    }
}

fn parse_key_part(value: &str, option: &str) -> Result<String> {
    if is_cli_key_hex_16_literal(value) {
        return Ok(value.to_ascii_lowercase());
    }
    let bytes =
        base64_decode(value).map_err(|message| Error::config(format!("{option}: {message}")))?;
    if bytes.len() != 16 {
        return Err(Error::config(format!("{option} must decode to 16 bytes")));
    }
    Ok(bytes_to_hex(&bytes))
}

fn is_cli_key_hex_16_literal(value: &str) -> bool {
    value.len() == 32 && value.chars().all(is_cli_key_hex_char)
}

fn is_cli_key_hex_char(ch: char) -> bool {
    ch.is_ascii_digit() || ('a'..='f').contains(&ch) || ('A'..='F').contains(&ch)
}

fn is_digit_literal(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit())
}

fn is_hex(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|ch| ch.is_ascii_hexdigit()) && is_even(value.len())
}

fn hex_to_bytes(value: &str, option: &str) -> Result<Vec<u8>> {
    if !is_even(value.len()) {
        return Err(Error::config(format!("{option} hex length must be even")));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut chars = value.chars();
    while let Some(high) = chars.next() {
        let low = chars
            .next()
            .ok_or_else(|| Error::config(format!("{option} hex length must be even")))?;
        let high =
            hex_value(high).ok_or_else(|| Error::config(format!("{option} hex is invalid")))?;
        let low =
            hex_value(low).ok_or_else(|| Error::config(format!("{option} hex is invalid")))?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn is_even(value: usize) -> bool {
    value & 1 == 0
}

fn hex_value(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some(ch as u8 - b'0'),
        'a'..='f' => Some(ch as u8 - b'a' + 10),
        'A'..='F' => Some(ch as u8 - b'A' + 10),
        _ => None,
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let high = usize::from(byte >> 4);
        let low = usize::from(byte & 0x0f);
        out.push(char::from(HEX[high]));
        out.push(char::from(HEX[low]));
    }
    out
}

fn base64_decode(value: &str) -> std::result::Result<Vec<u8>, String> {
    crate::base64::decode_base64(value).map_err(str::to_string)
}

struct ComplexParams {
    source: String,
}

impl ComplexParams {
    fn parse(value: &str) -> Result<Self> {
        Ok(Self {
            source: value.to_string(),
        })
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        if key.is_empty() || self.source.is_empty() {
            return Ok(None);
        }

        let needle = format!("{key}=");
        let Some(index) = self.source.find(&needle) else {
            if self.source.contains(key) && self.source.ends_with(key) {
                return Ok(Some("true".to_string()));
            }
            return Ok(None);
        };
        let start = index + needle.len();
        let Some(rest) = self.source.get(start..) else {
            return Err(Error::config(format!("complex option {key} is invalid")));
        };
        let mut result = String::new();
        let mut last = '\0';
        for ch in rest.chars() {
            if ch == ':' {
                if last == '\\' {
                    result = result.replace('\\', "");
                    last = ch;
                    result.push(ch);
                } else {
                    break;
                }
            } else {
                last = ch;
                result.push(ch);
            }
        }

        let cleaned = result
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        if cleaned.contains('"') || cleaned.contains('\'') {
            return Err(Error::config(format!("complex option {key} is invalid")));
        }
        Ok(Some(cleaned))
    }
}

fn pre_scan_ui_language(tokens: &[String]) -> Option<UiLanguage> {
    let mut index = 0_usize;
    while index < tokens.len() {
        let token = &tokens[index];
        let (name, inline_value) = split_inline_option(token.clone());
        if name == "--ui-language" {
            let value = match inline_value {
                Some(value) => Some(value),
                None => tokens.get(index + 1).cloned(),
            };
            if let Some(value) = value
                && let Ok(language) = parse_ui_language(&value)
            {
                return Some(language);
            }
        }
        index += 1;
    }
    None
}

fn morehelp_text(topic: &str, _language: Option<UiLanguage>) -> String {
    match topic {
        "mux-after-done" => mux_help(),
        "mux-import" => import_help(),
        "select-video" | "select-audio" | "select-subtitle" => selection_help(),
        "custom-range" => range_help(),
        topic => no_help_text(topic),
    }
}

fn help_text() -> String {
    [
        format!("Description:\n  haki-dl {}", env!("CARGO_PKG_VERSION")),
        "Usage:\n  haki-dl <input> [options]".to_string(),
        "Arguments:\n  <input>  Input URL or file".to_string(),
        format!("Options:\n{}", help_options_text()),
    ]
    .join("\n\n")
}

fn help_options_text() -> String {
    CLI_SCHEMA
        .iter()
        .filter(|row| row.value_kind != CliSchemaValueKind::Positional && row.show_in_help)
        .map(|row| {
            let aliases = row.aliases.join(", ");
            if row.default == "none" || row.default == "false" {
                format!("  {aliases}")
            } else {
                format!("  {aliases} [default: {}]", row.default)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn version_text() -> String {
    format!("haki-dl {}", env!("CARGO_PKG_VERSION"))
}

fn mux_help() -> String {
    [
        "mux-after-done grammar:",
        "  -M <format>",
        "  -M <format>:muxer=<ffmpeg|mkvmerge|mp4forge>:fallback_muxer=<ffmpeg|none>:bin_path=<path|auto>:keep=<true|false>:skip_sub=<true|false>",
        "  formats: mp4, mkv, ts",
        "  defaults: muxer=ffmpeg, fallback_muxer=none, bin_path=auto, keep=false, skip_sub=false",
    ]
    .join("\n")
}

fn import_help() -> String {
    [
        "mux-import grammar:",
        "  --mux-import <path>",
        "  --mux-import path=<path>:lang=<language>:name=<track name>",
        "  escape literal colons inside complex values as \\:",
        "  mux imports require --mux-after-done",
    ]
    .join("\n")
}

fn selection_help() -> String {
    [
        "selection filter grammar:",
        "  all | best | bestN | worst | worstN",
        "  for=<choice>:id=<pattern>:lang=<pattern>:name=<pattern>:codecs=<pattern>:res=<pattern>:frame=<pattern>:channel=<pattern>:range=<pattern>:url=<pattern>",
        "  segsMin=<n>:segsMax=<n>:plistDurMin=<1h2m3s>:plistDurMax=<1h2m3s>:bwMin=<kbps>:bwMax=<kbps>:role=<role>",
        "  escape literal colons inside complex values as \\:",
    ]
    .join("\n")
}

fn range_help() -> String {
    [
        "custom-range grammar:",
        "  --custom-range <start-segment>-<end-segment>",
        "  --custom-range <start-time>-<end-time>",
        "  time fields use colon-separated duration parts from right to left: seconds, minutes, hours, days",
        "  an empty start means 0; an empty end means open-ended",
    ]
    .join("\n")
}

fn no_help_text(topic: &str) -> String {
    format!("No extended help is available for {topic}.")
}

fn cli_error_text(error: &Error, _language: Option<UiLanguage>) -> String {
    let (category, message) = match error {
        Error::Protocol { message } => ("protocol", message.as_str()),
        Error::Http { message } => ("http", message.as_str()),
        Error::Io(error) => {
            return format!("{}: {error}", cli_error_category("io"));
        }
        Error::Decrypt { message } => ("decrypt", message.as_str()),
        Error::Mux { message } => ("mux", message.as_str()),
        Error::Subtitle { message } => ("subtitle", message.as_str()),
        Error::Live { message } => ("live", message.as_str()),
        Error::Config { message } => ("config", message.as_str()),
        Error::UserCancelled => return "operation cancelled".to_string(),
        Error::Compatibility { message } => ("compatibility", message.as_str()),
    };
    if message.starts_with("Segment count check not pass,") {
        return message.to_string();
    }
    format!("{}: {message}", cli_error_category(category))
}

fn cli_error_text_for_log_level(
    error: &Error,
    language: Option<UiLanguage>,
    log_level: LogLevel,
) -> String {
    if log_level == LogLevel::Error {
        format!("{error:?}")
    } else {
        cli_error_text(error, language)
    }
}

fn cli_error_category(category: &str) -> &'static str {
    match category {
        "protocol" => "protocol error",
        "http" => "http error",
        "io" => "io error",
        "decrypt" => "decrypt error",
        "mux" => "mux error",
        "subtitle" => "subtitle error",
        "live" => "live error",
        "config" => "config error",
        "compatibility" => "compatibility error",
        _ => "error",
    }
}
