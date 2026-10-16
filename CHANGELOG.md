# Changelog

What each release of Joyscroller changed. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org).

## [Unreleased]

### Added

- TV: a new page that plays your downloaded likes like television, in shuffled order, on four channels: everything, YouTube, TikTok and Instagram. Like real TV, a channel keeps going whether you watch or not, so you join a show partway through, and flipping away and back finds it further along. Nothing repeats until everything on the channel has aired, the same creator doesn't come up twice in a row, and every show plays at the same volume, however loud or quiet it was posted. Videos over 10 minutes air a 10-minute stretch, and the guide beside the set links to each show's page.
- It plays on an early-90s TV set in the clock radio's purple and lime, with rabbit ears, and the picture looks the way broadcast TV did on a tube: soft color, scanlines, a curved screen that crops the edges and glare on the glass. The set's buttons work: POWER turns it on (with the degaussing wobble and hum) and off (the picture shrinking to a dot), the channel buttons tune through a burst of snow, the knob turns the volume, and its display shows the channel. Changing channels also works from the remote beside it, the ↑ and ↓ keys or 1–4; + and − turn the volume, M mutes, P switches it on and off, F fills the screen.
- Every few minutes the station takes a break: short Joyscroller TV bumpers, including an "Up next" card with the name of the show that's coming on. Shows fade to black and back between each other, as stations did.

### Fixed

- A download could occasionally fail to be recorded with "database is locked" when something else wrote to the archive at the same moment. It now waits its turn.
- Stopping Joyscroller with Ctrl-C, or by closing its terminal or logging out, now also stops the downloads, conversions and hidden browser windows it had running, and deletes the hidden browsers' temporary profiles, which hold a copy of your logins. Before, they could keep running in the background after Joyscroller was gone.
- After each YouTube sync, a leftover yt-dlp no longer keeps running in the background for most of a minute.

## [0.10.0] - 2026-10-15

### Added

- The clock radio at the top of the side panel comes alive when you point at it: its tube switches on with electricity crackling through it, the red alarm button bounces and the tuning knob spins. Move away and it winds down. It's also a link to all your likes. If your system is set to reduce motion, the tube just glows.
- The taskbar's tray shows how things are going at a glance: a light that's green once everything has synced, blinks while a sync runs and turns red when one fails, and a download arrow that lights up while videos download and is grey otherwise. Hover over them, in Windows XP's yellow tooltips, for each platform's sync and how many videos are downloading. Hovering over the clock shows the date.

### Changed

- Turn off now asks in a Windows XP–style Turn off box, with the screen going grey behind it, instead of the browser's plain confirm box. Cancel, Esc or a click outside the box keeps Joyscroller running.
- The start button wears the heart badge.

### Fixed

- The taskbar's clock and Turn off button sit flush together, with no stray strip of blue between them. Turn off's power symbol shows on every system and no longer runs into the word.
- The taskbar's window button shows its full title when you hover over it, for titles too long to fit.

## [0.9.0] - 2026-10-15

### Added

- The sidebar lists the kinds of post under TikTok (videos and slideshows) and Instagram (videos, photos and carousels), each with its count, so you can browse just your slideshows, say. Search and sorting work within them.

## [0.8.2] - 2026-10-15

### Fixed

- Opening more than a handful of Joyscroller tabs no longer leaves the newest ones stuck loading. Each tab kept a live connection to Joyscroller open, and browsers allow only six per site; now only the tab you're looking at keeps one, and a tab you come back to catches up.

## [0.8.1] - 2026-10-15

### Changed

- A TikTok video that TikTok no longer shows ("Video currently unavailable") now says so on its page, instead of a scary "Your IP address is blocked" error, and Joyscroller stops trying to download it. It stays in your library with its link and title. Videos that already ran into this are marked when you update.
- The first full read of your Instagram likes goes more slowly, a page every 5 to 10 seconds, so Instagram is less likely to block it partway. One that Instagram does block now carries on where it stopped however long the block lasts, instead of starting over from your newest like after a day.

## [0.8.0] - 2026-10-15

### Added

- Joyscroller now also keeps the posts you like on Instagram, not only the ones you save.
- Instagram photos and carousels you save or like are kept too, not just videos: every picture at the size it was posted, and the song that plays over it. They play as a slideshow with their music, like TikTok's photo posts, and a carousel's videos play as you reach them. Photos you saved before this version are picked up on the first sync after you update. ([#14](https://github.com/sergiotapia/joyscroller/issues/14))

### Changed

- YouTube, TikTok and Instagram sync at the same time instead of one after another, so a long first sync of one doesn't hold up the others.
- Your TikTok and Instagram likes show up in the library as each page of them is read, instead of only once a sync has read them all. A sync that's stopped partway, by turning Joyscroller off or a lost connection, carries on where it left off the next time.
- When Instagram asks Joyscroller to slow down (its "Try Again Later" block), Joyscroller leaves that list alone for a few hours instead of asking again every 5 minutes, which only makes the block last longer. Your other Instagram list keeps syncing meanwhile, and the sidebar says when it'll try again.
- The sidebar explains a failed sync in plain words, and what to do about it where there's something to do (like logging back in), instead of showing the raw error. The error is still there when you hover over it.

### Fixed

- A tall video, like most of TikTok's, no longer shows a white gap between it and the player's purple frame; the screen stays dark on either side of it.

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

[Unreleased]: https://github.com/sergiotapia/joyscroller/compare/v0.10.0...HEAD
[0.10.0]: https://github.com/sergiotapia/joyscroller/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/sergiotapia/joyscroller/compare/v0.8.2...v0.9.0
[0.8.2]: https://github.com/sergiotapia/joyscroller/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/sergiotapia/joyscroller/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/sergiotapia/joyscroller/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/sergiotapia/joyscroller/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/sergiotapia/joyscroller/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/sergiotapia/joyscroller/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/sergiotapia/joyscroller/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/sergiotapia/joyscroller/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/sergiotapia/joyscroller/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sergiotapia/joyscroller/releases/tag/v0.1.0
