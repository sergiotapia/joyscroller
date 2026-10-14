# Changelog

What each release of Joyscroller changed. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

## [0.2.0] - 2026-10-14

### Added

- Search the video list. Words, `"quoted text"`, `-exclusions`, `a|b` alternatives, `prefix*` and `*patterns` all work, and matches come from each video's title, author and description.
- Sort the video list by any column. The sort is part of the page's address, so a sorted list can be bookmarked.
- Downloaded videos show their saved thumbnail in the list.

### Changed

- yt-dlp now uses bun instead of deno to solve YouTube's JavaScript challenges, since Deno's standalone runtime is winding down after Cloudflare bought it. Joyscroller fetches bun on first launch (or uses one already installed) and removes the deno earlier versions fetched. ([#10](https://github.com/sergiotapia/joyscroller/issues/10))

### Fixed

- Portrait videos fit the screen in fullscreen.

### Security

- Updated rustls to 0.23.46, which fixes resumed connections falling back to classical-only key exchange.

## [0.1.0] - 2026-10-12

The first release, for Linux on x86_64 and aarch64.

### Added

- Syncs your YouTube likes, TikTok likes and Instagram saved videos into a local library on launch and every 5 minutes after, signed in through your browser's cookies.
- A web UI at http://127.0.0.1:4774 where new likes appear live.
- Downloads each video at the best quality available into `downloads/<platform>/<date liked>/<title> [<id>]/`, including TikTok photo posts' images and sound. Each platform works through its backlog newest first, paced to stay clear of rate limits, and "Download now" on a video's page downloads it right away.
- A `/downloading` page showing what's downloading and what finished.
- Downloaded videos play on their page, and TikTok photo posts play as a slideshow with their sound.
- Fetches its own yt-dlp, ffmpeg and deno on first launch, checksum-verified, and keeps yt-dlp updated daily. Nothing else to install.

[Unreleased]: https://github.com/sergiotapia/joyscroller/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/sergiotapia/joyscroller/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sergiotapia/joyscroller/releases/tag/v0.1.0
