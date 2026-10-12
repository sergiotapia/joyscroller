# AGENTS.md

Notes for coding agents working in this repo. Read this, then `CONTRIBUTING.md`.

## What this is

A Rust app that archives the videos a user has liked. Plain `joyscroller` starts one long-running process that syncs YouTube and TikTok likes into SQLite right away and then every 5 minutes. It also serves a web UI at http://127.0.0.1:4774 for browsing the archive, where newly synced likes appear live over Server-Sent Events. `download.rs` saves a liked video's media at the best quality available into `downloads/<platform>/<YYYY-MM-DD>/<title> [<video id>]/`, by the local date it was liked (so the first sync's day holds the whole backlog). `filename.rs` makes titles safe as folder names on every OS. That folder and the database, `joyscroller.db`, sit next to the app's executable, where people will look for them. A binary running from this repo's `target/` (so `cargo run`) uses `data/` at the repo root instead, which survives `cargo clean` and is gitignored. Videos download only when the user asks on a video's page: the request goes into the download pipeline (`pipeline.rs`), and `/downloading` shows its progress live. Automatic downloads of the whole queue (`download_loop`) stay off for now. The `download` subcommand still runs them by hand. Instagram comes later. Every table row has a `platform` column.

**Product rules:**
- End users never run subcommands. Launching `joyscroller` must start everything. The `cookies`, `likes` and `sync` subcommands are developer and debugging tools only. New user-facing features go inside the long-running process, as a background task or in the web UI, not as new CLI flags.
- The app runs natively, not in Docker. It has to read the host browser's cookie database and the desktop keyring, which a container can't reach.
- Logging in means reading the user's browser cookies. Don't add OAuth or API-key flows.
- We don't call YouTube's API ourselves. `yt-dlp` does the scraping, and we parse its JSON.
- yt-dlp can't list TikTok likes, and TikTok rejects API requests its own page script didn't sign. So we load tiktok.com in a headless copy of the user's browser, logged in with their cookies, and call the API with `fetch` from inside the page, where TikTok's script signs it. Don't try to reimplement TikTok's request signing.

## Layout

```
src/main.rs     CLI (clap), the app loop (run_app), and sync_youtube / sync_tiktok, which tie the modules together
src/web.rs      The localhost UI (axum): server-rendered HTML list and video pages (with a Download button),
                /downloading, and /events, an SSE stream of newly stored videos, counts, sync status and
                the download pipeline's board. run_app feeds it sync status through a watch channel
src/browser.rs  Finds the browser profile, copies its Cookies SQLite DB, decrypts values
                (AES-128-CBC; v10 = fixed key, v11 = key from the keyring), writes Netscape cookies.txt
src/youtube.rs  Runs `yt-dlp --flat-playlist --lazy-playlist --dump-json` on the Liked playlist and
                streams JSON Lines; a callback can stop the listing early
src/tiktok.rs   Pages through TikTok's liked list (/api/favorite/item_list/) from inside a headless
                tiktok.com tab and parses the items; a callback can stop the listing early. Also
                fetches a photo post's images and sound from inside its page (yt-dlp only gets the sound)
src/headless.rs Minimal Chrome DevTools Protocol client: launches the browser headless with a throwaway
                profile, loads cookies, navigates, evaluates JS in the page
src/pipeline.rs The download pipeline: one lane per platform (lanes run in parallel, one video at a time
                each), a stage enum with only Download for now, and the Board of what's queued, running
                and finished that the UI watches. Requests are in memory only
src/download.rs Downloads one video's media into its folder: with yt-dlp (`video.<ext>`, `thumbnail.<ext>`;
                muxes TikTok's 1080p picture with its real audio), or for TikTok photo posts through
                tiktok.rs (`image-NN.<ext>`, `sound.<ext>`); returns trimmed metadata
src/filename.rs safe_name: text to a file/folder name valid on Linux, macOS and Windows (forbidden characters,
                reserved device names, trailing dots, invisible characters, byte-length limits)
src/db.rs       SQLite schema (the SCHEMA const) and every query; the only module that touches the archive DB
```

Config: `Cargo.toml` (lint levels), `clippy.toml`, `deny.toml`, `justfile`.

## Commands

```sh
just precommit                    # must pass before every commit (fmt, machete, clippy, deny, antislop, nextest)
just lint / just test             # run a single step

cargo run                         # the app: UI on http://127.0.0.1:4774, syncs into the real DB every 5 min
cp target/debug/joyscroller /tmp/x/ && /tmp/x/joyscroller   # the app on a throwaway DB and downloads folder
cargo run -- likes --limit 5      # print recent likes (reads real browser cookies, no DB)
cargo run -- likes --platform tiktok --limit 5
cargo run -- sync --db /tmp/t.db  # one sync into a throwaway DB (add --platform tiktok for TikTok)
cargo run -- sync --full          # read the whole liked list, not just until known videos
cargo run -- download --db /tmp/t.db --media /tmp/m --limit 3   # download the next 3 queued videos
cargo run -- cookies --out c.txt  # export cookies.txt (contains live credentials)
```

Runtime needs: Linux, `yt-dlp` and `ffmpeg` (with `ffprobe`) on `PATH`, a Chromium-family browser profile signed in to YouTube and TikTok (default `--browser brave --profile Default`), that browser's executable on `PATH` (TikTok runs it headless), and a Secret Service keyring (gnome-keyring or KWallet) for v11 cookies.

A full TikTok listing takes about 2 minutes for 5,000 likes (one request per 30). In testing, TikTok's liked list ended after exactly 200 pages (about 5,100 likes, back to January 2026). That looks like a server-side cap rather than the true end of the list.

The real archive is `joyscroller.db` and `downloads/` in `data/` at the repo root (for `cargo run`). Don't write to it while experimenting. Pass `--db` (and `--media`) with a temp path, or run a copy of the binary from a temp folder.

## Data model invariants

- `videos` is keyed by `(platform, video_id)`. A row existing means the video is liked.
- Rows are insert-only. A row keeps what the video looked like when first seen. Only `available` is updated on later syncs, so a video that later goes private keeps its title on file. Don't overwrite other columns.
- Insert oldest first, so rowid order matches like order. Both platforms list newest first, so each sync reverses the list.
- `raw_json` holds the full entry (yt-dlp's for YouTube, TikTok's API item for TikTok), so fields we don't parse yet aren't lost. For TikTok, every TikTok-hosted URL and the per-quality format lists are stripped first (`without_media_links` in `tiktok.rs`): they expire within about two days and the media is downloaded separately, and dropping them takes an item from ~23 KB to ~5 KB.
- TikTok leaves deleted and private videos out of the liked list instead of listing them stripped down, so TikTok rows are always stored with `available = 1`. `title` holds the caption, which is often empty (stored as NULL).
- `syncs` logs each run. `finished_at IS NULL` means the run crashed or is still going.
- An incremental sync stops after `STOP_AFTER_KNOWN` (100) already-stored likes in a row. Stopping at a single known video would be wrong, because re-liking an old video moves it to the top of the list.
- Schema changes are migrations: append a step to `MIGRATIONS` in `db.rs`. Never edit `SCHEMA` (the original tables) or a step that has shipped. `PRAGMA user_version` counts the steps a database has had, and `open` applies the rest.
- `kind` is `video`, or `photo` for a TikTok slideshow. `posted_at` is NULL until known: TikTok rows get it at sync, YouTube rows when their media is downloaded. It may be filled in once, never overwritten.
- `downloads` has a row once a video's download has been tried. `downloaded_at IS NULL` means not downloaded yet. A failed try counts in `attempts`, and the video is retried after 1h, 2h, 4h and so on, capped at a week (`MAX_RETRY_DELAY`). Videos are never given up on, since most failures (a broken yt-dlp, an expired login, an outage) aren't the video's fault. The queue (`pending_downloads`) is newest-stored first and skips unavailable videos.
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

- Commit straight to `master`. Never create branches. The repo has no remote.
- Run `just precommit` before committing. Don't commit if it fails.
- Commit message: an imperative subject line, then a body explaining what changed and why.
