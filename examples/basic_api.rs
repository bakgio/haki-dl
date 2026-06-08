use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use haki_dl::{
    DownloadClient, DownloadOptions, DownloadRequest, ProgressCallback, ProgressEventCollector,
    StreamSelector, summarize_events,
};

#[tokio::main]
async fn main() -> haki_dl::Result<()> {
    let workspace = prepare_workspace("haki_dl_basic_api").await?;
    let input = write_hls_fixture(&workspace).await?;
    let callback_events = Arc::new(Mutex::new(ProgressEventCollector::new()));
    let callback_sink = Arc::clone(&callback_events);

    let request = DownloadRequest::new(input.to_string_lossy().into_owned())
        .with_options(example_options(&workspace, "basic_api"))
        .with_stream_selector(StreamSelector::Auto)
        .with_progress_callback(ProgressCallback::new(move |event| {
            if let Ok(mut collector) = callback_sink.lock() {
                collector.emit(event.clone());
            }
            Ok(())
        }));

    let events = Box::pin(DownloadClient::new().prepare(request)?.start()).await?;
    let summary = summarize_events(&events);
    let callback_summary = callback_events
        .lock()
        .ok()
        .map(|collector| collector.summary());

    println!(
        "events={} finished={:?} callback_finished={:?} outputs={}",
        events.len(),
        summary.finished,
        callback_summary.and_then(|summary| summary.finished),
        summary.outputs.len()
    );
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

fn example_options(workspace: &Path, save_name: &str) -> DownloadOptions {
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
