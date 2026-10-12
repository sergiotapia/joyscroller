# AGENTS.md

Notes for coding agents working in this repo. Read this, then `CONTRIBUTING.md`.

## What this is

A Rust app that archives the videos a user has liked. Plain `joyscroller` starts one long-running process that syncs YouTube likes into SQLite right away and then every 5 minutes. A localhost web UI for browsing the archive is planned. Instagram and TikTok come later, which is why every table row has a `platform` column.

**Product rules:**
- End users never run subcommands. Launching `joyscroller` must start everything. The `cookies`, `likes` and `sync` subcommands are developer and debugging tools only. New user-facing features go inside the long-running process, as a background task or (later) the web UI, not as new CLI flags.
- The app runs natively, not in Docker. It has to read the host browser's cookie database and the desktop keyring, which a container can't reach.
- Logging in means reading the user's browser cookies. Don't add OAuth or API-key flows.
- We don't call YouTube's API ourselves. `yt-dlp` does the scraping, and we parse its JSON.

## Layout

```
src/main.rs     CLI (clap), the app loop (run_app), and sync_youtube, which ties the modules together
src/browser.rs  Finds the browser profile, copies its Cookies SQLite DB, decrypts values
                (AES-128-CBC; v10 = fixed key, v11 = key from the keyring), writes Netscape cookies.txt
src/youtube.rs  Runs `yt-dlp --flat-playlist --lazy-playlist --dump-json` on the Liked playlist and
                streams JSON Lines; a callback can stop the listing early
src/db.rs       SQLite schema (the SCHEMA const) and every query; the only module that touches the archive DB
```

Config: `Cargo.toml` (lint levels), `clippy.toml`, `deny.toml`, `justfile`.

## Commands

```sh
just precommit                    # must pass before every commit (fmt, machete, clippy, deny, antislop, nextest)
just lint / just test             # run a single step

cargo run                         # the app: syncs into the real DB every 5 min
cargo run -- likes --limit 5      # print recent likes (reads real browser cookies, no DB)
cargo run -- sync --db /tmp/t.db  # one sync into a throwaway DB
cargo run -- sync --full          # read the whole liked list, not just until known videos
cargo run -- cookies --out c.txt  # export cookies.txt (contains live credentials)
```

Runtime needs: Linux, `yt-dlp` on `PATH`, a Chromium-family browser profile signed in to YouTube (default `--browser brave --profile Default`), and a Secret Service keyring (gnome-keyring or KWallet) for v11 cookies.

The real archive is at `~/.local/share/joyscroller/joyscroller.db`. Don't write to it while experimenting. Pass `--db` with a temp path.

## Data model invariants

- `videos` is keyed by `(platform, video_id)`. A row existing means the video is liked.
- Rows are insert-only. A row keeps what the video looked like when first seen. Only `available` is updated on later syncs, so a video that later goes private keeps its title on file. Don't overwrite other columns.
- Insert oldest first, so rowid order matches like order. yt-dlp lists newest first, so `sync_youtube` reverses the list.
- `raw_json` holds the full yt-dlp entry, so fields we don't parse yet aren't lost.
- `syncs` logs each run. `finished_at IS NULL` means the run crashed or is still going.
- An incremental sync stops after `STOP_AFTER_KNOWN` (100) already-stored likes in a row. Stopping at a single known video would be wrong, because re-liking an old video moves it to the top of the list.
- There are no migrations yet. The schema is `CREATE TABLE IF NOT EXISTS`, so changing an existing table won't touch databases that already exist. If you change the schema, add a migration step and say so.

## Code conventions

- Errors: functions return `anyhow::Result` and add context with `.context(...)` / `.with_context(...)`. Don't use `unwrap`/`expect` outside tests (clippy denies them). Don't swallow errors with `map_err(|_| ...)`.
- A failed sync in the app loop is logged and retried on the next tick. It must never kill the process.
- Logging is `eprintln!` to stderr, prefixed by area (`youtube: ...`, `warning: ...`). There's no logging framework yet. stdout is for command output.
- Cookie values are credentials. Never log or print them, except in the `cookies` command's output, and never commit a cookies file.
- Comments explain *why* (format quirks, ordering constraints, yt-dlp behavior). Every module has a `//!` doc comment saying what it's for. Keep those up to date when a module's job changes.
- Tests go in a `#[cfg(test)] mod tests` at the bottom of the file they test. DB tests use `Connection::open_in_memory()` with `SCHEMA`. Tests must not touch the network, the real browser, the keyring or the real DB.
- Clippy runs pedantic with warnings as errors. Fix the code rather than silencing the lint. If an exception really is right, use a narrow `#[expect(clippy::lint, reason = "...")]` on that item.

## Git

- Commit straight to `master`. Never create branches. The repo has no remote.
- Run `just precommit` before committing. Don't commit if it fails.
- Commit message: an imperative subject line, then a body explaining what changed and why.
