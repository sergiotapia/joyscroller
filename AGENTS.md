# AGENTS.md

Notes for coding agents working in this repo. Read this, then `CONTRIBUTING.md`.

## What this is

A Rust app that archives the videos a user has liked. Plain `joyscroller` starts one long-running process that syncs YouTube and TikTok likes and Instagram saves (the bookmark, Instagram's way of keeping a post) into SQLite right away and then every 5 minutes. It also serves a web UI at http://127.0.0.1:4774 for browsing the archive, where newly synced likes appear live over Server-Sent Events. The list sorts by any column (the sort lives in the URL) and has a search box with magnetissimo's operators (`query.rs`), answered by two FTS5 indexes over each video's title, author and description (`db/search.rs`). `download.rs` saves a liked video's media at the best quality available into `downloads/<platform>/<YYYY-MM-DD>/<title> [<video id>]/`, by the local date it was liked (so the first sync's day holds the whole backlog). `filename.rs` makes titles safe as folder names on every OS. That folder, the database (`joyscroller.db`) and `tools/` sit next to the app's executable, where people will look for them. A binary running from this repo's `target/` (so `cargo run`) uses `data/` at the repo root instead, which survives `cargo clean` and is gitignored. The macOS app (`Joyscroller.app`) can't hold them, so it keeps downloads in `~/Movies/Joyscroller` and everything else, its log included, in `~/Library/Application Support/Joyscroller` (`desktop.rs`). On Windows and in the macOS app, launching opens the UI in the browser; launching while it's already running opens the running copy's UI. The taskbar's Turn off button stops the app, killing any yt-dlp or ffmpeg it started. Videos download when the user clicks "Download now" on a video's page: the download pipeline (`pipeline.rs`) starts it right away on a track of its own, several at once if asked. The pipeline also has a lane per platform that works through the whole backlog newest first, one video at a time, waiting 30 s between YouTube videos, a random 15–30 s between TikTok ones and 30–60 s between Instagram ones (yt-dlp's wiki puts YouTube's logged-in limit at ~2,000 videos/hour; TikTok and Instagram publish none, and Instagram is quick to lock out accounts that look automated). The lanes are on (`AUTOMATIC` in `pipeline.rs`) and wait for `tools.rs` to have fetched yt-dlp and ffmpeg before their first video. After each download, the pipeline's converter (`transcode.rs`) makes a playback copy, `playback.mp4` (H.264 + AAC, at the original's resolution and frame rate), for any video that isn't already H.264 and AAC in MP4, since AV1, HEVC and VP9 each fail to play somewhere (Safari without an AV1 decoder, Chromium and Firefox without a GPU HEVC decoder, a Tesla). It runs one video at a time under `nice`, newest first, and a page that can't play a video's original moves that video to the front. `/downloading` shows it all live. Once downloaded, a video plays on its page (`player.rs`): a video in a player, a TikTok photo post as a slideshow with its sound playing. With a copy, `player.js` plays the original only where MediaCapabilities says it decodes smoothly and power-efficiently, and the copy otherwise. The page loads the files from `/videos/<platform>/<id>/media/<file>`, which serves only the files the download recorded and the playback copy. The `download` subcommand downloads by hand. Every table row has a `platform` column.

**Product rules:**
- End users never run subcommands. Launching `joyscroller` must start everything. The `cookies`, `likes` and `sync` subcommands are developer and debugging tools only. New user-facing features go inside the long-running process, as a background task or in the web UI, not as new CLI flags.
- The app runs natively, not in Docker. It has to read the host browser's cookie database and the desktop keyring, which a container can't reach.
- Logging in means reading the user's browser cookies. Don't add OAuth or API-key flows.
- Users install nothing but the app. `tools.rs` fetches yt-dlp, ffmpeg/ffprobe and bun (yt-dlp needs a JavaScript runtime for YouTube; bun rather than deno, whose standalone runtime winds down after Cloudflare bought it) into `tools/` on launch and runs `yt-dlp --update` daily. yt-dlp is always our own copy, since distro packages lag behind site changes; ffmpeg and bun already on `PATH` are used as they are. Run the tools through `tools::yt_dlp()`, `tools::ffmpeg()` (`tools::ffmpeg_in_background()`, under `nice`, for long background work) and `tools::ffprobe()`, never `Command::new("yt-dlp")`.
- We don't call YouTube's API ourselves. `yt-dlp` does the scraping, and we parse its JSON.
- yt-dlp can't list TikTok likes, and TikTok rejects API requests its own page script didn't sign. So we load tiktok.com in a headless copy of the user's browser, logged in with their cookies, and call the API with `fetch` from inside the page, where TikTok's script signs it. Don't try to reimplement TikTok's request signing.
- yt-dlp can't list Instagram saves either. Instagram's saved-posts endpoint (`/api/v1/feed/saved/posts/`) needs no signing, only the cookies plus the `x-ig-app-id` and `x-csrftoken` headers, but it's called the same way, from a headless instagram.com tab. Only videos (`media_type` 2: reels and video posts) are kept; saved photos and carousels are skipped. yt-dlp downloads each one.

## Layout

```
src/main.rs     CLI (clap), the app loop (run_app), and sync_youtube / sync_tiktok, which tie the modules together
src/desktop.rs  Fitting into each desktop: the macOS app bundle's data folders and log file, opening the UI
                in the browser, and recognizing a copy that's already running
src/web.rs      The localhost UI (axum): server-rendered HTML list and video pages (with a Download button),
                /downloading, and /events, an SSE stream of newly stored videos, counts, sync status and
                the download pipeline's board. run_app feeds it sync status through a watch channel.
                Also serves downloaded files (with range requests, for seeking) to the player
src/player.rs   The video page's player markup, from Video.js 10's elements: a video player (naming its
                playback copy too, when there is one), or a photo post's slideshow over its sound, which
                autoplays and loops
src/browser.rs  Finds the browser profile, copies its Cookies SQLite DB, decrypts values
                (AES-128-CBC; Linux: v10 = fixed key, v11 = key from the keyring; macOS: v10 = key from
                the Keychain, read once per run), writes Netscape cookies.txt. `sniff` picks the browser
                logged in to the most platforms
src/youtube.rs  Runs `yt-dlp --flat-playlist --lazy-playlist --dump-json` on the Liked playlist and
                streams JSON Lines; a callback can stop the listing early
src/tiktok.rs   Pages through TikTok's liked list (/api/favorite/item_list/) from inside a headless
                tiktok.com tab and parses the items; a callback can stop the listing early. Also
                fetches a photo post's images and sound from inside its page (yt-dlp only gets the sound)
src/instagram.rs Pages through Instagram's saved list (/api/v1/feed/saved/posts/) from inside a headless
                instagram.com tab and parses the videos; a callback can stop the listing early
src/headless.rs Minimal Chrome DevTools Protocol client: launches the browser headless with a throwaway
                profile, loads cookies, navigates, evaluates JS in the page
src/tools.rs    Fetches the app's own yt-dlp, ffmpeg/ffprobe and bun from their GitHub releases (checksum-
                verified) into tools/ on Linux, macOS and Windows (macOS's ffmpeg comes from
                ffmpeg.martin-riedl.de), keeps yt-dlp updated, and builds the commands that run them
src/pipeline.rs The download pipeline: one lane per platform working through its backlog (lanes run in
                parallel, one video at a time each, paced by `pace`), an immediate track per video asked
                for in the UI, the converter that makes playback copies one at a time, and the Board of
                what's running and finished that the UI watches
src/transcode.rs Makes a downloaded video's playback copy (`playback.mp4`, H.264 + AAC, same resolution) when
                the original won't play in every browser: probes it with ffprobe, decides (none, remux or
                encode), runs ffmpeg at low priority, and describes the original for MediaCapabilities
src/download.rs Downloads one video's media into its folder: with yt-dlp (`video.<ext>`, `thumbnail.<ext>`;
                muxes TikTok's 1080p picture with its real audio; merges Instagram's DASH video and audio), or for TikTok photo posts through
                tiktok.rs (`image-NN.<ext>`, `sound.<ext>`); returns trimmed metadata
src/filename.rs safe_name: text to a file/folder name valid on Linux, macOS and Windows (forbidden characters,
                reserved device names, trailing dots, invisible characters, byte-length limits)
src/db.rs       The archive database: `open` (backup, migrations) and re-exports of the modules below, which
                hold every query; the only code that touches the archive DB
src/db/migrate.rs   Brings a database up to date from the SQL files in src/db/migrations/ (0000_schema.sql,
                then one numbered file per step), with a backup first; refuses a newer database
src/db/videos.rs    Storing synced likes (insert-only) and reading them back
src/db/syncs.rs     The log of sync runs
src/db/downloads.rs The download queue (with retry back-off) and what each download saved
src/db/playback.rs  The playback-copy queue and what each try made
src/db/list.rs      The UI's pages: platform filter, search, and the column sorts with their indexes
src/db/search.rs    Turns a parsed search into SQL against the FTS5 indexes (tests in db/search/tests.rs)
src/db/test_support.rs  Fixtures the db tests share (in-memory database, sample rows, a search helper)
src/query.rs    Parses what's typed in the search box: words, "quoted text", -exclusions, a|b alternatives,
                prefix* and *patterns (magnetissimo's operators); splits text into tokens the way unicode61 does
assets/         The UI's look, built into the binary by web.rs (`ASSETS`) and served at /assets/<file>:
                style.css (Windows XP desktop + Nickelodeon clock radio), lab.webp (the page background), splat.svg (favicon),
                player.css (the player's skin) and player.js (the slideshow, and picking the original or
                the playback copy), videojs.js (Video.js 10, bundled
                by `just videojs` from the versions pinned in videojs/; don't edit it by hand),
                radio.webp (rendered by blender/radio.py; the page overlays a live clock on its LCD),
                splat.webp and icon-*.webp (rendered by blender/badges.py in the radio's materials),
                the video page's hardware (rendered by blender/player.py: the Download now dome,
                the player's casing, buttons, slot, thumbs, LCD and LEDs, mostly nine-slice images
                and up/pressed sprites whose sizes player.css follows),
                and fonts/ (Luckiest Guy, Fredoka, DSEG LCD digits, with their licenses)
```

Config: `Cargo.toml` (lint levels), `clippy.toml`, `deny.toml`, `justfile`, `rust-toolchain.toml` (the Rust release every build uses, locally and in CI; upgrade it on purpose and fix what new lints find).

CI (`.github/workflows/`):
- `ci.yml`: clippy and the tests on Linux, macOS and Windows for every push to master, every pull request and every release; fmt, machete, deny and antislop once.
- `package.yml`: builds every download (Linux tarballs for x86_64 and aarch64, a macOS DMG per chip holding `Joyscroller.app`) and smoke-tests each on a machine of its kind with `packaging/smoke.sh`. Pull requests run it, so a PR's run has builds to try. The Mac app is never signed with a Developer ID or notarized: `packaging/macos/make-app.sh` only gives it the free ad-hoc signature Apple Silicon needs.
- `release.yml`: when a `v*` tag matching Cargo.toml's version is pushed, runs `ci.yml`, then `package.yml`, and publishes a GitHub Release with the files, a `SHA256SUMS`, the CHANGELOG section and `packaging/install-notes.md`.

## Commands

```sh
just precommit                    # must pass before every commit (fmt, machete, clippy, deny, antislop, nextest)
just lint / just test             # run a single step
just videojs                      # rebuild assets/videojs.js after changing assets/videojs/ (needs Node)

cargo run                         # the app: UI on http://127.0.0.1:4774, syncs into the real DB every 5 min
cp target/debug/joyscroller /tmp/x/ && /tmp/x/joyscroller   # the app on a throwaway DB and downloads folder
cargo run -- likes --limit 5      # print recent likes (reads real browser cookies, no DB)
cargo run -- likes --platform tiktok --limit 5
cargo run -- likes --platform instagram --limit 5   # Instagram's saved videos
cargo run -- sync --db /tmp/t.db  # one sync into a throwaway DB (add --platform tiktok for TikTok)
cargo run -- sync --full          # read the whole liked list, not just until known videos
cargo run -- download --db /tmp/t.db --media /tmp/m --limit 3   # download the next 3 queued videos
cargo run -- cookies --out c.txt  # export cookies.txt (contains live credentials)
cargo run -- tools                # fetch the app's yt-dlp, ffmpeg, ffprobe and bun and print their versions
```

Runtime needs: Linux or macOS (Windows isn't released yet: the tools and desktop fit work there and CI tests it, but reading browser logins doesn't, see #2), a Chromium-family browser (Chrome, Edge, Opera, Brave, Opera GX, Arc, Chromium or Vivaldi; Opera GX and Arc are macOS-only here) whose `Default` profile is signed in to YouTube, TikTok and Instagram. On each launch `browser::sniff` picks the installed browser logged in to the most of the three, most widely used first on a tie, from cookie names alone (no decryption, so no keyring or Keychain prompt). The subcommands do the same unless given `--browser`, and that browser's executable (TikTok and Instagram run it headless; Arc can't run headless, so for Arc another installed one runs instead, see `Browser::headless_executable`): on Linux on `PATH`, on macOS in `/Applications` or `~/Applications`. On Linux, v11 cookies need a Secret Service keyring (gnome-keyring or KWallet). On macOS, the first cookie read of each run asks the user to let Joyscroller use the browser's "Safe Storage" Keychain item, unless they clicked Always Allow.

A full TikTok listing takes about 2 minutes for 5,000 likes (one request per 30). In testing, TikTok's liked list ended after exactly 200 pages (about 5,100 likes, back to January 2026). That looks like a server-side cap rather than the true end of the list.

The real archive is `joyscroller.db` and `downloads/` in `data/` at the repo root (for `cargo run`). Don't write to it while experimenting. Pass `--db` (and `--media`) with a temp path, or run a copy of the binary from a temp folder.

## Data model invariants

- `videos` is keyed by `(platform, video_id)`. A row existing means the video is liked.
- Rows are insert-only. A row keeps what the video looked like when first seen. Only `available` is updated on later syncs, so a video that later goes private keeps its title on file. Don't overwrite other columns.
- Insert oldest first, so rowid order matches like order. Both platforms list newest first, so each sync reverses the list.
- `videos.listing_json` holds the full entry from the liked/saved list (yt-dlp's for YouTube, the API item for TikTok and Instagram), so fields we don't parse yet aren't lost. For TikTok, every TikTok-hosted URL and the per-quality format lists are stripped first (`without_media_links` in `tiktok.rs`): they expire within about two days and the media is downloaded separately, and dropping them takes an item from ~23 KB to ~5 KB. Instagram's items get the same treatment (`without_media_links` in `instagram.rs`: CDN URLs, format lists and DASH manifests), ~45 KB down to ~13 KB.
- Instagram rows are keyed by the post's shortcode (`instagram.com/reel/<code>/`), not its numeric `pk`. Like TikTok, they're always `available = 1`, `title` is the caption (NULL when empty), and `author_name` is the @handle.
- TikTok leaves deleted and private videos out of the liked list instead of listing them stripped down, so TikTok rows are always stored with `available = 1`. `title` holds the caption, which is often empty (stored as NULL).
- `syncs` logs each run. `finished_at IS NULL` means the run crashed or is still going.
- An incremental sync stops after `STOP_AFTER_KNOWN` (100) already-stored likes in a row. Stopping at a single known video would be wrong, because re-liking an old video moves it to the top of the list.
- Schema changes are migrations: add the next numbered SQL file in `src/db/migrations/` and append it to `MIGRATIONS` in `db/migrate.rs`. Never edit `0000_schema.sql` (the original tables) or a step that has shipped. `PRAGMA user_version` counts the steps a database has had, and `open` applies the rest. Before applying any, `open` saves a copy of the database in `backups/` beside it (`VACUUM INTO`), and it refuses a database whose `user_version` is past the steps it knows (made by a newer Joyscroller). Naming: a timestamp column ends in `_at` (unix seconds), a JSON column in `_json`, a size in `_bytes`, and failed tries are `failures`. `videos.added_at` is when a sync first found the like; the platforms don't say when it was liked.
- `kind` is `video`, or `photo` for a TikTok slideshow. `posted_at` is NULL until known: TikTok rows get it at sync, YouTube rows when their media is downloaded. It may be filled in once, never overwritten.
- `downloads` has a row once a video's download has been tried. `downloaded_at IS NULL` means not downloaded yet. A failed try counts in `failures`, and the video is retried after 1h, 2h, 4h and so on, capped at a week (`MAX_RETRY_DELAY`). Videos are never given up on, since most failures (a broken yt-dlp, an expired login, an outage) aren't the video's fault. The queue (`pending_downloads`) is newest-stored first and skips unavailable videos.
- Search reads `search_text` (title, author, description; one row per video under its rowid), which triggers fill from `videos` and `downloads` through the `search_text_source` view, and the FTS5 tables `search_trigrams` and `search_words` (unicode61) index it as external content. To search more fields, add a migration that replaces the view and refills `search_text`. The FTS5 tables name `search_text` in their own definition, so renaming it (or anything they read) means dropping and recreating them, as migration 4 does. Each column sort has two indexes, and `order_by` in `db/list.rs` must match them term for term (a test checks the query plans).
- `downloads.metadata_json` is yt-dlp's metadata (TikTok's item for photo posts) minus `INFO_DROP` in `download.rs`. That covers format lists, URLs and per-stream fields, and `cookies`, which yt-dlp fills with the user's login. Never store that field.
- `playback_copies` has a row once a downloaded video's playback copy has been tried, for the download in its `download_folder`. `finished_at IS NULL` means not done yet, and failures back off like downloads. `file_name` is NULL when the original already plays everywhere (H.264 and AAC/MP3 in MP4). A new download of the video deletes the row, a copy of an earlier download that finishes afterwards isn't recorded, and the copy is only served while its `download_folder` matches the download's `folder`. The copy never changes the resolution; the only exception is cropping an odd width or height by one pixel, which 4:2:0 H.264 requires. HDR is tone-mapped to SDR.
- "Best quality" means one file per video at the highest quality it has: no second copies at lower qualities. The playback copy is the one exception: it's there for compatibility, at the original's resolution, and the original is never replaced. TikTok's 1080p streams have no audio, and its audio-only format is the post's *sound* (the song), not the video's audio. So `download.rs` fetches the best video and the best combined file and muxes the picture of one with the audio of the other. yt-dlp can't do this itself: with `bv*+b` it silently drops the second file.

## Code conventions

- Errors: functions return `anyhow::Result` and add context with `.context(...)` / `.with_context(...)`. Don't use `unwrap`/`expect` outside tests (clippy denies them). Don't swallow errors with `map_err(|_| ...)`.
- A failed sync in the app loop is logged and retried on the next tick. It must never kill the process.
- Logging is `eprintln!` to stderr, prefixed by area (`youtube: ...`, `warning: ...`). There's no logging framework yet. stdout is for command output.
- Cookie values are credentials. Never log or print them, except in the `cookies` command's output, and never commit a cookies file.
- Comments explain *why* (format quirks, ordering constraints, yt-dlp behavior). Every module has a `//!` doc comment saying what it's for. Keep those up to date when a module's job changes.
- Tests go in a `#[cfg(test)] mod tests` at the bottom of the file they test; a module whose tests outgrow it keeps them in `<module>/tests.rs` (as `db/search.rs` does). DB tests use `memory_db()` from `db/test_support.rs`, an in-memory database with `init` (schema plus migrations). A module with submodules is `foo.rs` plus `foo/`, never `foo/mod.rs` (clippy's `mod_module_files`). Tests must not touch the network, the real browser, the keyring or the real DB.
- Clippy runs pedantic with warnings as errors. Fix the code rather than silencing the lint. If an exception really is right, use a narrow `#[expect(clippy::lint, reason = "...")]` on that item.

## Git

- Commit straight to `master`. Never create branches. The remote is `origin` on GitHub; push and tag only when asked.
- Run `just precommit` before committing. Don't commit if it fails.
- Commit message: an imperative subject line, then a body explaining what changed and why.
- A change users would notice gets a line under `## [Unreleased]` in `CHANGELOG.md`, in the same commit, written for users rather than developers. To release, rename that section to the new version and date, add a fresh `[Unreleased]` above it and fix the compare links at the bottom, bump `version` in Cargo.toml, commit, then push a `v<version>` tag. The release workflow uses that CHANGELOG section (plus its compare link) as the GitHub Release's description and fails without it; check the published release has it.
