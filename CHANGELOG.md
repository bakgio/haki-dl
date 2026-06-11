# 0.2.0 (Jun 10, 2026)

- Added CLI missing-input handling, including the required-argument message and full help output when no input is provided.
- Reworked CLI help rendering, value placeholders, aligned descriptions, wrapped long descriptions, and expanded `--morehelp` text for mux, import, selection, and range options.
- Preserved forced ANSI console behavior for redirected output so redirected console runs can still validate colored output when explicitly requested.
- Fixed ffmpeg concat-demuxer merge planning to use absolute, normalized input paths for portable generated concat lists.
- Enabled the release workflow semver check as a non-blocking pre-1.0 gate and made release binary/release jobs wait for it.

# 0.1.0 (Jun 8, 2026)

- Initial crate release
