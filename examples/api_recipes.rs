use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use haki_dl::{
    CancellationToken, CustomKey, DecryptionEngine, DownloadClient, DownloadOptions,
    DownloadRequest, MuxAfterDoneOptions, MuxFormat, MuxerKind, ProgressEvent, StreamSelector,
    summarize_events,
};

#[tokio::main]
async fn main() -> haki_dl::Result<()> {
    let workspace = prepare_workspace("haki_dl_api_recipes").await?;
    let hls_input = write_hls_fixture(&workspace).await?;
    let dash_input = write_dash_fixture(&workspace).await?;
    let token = CancellationToken::new();
    let mut recipes = vec![
        ("simple_hls", simple_hls_request(&hls_input, &workspace)),
        ("simple_dash", simple_dash_request(&dash_input, &workspace)),
        (
            "default_decrypt",
            encrypted_request_with_default_decrypt(&dash_input, &workspace),
        ),
        (
            "custom_headers_proxy",
            custom_header_proxy_request(&hls_input, &workspace),
        ),
        (
            "cancellable_live",
            cancellable_live_request(&hls_input, &workspace, token.clone()),
        ),
        (
            "explicit_mp4_mux",
            explicit_mp4_mux_request(&dash_input, &workspace),
        ),
    ];
    if let Some(request) = external_decrypt_request(&dash_input, &workspace) {
        recipes.push(("external_decrypt", request));
    } else {
        println!("external_decrypt skipped: ffmpeg was not found on PATH");
    }

    for (name, request) in recipes {
        let events = Box::pin(DownloadClient::new().prepare(request)?.start()).await?;
        let summary = summarize_events(&events);
        let latest_progress = events.iter().rev().find_map(progress_bytes);
        println!(
            "recipe={name} events={} finished={:?} latest_progress={latest_progress:?}",
            events.len(),
            summary.finished
        );
    }

    token.cancel();
    Ok(())
}

async fn prepare_workspace(name: &str) -> haki_dl::Result<PathBuf> {
    let workspace = std::env::temp_dir().join(name);
    if tokio::fs::metadata(&workspace).await.is_ok() {
        tokio::fs::remove_dir_all(&workspace).await?;
    }
    tokio::fs::create_dir_all(&workspace).await?;
    Ok(workspace)
}

async fn write_hls_fixture(workspace: &Path) -> haki_dl::Result<PathBuf> {
    let playlist = workspace.join("media.m3u8");
    tokio::fs::write(
        &playlist,
        concat!(
            "#EXTM3U\n",
            "#EXT-X-VERSION:3\n",
            "#EXT-X-TARGETDURATION:1\n",
            "#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXTINF:1.000,\n",
            "segment-0.ts\n",
            "#EXT-X-ENDLIST\n",
        ),
    )
    .await?;
    Ok(playlist)
}

async fn write_dash_fixture(workspace: &Path) -> haki_dl::Result<PathBuf> {
    let manifest = workspace.join("stream.mpd");
    tokio::fs::write(
        &manifest,
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            "\n",
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT2S">"#,
            "\n  <Period id=\"0\" duration=\"PT2S\">",
            "\n    <AdaptationSet contentType=\"video\" mimeType=\"video/mp4\" codecs=\"avc1.42c00a\">",
            "\n      <Representation id=\"v1\" bandwidth=\"12000\" width=\"32\" height=\"32\">",
            "\n        <SegmentList timescale=\"1\" duration=\"1\">",
            "\n          <Initialization sourceURL=\"init.mp4\"/>",
            "\n          <SegmentURL media=\"seg-1.m4s\"/>",
            "\n          <SegmentURL media=\"seg-2.m4s\"/>",
            "\n        </SegmentList>",
            "\n      </Representation>",
            "\n    </AdaptationSet>",
            "\n  </Period>",
            "\n</MPD>\n",
        ),
    )
    .await?;
    Ok(manifest)
}

fn base_options(workspace: &Path, save_name: &str) -> DownloadOptions {
    DownloadOptions {
        save_name: Some(save_name.to_string()),
        save_dir: Some(workspace.join("out")),
        tmp_dir: Some(workspace.join("tmp")),
        no_log: true,
        disable_update_check: true,
        skip_download: true,
        write_meta_json: false,
        ..DownloadOptions::default()
    }
}

fn base_request(input: &Path, workspace: &Path, save_name: &str) -> DownloadRequest {
    DownloadRequest::new(input.to_string_lossy().into_owned())
        .with_options(base_options(workspace, save_name))
        .with_stream_selector(StreamSelector::Auto)
}

fn simple_hls_request(input: &Path, workspace: &Path) -> DownloadRequest {
    base_request(input, workspace, "simple_hls")
}

fn simple_dash_request(input: &Path, workspace: &Path) -> DownloadRequest {
    base_request(input, workspace, "simple_dash")
}

fn encrypted_request_with_default_decrypt(input: &Path, workspace: &Path) -> DownloadRequest {
    let mut request = base_request(input, workspace, "default_decrypt");
    request.options.keys.push(CustomKey::Kid {
        kid_hex: "00112233445566778899aabbccddeeff".to_string(),
        key_hex: "000102030405060708090a0b0c0d0e0f".to_string(),
    });
    request
}

fn external_decrypt_request(input: &Path, workspace: &Path) -> Option<DownloadRequest> {
    let mut request = encrypted_request_with_default_decrypt(input, workspace);
    request.options.save_name = Some("external_decrypt".to_string());
    request.options.decryption_engine = DecryptionEngine::Ffmpeg;
    request.options.decryption_binary_path = Some(find_ffmpeg()?);
    Some(request)
}

fn custom_header_proxy_request(input: &Path, workspace: &Path) -> DownloadRequest {
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_string(), "Bearer redacted".to_string());
    let mut request = base_request(input, workspace, "custom_headers_proxy");
    request.options.headers = headers;
    request.options.custom_proxy = Some("http://127.0.0.1:8080".to_string());
    request
}

fn cancellable_live_request(
    input: &Path,
    workspace: &Path,
    token: CancellationToken,
) -> DownloadRequest {
    let mut request = base_request(input, workspace, "cancellable_live");
    request.options.live_record_limit = Some(Duration::from_secs(60));
    request.options.live_wait_time = Some(2);
    request.with_cancellation_token(token)
}

fn explicit_mp4_mux_request(input: &Path, workspace: &Path) -> DownloadRequest {
    let mut request = base_request(input, workspace, "explicit_mp4_mux");
    request.options.mux_after_done = Some(MuxAfterDoneOptions {
        format: MuxFormat::Mp4,
        muxer: MuxerKind::Mp4forge,
        fallback_muxer: None,
        bin_path: None,
        keep: false,
        skip_sub: false,
    });
    request
}

fn progress_bytes(event: &ProgressEvent) -> Option<(u64, u64)> {
    match event {
        ProgressEvent::AggregateProgress(progress) => {
            Some((progress.downloaded_bytes, progress.bytes_per_second))
        }
        ProgressEvent::StreamProgress(progress) => {
            Some((progress.downloaded_bytes, progress.bytes_per_second))
        }
        _ => None,
    }
}

fn find_ffmpeg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        for name in ffmpeg_file_names() {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(windows)]
fn ffmpeg_file_names() -> &'static [&'static str] {
    &["ffmpeg.exe", "ffmpeg.cmd", "ffmpeg.bat", "ffmpeg"]
}

#[cfg(not(windows))]
fn ffmpeg_file_names() -> &'static [&'static str] {
    &["ffmpeg"]
}
