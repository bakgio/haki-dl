# 0.3.0 (Jun 11, 2026)

- Added the optional `rpc` feature with a haki-native JSON-RPC server over HTTP POST and WebSocket, including `RpcServer`, `RpcServerBuilder`, `RpcSessionManager`, token/basic/TLS configuration, CORS control, and request-size limits.
- Added RPC download/session methods for adding downloads, queue-aware pause/resume/removal, status/list queries, option changes, queued URI replacement, global stats/options, shutdown, method discovery, notification discovery, and `system.multicall`.
- Added structured RPC notifications for download start, pause, stop, completion, error, and live progress events with gid-based status fields.
- Added queue and concurrency controls so RPC callers can enqueue many downloads while limiting active non-blocking async work.
- Added CLI RPC server mode and matching options for listen address/port, auth, TLS, request size, CORS, queue mode, and max concurrent downloads while keeping the public CLI parse result semver-compatible.
- Updated the public examples to use auto-cleaned temporary workspaces with `tempfile::TempDir`.

# 0.2.0 (Jun 10, 2026)

- Added CLI missing-input handling, including the required-argument message and full help output when no input is provided.
- Reworked CLI help rendering, value placeholders, aligned descriptions, wrapped long descriptions, and expanded `--morehelp` text for mux, import, selection, and range options.
- Preserved forced ANSI console behavior for redirected output so redirected console runs can still validate colored output when explicitly requested.
- Fixed ffmpeg concat-demuxer merge planning to use absolute, normalized input paths for portable generated concat lists.
- Enabled the release workflow semver check as a non-blocking pre-1.0 gate and made release binary/release jobs wait for it.

# 0.1.0 (Jun 8, 2026)

- Initial crate release
