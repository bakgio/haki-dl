<p align="center">
  <h1 align="center">haki-dl</h1>
  <p align="center">
    Async/Tokio-based Rust downloader library and CLI foundation for HLS, DASH, MSS, live recording, decryption planning, subtitle handling, and mux orchestration.
  </p>
  <p align="center">
    <a href="https://crates.io/crates/haki-dl"><img src="https://img.shields.io/crates/v/haki-dl.svg" alt="Crates.io"></a>
    &nbsp;&nbsp;
    <a href="https://docs.rs/haki-dl"><img src="https://img.shields.io/docsrs/haki-dl" alt="docs.rs"></a>
    &nbsp;&nbsp;
    <a href="LICENSE-MIT"><img src="https://img.shields.io/crates/l/haki-dl.svg" alt="License"></a>
    &nbsp;&nbsp;
    <img src="https://img.shields.io/badge/MSRV-1.88-blue.svg" alt="MSRV 1.88">
  </p>
</p>

---

- Async/Tokio-based library API for planned download sessions, live recording, cancellation, structured progress events, stream selection, and output artifacts
- CLI adapter over the same public request/options model, with CLI parameter names represented as snake_case API fields
- HLS, DASH, MSS, local file, `file:`, `base64://`, and `hex://` input handling
- Segment scheduling, retry helpers, cleanup helpers, temp-root ownership, metadata writing, and progress aggregation
- HLS segment decryption helpers, MP4 protection metadata probing, external decrypt planning, and redacted diagnostics
- Subtitle parsing, WebVTT/SRT formatting, WVTT/STPP/TTML extraction helpers, and image subtitle output helpers
- Binary merge helpers plus ffmpeg and mkvmerge command planning; mp4forge decryption is in-process by default while mp4forge muxing is explicit opt-in for MP4-family outputs only

## Installation

```toml
[dependencies]
haki-dl = "0.2.0"
tokio = { version = "1.52.1", features = ["rt-multi-thread", "macros"] }

# With optional features:
# haki-dl = { version = "0.2.0", features = ["mp4forge"] }
# haki-dl = { version = "0.2.0", features = ["serde"] }
# Minimal builds can opt out of defaults:
# haki-dl = { version = "0.2.0", default-features = false }
```

Install the CLI from crates.io:

```sh
cargo install haki-dl --locked
```

Install the current checkout locally:

```sh
cargo install --path . --locked
```

The published package is `haki-dl`; it exposes the Rust library crate as `haki_dl` and installs the `haki-dl` binary from `src/main.rs`.

API consumers need a Tokio runtime. The CLI creates its own runtime, but library callers should use `#[tokio::main]`, an existing application runtime, or an explicitly managed runtime.

## Feature Flags

`haki-dl` keeps protocol parsing, live support, decryption helpers, subtitle handling, and mux planning in the core library. Cargo features are intentionally limited to package-level CLI selection and true optional Rust integrations. mp4forge decryption is the in-process default when the `mp4forge` feature is enabled; mp4forge muxing remains explicit opt-in.

- `cli`: retained as the default CLI-capable package feature. The `haki-dl` binary is built from `src/main.rs` and is not hidden behind a `required-features` gate.
- `mp4forge`: enables the published `mp4forge` crate backend for default in-process MP4 decryption and explicit MP4-family mux requests. Muxing is not selected by default.
- `serde`: derives `Serialize` and `Deserialize` for reusable public metadata/report types where exposed.

HLS, DASH, MSS, live, decrypt, capture, and license are not Cargo feature flags. Core protocol/decrypt/live behavior stays available in the library until there is a real, tested need for coarse minimal-build gates. ffmpeg is validated at session startup for compatibility runtime behavior. mkvmerge and external decrypt tools are runtime process tools selected through options such as `mkvmerge_binary_path`, `decryption_engine`, `decryption_binary_path`, and `mux_after_done`. They are intentionally not Cargo feature flags.

## CLI

```text
USAGE: haki-dl [OPTIONS] <INPUT>

INPUT:
  Manifest URL, local manifest path, or direct media URL

COMMON OPTIONS:
  --save-name <NAME>
  --save-dir <DIR>
  --tmp-dir <DIR>
  --thread-count <N>
  --download-retry-count <N>
  --ffmpeg-binary-path <PATH>
  --mkvmerge-binary-path <PATH>
  --mux-after-done <FORMAT[:key=value...]>
  --custom-hls-key <KEY>
  --custom-proxy <PROXY>
  --no-log
```

`--mux-after-done` defaults to ffmpeg-backed planning unless the user explicitly selects mkvmerge. mp4forge muxing is only used when the request explicitly selects `muxer=mp4forge` for a supported MP4-family output.

> See the [`examples/`](examples/) directory for the public API usage instead of the CLI.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in haki-dl by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
