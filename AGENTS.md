# AGENTS.md

Notes for coding agents working in this repo. Read this, then `CONTRIBUTING.md`.

## What this is

A Rust app that archives the videos a user has liked. Plain `joyscroller` starts one long-running process that syncs YouTube and TikTok likes and Instagram saves (the bookmark, Instagram's way of keeping a post) into SQLite right away and then every 5 minutes. It also serves a web UI at http://127.0.0.1:4774 for browsing the archive, where newly synced likes appear live over Server-Sent Events. The list sorts by any column (the sort lives in the URL) and has a search box with magnetissimo's operators (`query.rs`), answered by two FTS5 indexes over each video's title, author and description (`db.rs`). `download.rs` saves a liked video's media at the best quality available into `downloads/<platform>/<YYYY-MM-DD>/<title> [<video id>]/`, by the local date it was liked (so the first sync's day holds the whole backlog). `filename.rs` makes titles safe as folder names on every OS. That folder, the database (`joyscroller.db`) and `tools/` sit next to the app's executable, where people will look for them. A binary running from this repo's `target/` (so `cargo run`) uses `data/` at the repo root instead, which survives `cargo clean` and is gitignored. Videos download when the user clicks "Download now" on a video's page: the download pipeline (`pipeline.rs`) starts it right away on a track of its own, several at once if asked. The pipeline also has a lane per platform that works through the whole backlog newest first, one video at a time, waiting 30 s between YouTube videos, a random 15–30 s between TikTok ones and 30–60 s between Instagram ones (yt-dlp's wiki puts YouTube's logged-in limit at ~2,000 videos/hour; TikTok and Instagram publish none, and Instagram is quick to lock out accounts that look automated). The lanes are on (`AUTOMATIC` in `pipeline.rs`) and wait for `tools.rs` to have fetched yt-dlp and ffmpeg before their first video. `/downloading` shows it all live. Once downloaded, a video plays on its page (`player.rs`): a video in a player, a TikTok photo post as a slideshow with its sound playing. The page loads the files from `/videos/<platform>/<id>/media/<file>`, which serves only the files the download recorded. The `download` subcommand downloads by hand. Every table row has a `platform` column.

**Product rules:**
- End users never run subcommands. Launching `joyscroller` must start everything. The `cookies`, `likes` and `sync` subcommands are developer and debugging tools only. New user-facing features go inside the long-running process, as a background task or in the web UI, not as new CLI flags.
- The app runs natively, not in Docker. It has to read the host browser's cookie database and the desktop keyring, which a container can't reach.
- Logging in means reading the user's browser cookies. Don't add OAuth or API-key flows.
- Users install nothing but the app. `tools.rs` fetches yt-dlp, ffmpeg/ffprobe and bun (yt-dlp needs a JavaScript runtime for YouTube; bun rather than deno, whose standalone runtime winds down after Cloudflare bought it) into `tools/` on launch and runs `yt-dlp --update` daily. yt-dlp is always our own copy, since distro packages lag behind site changes; ffmpeg and bun already on `PATH` are used as they are. Run the tools through `tools::yt_dlp()`, `tools::ffmpeg()` and `tools::ffprobe()`, never `Command::new("yt-dlp")`.
- We don't call YouTube's API ourselves. `yt-dlp` does the scraping, and we parse its JSON.
- yt-dlp can't list TikTok likes, and TikTok rejects API requests its own page script didn't sign. So we load tiktok.com in a headless copy of the user's browser, logged in with their cookies, and call the API with `fetch` from inside the page, where TikTok's script signs it. Don't try to reimplement TikTok's request signing.
- yt-dlp can't list Instagram saves either. Instagram's saved-posts endpoint (`/api/v1/feed/saved/posts/`) needs no signing, only the cookies plus the `x-ig-app-id` and `x-csrftoken` headers, but it's called the same way, from a headless instagram.com tab. Only videos (`media_type` 2: reels and video posts) are kept; saved photos and carousels are skipped. yt-dlp downloads each one.

## Layout

```
src/main.rs     CLI (clap), the app loop (run_app), and sync_youtube / sync_tiktok, which tie the modules together
src/web.rs      The localhost UI (axum): server-rendered HTML list and video pages (with a Download button),
                /downloading, and /events, an SSE stream of newly stored videos, counts, sync status and
                the download pipeline's board. run_app feeds it sync status through a watch channel.
                Also serves downloaded files (with range requests, for seeking) to the player
src/player.rs   The video page's player markup, from Video.js 10's elements: a video player, or a photo
                post's slideshow over its sound, which autoplays and loops
src/browser.rs  Finds the browser profile, copies its Cookies SQLite DB, decrypts values
                (AES-128-CBC; v10 = fixed key, v11 = key from the keyring), writes Netscape cookies.txt
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
                verified) into tools/, keeps yt-dlp updated, and builds the commands that run them
src/pipeline.rs The download pipeline: one lane per platform working through its backlog (lanes run in
                parallel, one video at a time each, paced by `pace`), an immediate track per video asked
                for in the UI, and the Board of what's running and finished that the UI watches
src/download.rs Downloads one video's media into its folder: with yt-dlp (`video.<ext>`, `thumbnail.<ext>`;
                muxes TikTok's 1080p picture with its real audio; merges Instagram's DASH video and audio), or for TikTok photo posts through
                tiktok.rs (`image-NN.<ext>`, `sound.<ext>`); returns trimmed metadata
src/filename.rs safe_name: text to a file/folder name valid on Linux, macOS and Windows (forbidden characters,
                reserved device names, trailing dots, invisible characters, byte-length limits)
src/db.rs       SQLite schema (the SCHEMA const) and every query; the only module that touches the archive DB.
                Also turns a parsed search into SQL against the FTS5 indexes, and orders the list
src/query.rs    Parses what's typed in the search box: words, "quoted text", -exclusions, a|b alternatives,
                prefix* and *patterns (magnetissimo's operators); splits text into tokens the way unicode61 does
assets/         The UI's look, built into the binary by web.rs (`ASSETS`) and served at /assets/<file>:
                style.css (Windows XP desktop + Nickelodeon clock radio), lab.webp (the page background), splat.svg (favicon),
                player.css (the player's skin) and player.js (the slideshow), videojs.js (Video.js 10, bundled
                by `just videojs` from the versions pinned in videojs/; don't edit it by hand),
                radio.webp (rendered by blender/radio.py; the page overlays a live clock on its LCD),
                splat.webp and icon-*.webp (rendered by blender/badges.py in the radio's materials),
                the video page's hardware (rendered by blender/player.py: the Download now dome,
                the player's casing, buttons, slot, thumbs, LCD and LEDs, mostly nine-slice images
                and up/pressed sprites whose sizes player.css follows),
                and fonts/ (Luckiest Guy, Fredoka, DSEG LCD digits, with their licenses)
```

Config: `Cargo.toml` (lint levels), `clippy.toml`, `deny.toml`, `justfile`. `.github/workflows/release.yml` builds Linux x86_64 and aarch64 binaries and publishes a GitHub Release when a `v*` tag matching Cargo.toml's version is pushed.

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
```

Runtime needs: Linux (x86_64 or aarch64 for the fetched tools; elsewhere `yt-dlp`, `ffmpeg`, `ffprobe` and `bun` on `PATH`), a Chromium-family browser profile signed in to YouTube, TikTok and Instagram (default `--browser brave --profile Default`), that browser's executable on `PATH` (TikTok and Instagram run it headless), and a Secret Service keyring (gnome-keyring or KWallet) for v11 cookies.

A full TikTok listing takes about 2 minutes for 5,000 likes (one request per 30). In testing, TikTok's liked list ended after exactly 200 pages (about 5,100 likes, back to January 2026). That looks like a server-side cap rather than the true end of the list.

The real archive is `joyscroller.db` and `downloads/` in `data/` at the repo root (for `cargo run`). Don't write to it while experimenting. Pass `--db` (and `--media`) with a temp path, or run a copy of the binary from a temp folder.

## Data model invariants

- `videos` is keyed by `(platform, video_id)`. A row existing means the video is liked.
- Rows are insert-only. A row keeps what the video looked like when first seen. Only `available` is updated on later syncs, so a video that later goes private keeps its title on file. Don't overwrite other columns.
- Insert oldest first, so rowid order matches like order. Both platforms list newest first, so each sync reverses the list.
- `raw_json` holds the full entry (yt-dlp's for YouTube, the API item for TikTok and Instagram), so fields we don't parse yet aren't lost. For TikTok, every TikTok-hosted URL and the per-quality format lists are stripped first (`without_media_links` in `tiktok.rs`): they expire within about two days and the media is downloaded separately, and dropping them takes an item from ~23 KB to ~5 KB. Instagram's items get the same treatment (`without_media_links` in `instagram.rs`: CDN URLs, format lists and DASH manifests), ~45 KB down to ~13 KB.
- Instagram rows are keyed by the post's shortcode (`instagram.com/reel/<code>/`), not its numeric `pk`. Like TikTok, they're always `available = 1`, `title` is the caption (NULL when empty), and `author_name` is the @handle.
- TikTok leaves deleted and private videos out of the liked list instead of listing them stripped down, so TikTok rows are always stored with `available = 1`. `title` holds the caption, which is often empty (stored as NULL).
- `syncs` logs each run. `finished_at IS NULL` means the run crashed or is still going.
- An incremental sync stops after `STOP_AFTER_KNOWN` (100) already-stored likes in a row. Stopping at a single known video would be wrong, because re-liking an old video moves it to the top of the list.
- Schema changes are migrations: append a step to `MIGRATIONS` in `db.rs`. Never edit `SCHEMA` (the original tables) or a step that has shipped. `PRAGMA user_version` counts the steps a database has had, and `open` applies the rest.
- `kind` is `video`, or `photo` for a TikTok slideshow. `posted_at` is NULL until known: TikTok rows get it at sync, YouTube rows when their media is downloaded. It may be filled in once, never overwritten.
- `downloads` has a row once a video's download has been tried. `downloaded_at IS NULL` means not downloaded yet. A failed try counts in `attempts`, and the video is retried after 1h, 2h, 4h and so on, capped at a week (`MAX_RETRY_DELAY`). Videos are never given up on, since most failures (a broken yt-dlp, an expired login, an outage) aren't the video's fault. The queue (`pending_downloads`) is newest-stored first and skips unavailable videos.
- Search reads `video_text` (title, author, description; one row per video under its rowid), which triggers fill from `videos` and `downloads` through the `video_text_source` view, and the FTS5 tables `video_search` (trigram) and `video_words` (unicode61) index it as external content. To search more fields, add a migration that replaces the view and refills `video_text`. Each column sort has two indexes, and `order_by` in `db.rs` must match them term for term (a test checks the query plans).
- `downloads.info_json` is yt-dlp's metadata minus `INFO_DROP` in `download.rs`. That covers format lists, URLs and per-stream fields, and `cookies`, which yt-dlp fills with the user's login. Never store that field.
- "Best quality" means one file per video at the highest quality it has: no second copies at lower qualities. TikTok's 1080p streams have no audio, and its audio-only format is the post's *sound* (the song), not the video's audio. So `download.rs` fetches the best video and the best combined file and muxes the picture of one with the audio of the other. yt-dlp can't do this itself: with `bv*+b` it silently drops the second file.

## Code conventions

- Errors: functions return `anyhow::Result` and add context with `.context(...)` / `.with_context(...)`. Don't use `unwrap`/`expect` outside tests (clippy denies them). Don't swallow errors with `map_err(|_| ...)`.
- A failed sync in the app loop is logged and retried on the next tick. It must never kill the process.
- Logging is `eprintln!` to stderr, prefixed by area (`youtube: ...`, `warning: ...`). There's no logging framework yet. stdout is for command output.
- Cookie values are credentials. Never log or print them, except in the `cookies` command's output, and never commit a cookies file.
- Comments explain *why* (format quirks, ordering constraints, yt-dlp behavior). Every module has a `//!` doc comment saying what it's for. Keep those up to date when a module's job changes.
- Tests go in a `#[cfg(test)] mod tests` at the bottom of the file they test. DB tests use `Connection::open_in_memory()` with `init` (schema plus migrations). Tests must not touch the network, the real browser, the keyring or the real DB.
- Clippy runs pedantic with warnings as errors. Fix the code rather than silencing the lint. If an exception really is right, use a narrow `#[expect(clippy::lint, reason = "...")]` on that item.

## Git

- Commit straight to `master`. Never create branches. The remote is `origin` on GitHub; push and tag only when asked.
- Run `just precommit` before committing. Don't commit if it fails.
- Commit message: an imperative subject line, then a body explaining what changed and why.
