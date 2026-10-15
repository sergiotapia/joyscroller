# Changelog

What each release of Joyscroller changed. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

## [0.7.0] - 2026-10-15

### Added

- On a Mac, Joyscroller now also reads your logins from Safari. macOS only lets it do that once you turn on Joyscroller in **System Settings → Privacy & Security → Full Disk Access**; until you do, the sidebar says so where a sync failed. Safari can't open TikTok and Instagram unseen in the background, so Joyscroller uses another browser you have installed for them, or downloads its own hidden copy of Chrome.

## [0.6.0] - 2026-10-15

### Added

- Joyscroller now also reads your logins from Firefox and Opera, and on a Mac from Opera GX and Arc. Firefox and Arc can't open TikTok and Instagram unseen in the background, so with them Joyscroller uses another browser you have installed, or the first time downloads its own hidden copy of Chrome (about 125 MB).

### Changed

- Joyscroller no longer needs Brave. Each time it starts, it looks for every browser it supports and reads your logins from the one logged in to the most of YouTube, TikTok and Instagram. To switch browsers, log in with the new one and restart Joyscroller.

## [0.5.0] - 2026-10-14

### Added

- Joyscroller runs on macOS. Download the `.dmg` for your Mac's chip, drag Joyscroller to Applications, and it opens in your browser. It reads your logins from Brave like it does on Linux, after asking once for its key in the Keychain. Videos are saved in `~/Movies/Joyscroller`. ([#1](https://github.com/sergiotapia/joyscroller/issues/1))
- A red **Turn off** button at the end of the taskbar stops Joyscroller, along with any download in progress, after asking you to confirm.
- Starting Joyscroller while it's already running opens the running copy in your browser, instead of stopping with an error about its address being in use.

### Fixed

- Syncing with Microsoft Edge on Linux now reads all of your logins. Joyscroller looked for Edge's key in the keyring under the wrong name, so it skipped most of Edge's cookies.

## [0.4.0] - 2026-10-14

### Added

- Every page now shows which version of Joyscroller is running, in the status bar at the bottom of the window, linking to that version's release notes. The version is also in the first line Joyscroller prints when it starts, and `joyscroller --version` prints it. ([#12](https://github.com/sergiotapia/joyscroller/issues/12))

## [0.3.0] - 2026-10-14

### Added

- Every downloaded video now plays in every browser, phones and cars included. Most videos come in formats only some browsers play (YouTube's AV1, TikTok's HEVC, Instagram's VP9), so Joyscroller makes a playback copy in H.264 next to each one, at the same resolution, in the background and one at a time. The original is kept as downloaded. A video's page plays the original where the browser handles it smoothly and the copy everywhere else, and a button switches between them. ([#4](https://github.com/sergiotapia/joyscroller/issues/4))
- Before updating your library to a new version, Joyscroller now saves a copy of it in a `backups` folder beside it, so an update can always be undone.

### Changed

- This version updates your library in a way older versions can't read. If you go back to an older Joyscroller, restore the copy it saved in `backups` first. From this version on, Joyscroller says plainly when a library was made by a newer version, instead of failing with database errors.

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

[Unreleased]: https://github.com/sergiotapia/joyscroller/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/sergiotapia/joyscroller/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/sergiotapia/joyscroller/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/sergiotapia/joyscroller/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/sergiotapia/joyscroller/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/sergiotapia/joyscroller/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sergiotapia/joyscroller/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sergiotapia/joyscroller/releases/tag/v0.1.0
