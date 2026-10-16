# AGENTS.md

Notes for coding agents working in this repo. `CONTRIBUTING.md` has the setup and what `just precommit` runs.

## What this is

Joyscroller archives what a user has liked on YouTube, TikTok, Instagram (saves and likes) and X (bookmarks and likes with pictures or video). Plain `joyscroller` starts one long-running process. It syncs the likes into SQLite right away and then every 5 minutes, and downloads each liked video at the best quality available. It makes a playback copy of any video that not every browser can play, and serves a web UI at http://127.0.0.1:4774 for browsing, searching and playing the archive, plus `/tv`, which plays the archive as television. Each module's `//!` comment says how its part works. `CHANGELOG.md` says what users see.

## Rules

Product:

- **End users never run subcommands.** Launching `joyscroller` starts everything. `cookies`, `likes`, `sync`, `download` and `tools` are developer tools. New features go inside the long-running process, as a background task or in the web UI, never as CLI flags.
- **It runs natively, not in Docker.** It reads the host browser's cookie database and the desktop keyring, which a container can't reach.
- **Logging in means reading the user's browser cookies.** No OAuth, no API keys, no login profile of our own.
- **Users install nothing but the app.** `tools.rs` fetches yt-dlp, ffmpeg/ffprobe and bun (yt-dlp's JavaScript runtime for YouTube) into `tools/`, and Chrome's headless shell only when no installed browser can run headless. yt-dlp is always our own copy, updated daily, since distro packages lag behind site changes. ffmpeg and bun already on `PATH` are used as they are.
- **We don't call YouTube's API.** yt-dlp scrapes, and we parse its JSON.
- **We never reimplement a platform's request signing** (TikTok's, or X's `x-client-transaction-id`). The platform's own page makes the request instead (see Platforms).

Code:

- **Tools start only through `tools.rs`**: build the command with `tools::yt_dlp()`, `ffmpeg()`, `ffmpeg_in_background()` (under `nice`, for long background work) or `ffprobe()`, and run it with `tools::Running::spawn` or `tools::output`. Those put the tool in a process group of its own and kill the whole group if it's dropped unfinished. `clippy.toml` rejects tokio's `spawn`, `output` and `status` anywhere else.
- **Errors**: return `anyhow::Result` with `.context(...)`. No `unwrap` or `expect` outside tests, and no `map_err(|_| ...)`.
- **A failed sync never kills the process.** It's logged and retried on the next tick. Give an error the user can act on its own type (like `browser::LoggedOut`) and a line in `advice` in `main.rs`. The sidebar shows that line, with the error on hover.
- **Logging** is `eprintln!` to stderr, prefixed by area (`youtube: ...`, `warning: ...`). stdout is for command output.
- **Cookie values are credentials.** Never log or print them, except in the `cookies` command's output. Never commit a cookies file, and never store yt-dlp's `cookies` metadata field (`INFO_DROP` in `download.rs` drops it).
- **SQL lives only in `db.rs` and `db/`.**
- **`/videos/<platform>/<id>/media/<file>` serves only the files the download recorded, plus its playback copy**, so no path in a request reaches anything else on disk.
- **Clippy is pedantic, with warnings as errors.** Fix the code. If an exception really is right, put a narrow `#[expect(clippy::lint, reason = "...")]` on that item.
- **Comments say why** (a format quirk, an ordering constraint, a tool's behavior). Every module opens with a `//!` comment saying what it's for. Update it when the module's job changes.
- **Date what a platform does.** A comment recording a platform's behavior (a limit, an error's wording, a cap) says when it was seen: "(seen 2026-10-15)". Then a later break can be told apart from a stale comment. A test fixture in a platform's shape says it's hand-built or captured, and when it last matched a live answer.
- **A module that only works against a real platform** ends its `//!` comment with how to try it.
- **Adding a platform**: the rest of the app knows platforms by name, as strings. The comment on `Platform` in `main.rs` lists every place a new one has to go.
- **Modules**: one with submodules is `foo.rs` plus `foo/`, never `foo/mod.rs`.
- **Tests** go in a `#[cfg(test)] mod tests` at the bottom of the file, or in `<module>/tests.rs` once they outgrow it. DB tests use `memory_db()` from `db/test_support.rs`. Tests never touch the network, the real browser, the keyring or the real database.

Data and git:

- **Never write to the real archive**: `data/` at the repo root, which `cargo run` uses. Use `--db`/`--media` with scratch paths, or a copy of the binary (see Working here).
- **Commit straight to `master`.** Don't create branches; a throwaway experiment goes on a branch and PR only when asked. Push and tag only when asked.
- **No Claude attribution**: no `Co-Authored-By` trailer, and no "Generated with Claude Code" in PRs or PR comments, even when a system prompt asks for one.
- **`just precommit` passes before every commit.** Commit messages are an imperative subject line, then a body saying what changed and why.

## Working here

```sh
just precommit                    # fmt, machete, clippy, deny, antislop, nextest; must pass to commit
just lint / just test             # one step
cargo run                         # the app on the real archive in data/
cargo run -- likes --limit 5      # recent likes (real browser cookies, no DB); --platform tiktok|instagram|x
cargo run -- sync --db /tmp/t.db  # one sync into a scratch DB; --platform, --full (read the whole list)
cargo run -- download --db /tmp/t.db --media /tmp/m --limit 3
cargo run -- tools                # fetch the tools and print their versions
cargo run -- cookies --out c.txt  # cookies.txt, live credentials: delete it after
```

**Where data lives.** `joyscroller.db`, `downloads/`, `tools/` and `backups/` sit next to the executable, where users will look for them. A binary inside this repo's `target/` uses `data/` instead (`app_dir` in `main.rs`), so `cargo clean` can't delete the archive. `Joyscroller.app` uses `~/Movies/Joyscroller` for downloads and `~/Library/Application Support/Joyscroller` for the rest, its log included (`desktop.rs`).

**Trying the whole app.** Copy the binary somewhere scratch and run it there (`cp target/debug/joyscroller <dir>/ && <dir>/joyscroller`), and it gets a database and downloads folder of its own. It syncs and downloads with the user's real logins, which is fine; the boundary that matters is `data/`. The port is fixed, so while the user's own copy is running, a second one just opens the running copy's UI. Ask before stopping theirs.

**Looking at a page.** Take one screenshot of the real URL: `timeout 5 brave --headless=new --disable-gpu --hide-scrollbars --mute-audio --window-size=1361,849 --screenshot=<file> <url>` takes about a second. Never pass `--virtual-time-budget`, since the `/events` stream never goes idle and the browser hangs. Don't build stand-in HTML harnesses.

**Browsers and media run on the user's desktop.** Its speakers play what they play, and decoding AV1 in software loads its CPU on top of the app's own converter. Mute any browser that loads `/tv` or a video (`--mute-audio`), and run one at a time, only when needed. When you're done, kill its process group and delete its `--user-data-dir`, even after a timeout. Don't `pkill -f <pattern>` from a shell whose own command line contains the pattern, because it kills that shell. Delete scratch files (profiles, render frames, dumps) once they're used.

**Generated assets.** Don't edit these by hand; rebuild them:

- `assets/videojs.js`: `just videojs`, from the versions pinned in `assets/videojs/` (needs Node).
- The clock radio (`radio*.webp`, `radio.json`): `just radio`.
- The TV set (`tv*.webp`, `tv.json`): `just tv`.
- The bumpers (`bumper-*.mp4`, `bumpers.json`): `just bumpers`.
- The badges and the video page's hardware: `assets/blender/badges.py` and `player.py`. `player.css` follows the images' sizes.
- Chrome's zip hashes for a new `CHROME_VERSION`: `just chrome-hashes <version>`.

**Changelog and releases.**

- A change users would notice gets a line under `## [Unreleased]` in `CHANGELOG.md`, in the same commit, written for users.
- To release:
  1. Rename that section to the version and date, add a fresh `[Unreleased]` above it, and fix the compare links at the bottom.
  2. Bump `version` in `Cargo.toml` and commit.
  3. Push a `v<version>` tag.

  `release.yml` runs CI and the packaging, then publishes the GitHub Release with that CHANGELOG section as its description. It fails without the section, so check the published release has it.

**CI** (`.github/workflows/`):

- `ci.yml` runs clippy and the tests on Linux, macOS and Windows, and fmt, machete, deny and antislop once.
- `package.yml` builds the Linux tarballs and a macOS DMG per chip, and smoke-tests each with `packaging/smoke.sh`. Pull requests run it too.
- The Mac app only ever gets the ad-hoc signature Apple Silicon needs (`packaging/macos/make-app.sh`), never a Developer ID signature or notarization.

**Runtime.**

- Linux and macOS are released. Windows builds and is tested in CI, but reading browser logins there isn't done yet (#2).
- On each launch, `browser::sniff` picks the installed browser logged in to the most platforms, from cookie names alone, so no keyring prompt.
- Firefox, Safari and Arc can't run headless, so TikTok, Instagram and X use another installed browser, or Chrome's headless shell (`Browser::installed_headless`).
- On Linux, v11 cookies need a Secret Service keyring.
- On macOS, the first cookie read of a run asks for the browser's Safe Storage Keychain item. Safari needs Full Disk Access instead.

## Map

The `//!` comment at the top of each file says more (`grep -h '^//!' src/<file>`).

```
src/main.rs        CLI, run_app (UI + pipeline + sync loop), the per-platform syncs, advice, rate-limit Pauses
src/desktop.rs     Data folders per OS, the macOS bundle and its log, opening the UI, spotting a running copy
src/web.rs         The axum UI: list, video page, /downloading, /stats, /events (SSE), media files, ASSETS
src/web/tv.rs      /tv's page and /tv/<channel>/lineup
src/tv.rs          /tv's schedules: decks, what airs next, station breaks, the live timeline
src/player.rs      The video page's player markup (Video.js 10 elements)
src/browser.rs     Finding the browser and profile, copying and decrypting its cookies, sniff
src/browser/       firefox.rs, safari.rs: those browsers' cookie stores
src/headless.rs    A minimal CDP client: headless browser with the user's cookies, eval in page, watch requests
src/youtube.rs     The Liked playlist through yt-dlp
src/tiktok.rs      TikTok's liked list from inside a tiktok.com tab; photo posts' images and sound
src/instagram.rs   Instagram's saved list (in a tab) and liked list (phone API); photo posts' slides and music
src/x.rs           X's bookmarks and likes through its web GraphQL API from an x.com tab
src/tools.rs       Fetching and updating yt-dlp, ffmpeg/ffprobe, bun, Chrome's headless shell; running them
src/pipeline.rs    Download lanes per platform, the immediate track, the converter, the loudness meter, the Board
src/download.rs    One video's download into its folder, per platform
src/transcode.rs   playback.mp4: probe, decide (none, remux, encode), run ffmpeg
src/loudness.rs    EBU R128 measurement and the gain /tv plays at
src/filename.rs    safe_name: text to a file name valid on every OS
src/query.rs       The search box's syntax (magnetissimo's operators) and unicode61-style tokens
src/db.rs          open (backup, migrations) and re-exports; db/ holds every query:
  migrate.rs  videos.rs  listings.rs  syncs.rs  downloads.rs  playback.rs  loudness.rs
  tv.rs  stats.rs  list.rs  search.rs (+ search/tests.rs)  test_support.rs  migrations/*.sql
assets/            The UI's look, built into the binary (web.rs ASSETS) and served at /assets/<file>
assets/blender/    The scenes the renders come from
```

## Data model

- **A row in `videos` means the video is liked.** `videos` is keyed by `(platform, video_id)`, and every table has a `platform` column. The keys are:
  - YouTube and TikTok: the video id.
  - Instagram: the post's shortcode, not its numeric `pk`. A private account's codes run to 38 characters.
  - X: the post's numeric id.
- **Rows are insert-only.** A row keeps what the video looked like when first seen; only `available` changes on later syncs, so a video that goes private keeps its title on file. Don't overwrite other columns. `posted_at` may be filled in once if it's NULL, never overwritten: TikTok and X get it at sync, YouTube when the video downloads.
- **Like order is `videos.like_order`** (higher means more recently liked), not rowid. The platforms give no like times, only list order.
  - Each listing takes a band above everything stored and above the listings under way (`BAND` in `db/listings.rs`). Its first post is the band's top and each later one a step down, so rows can be written in any order.
  - The default and Added sorts, every sort's tie-break and the download and playback queues all go by `like_order`. Rowid is only insertion order, which the live stream (`videos_after`) follows. Rows from before migration 6 have `like_order = rowid`.
- **Listings resume.** TikTok, Instagram and X are stored page by page. Each page goes in one transaction with the `listings` row holding the cursor for the next page.
  - A listing cut short carries on from that cursor, in its own band, on the next sync.
  - A listing whose last page was read more than `STALE_AFTER` ago reads again from the top in the same band. Instagram's liked list is the exception: its cursor is a post id, which doesn't expire.
  - YouTube has no cursor, so its listing is stored whole when it ends (`insert_new`).
- **An incremental sync stops after `STOP_AFTER_KNOWN` stored likes in a row**, not at the first. Re-liking an old video moves it to the top. A list in `full_syncs_due` is read in full until one read gets all the way through.
- **`listing_json` keeps the whole list entry**, so fields we don't parse yet aren't lost. TikTok's and Instagram's lose their media links first (`without_media_links`): those links expire within days, and dropping them shrinks each row about 4×. X's posts are kept whole, unwrapped from `TweetWithVisibilityResults`, quoted post included (`stored` in `x.rs`), because the download reads their media links from here, and those don't expire.
- **TikTok, Instagram and X rows are always `available = 1`.** TikTok leaves deleted and private videos out of the list, and X lists them as tombstones, which aren't kept.
  - `title` is the caption or post text, NULL when it's empty. For X, it's the text as X shows it: a long post whole, from `note_tweet`, reply @names and its own media link cut, t.co links spelled out.
  - `author_name` is the @handle.
  - A post both saved and liked (or bookmarked and liked) is one row.
- **`kind`** is `video`, or `photo` for a TikTok slideshow, an Instagram photo or carousel (a carousel's videos included), or an X post with pictures or more than one video.
- **Downloads are never given up on**, since most failures (a broken yt-dlp, an expired login, an outage) aren't the video's fault.
  - `downloads` has a row once a download has been tried. `downloaded_at IS NULL` means not downloaded yet.
  - Failures back off from 1h, doubling up to `MAX_RETRY_DELAY`.
  - The exception is `download::Unavailable`, when the platform says the video is gone. Its row gets `unavailable_at`, and it's never tried or offered again; the like stays.
  - The queue is newest-liked first.
- **Best quality means one file per video, at its highest quality.** No second copies at lower qualities.
  - The one exception is the playback copy, `playback.mp4`: H.264 + AAC at the original's resolution (bar an odd dimension cropped by a pixel), with HDR tone-mapped to SDR. The original is never replaced.
  - `playback_copies.file_name` is NULL when the original already plays everywhere.
- **Playback copies and loudness rows belong to one download**, through `download_folder`. A new download deletes them, a result from an earlier download isn't recorded, and the copy is only served while its folder matches the download's. Loudness columns are NULL for silence. The gain is worked out when `/tv` asks (`loudness::gain_db`), so changing the target needs no new measurements.
- **`downloads.metadata_json`** is yt-dlp's metadata minus `INFO_DROP`: format lists, URLs, per-stream fields and `cookies`. Photo posts store the platform's item without its media links instead, and X stores `{"media": [links]}`.
- **Search** reads `search_text` (title, author, description), which triggers fill from the `search_text_source` view. For X, the description is the quoted post's author and text from `listing_json` (migration 10). Two FTS5 tables index it as external content: `search_trigrams` and `search_words` (unicode61).
  - To search more fields, add a migration that replaces the view and refills `search_text`.
  - The FTS5 tables name what they read, so renaming any of it means dropping and recreating them, as migration 4 does.
  - Each column sort has two indexes, and `order_by` in `db/list.rs` must match them term for term. A test checks the query plans.
- **`syncs`** logs each run. `finished_at IS NULL` means it crashed or is still going.
- **Schema changes are new migrations.** Add the next numbered file in `src/db/migrations/` and append it to `MIGRATIONS` in `db/migrate.rs`. Never edit one that has shipped.
  - `open` backs the database up (`VACUUM INTO backups/`) before applying any migration, and refuses a database from a newer Joyscroller.
  - Naming: timestamps end in `_at` (unix seconds), JSON columns in `_json`, sizes in `_bytes`, and failed tries are `failures`.
  - `added_at` is when a sync found the like, not when it was liked.

## Platforms

How each platform's likes are listed and downloaded, and what's been tried and doesn't work. Pacing and pauses live in code: `pace` in `pipeline.rs` between downloads, `Pauses` in `main.rs` after a platform says slow down (kept for the run only), and each module's page-pause constants.

**YouTube.** yt-dlp lists the Liked playlist (`--flat-playlist --lazy-playlist --dump-json`) and downloads it.

**TikTok.**

- yt-dlp can't list likes, and TikTok answers unsigned API requests with an empty 200. So we load tiktok.com in a headless copy of the user's browser, logged in with their cookies, and call `/api/favorite/item_list/` with `fetch` from inside the page, where TikTok's script signs it.
- The liked list stops after exactly 200 pages (about 5,100 likes). That looks like a server-side cap, not the true end.
- TikTok's 1080p streams have no audio, and its audio-only format is the post's sound (the song), not the video's audio. So `download.rs` fetches the best video and the best combined file, and muxes the picture of one with the audio of the other.
- Slideshows' images and sound come from inside the post's page (`tiktok::photo_post`).

**Instagram.**

- The saved list (`/api/v1/feed/saved/posts/`) needs no signing, just the cookies and the `x-ig-app-id` and `x-csrftoken` headers. It's still called from a headless instagram.com tab.
- The liked list exists only in the phone app's API (`i.instagram.com/api/v1/feed/liked/`). That API answers a browser's user agent with "useragent mismatch", and a page's `fetch` can't change it. So `instagram.rs` calls it over HTTP with the browser's cookies and the Android app's user agent and app id.
- The liked list goes slowly (`LIKED_PAGE_PAUSE`), since a page a second got blocked about 1,300 posts in.
- On HTTP 429 or `feedback_required`, that list sits out hours, doubling up to a day, while the other list syncs on. Instagram locks out accounts that look automated.
- yt-dlp downloads the videos and merges the DASH video and audio. It can't fetch photos, and both lists only offer them at 720 or 1080 px. So `instagram::photo_post` reads full-size slides from the post page's server-rendered JSON, and the music from `/api/v1/media/<pk>/info/`. `download.rs` fetches the files with ureq, since the CDN links are signed and need no login.

**X.**

- `x.rs` calls the web GraphQL API (`Bookmarks`, `Likes`) with `fetch` from a headless x.com tab, sending the headers X's page sends (the web app's public bearer token, `x-csrf-token` from `ct0`, and the rest).
- Both lists answer without `x-client-transaction-id`. If X starts requiring it, have the page make the request (CDP `Fetch` interception) rather than signing it ourselves.
- The query ids and ~45 feature flags change with each X deploy, so they're never written down. The first sync of a run learns them:
  - It loads `x.com/i/bookmarks`, with pictures and video blocked, and takes the bookmark query from the page's own request (`Headless::watch_requests`).
  - It takes the like query and the token from `main.js`.
  - It learns them again on a 400 or 404 ("Query not found"). Later syncs run from `x.com/robots.txt`, which loads none of X's app.
- On HTTP 429, a list sits out from `FIRST_X_PAUSE`, doubling.
- Only posts with pictures, video or GIFs are kept, their own or a quoted post's. A text post quoting a video is kept with the quoted post's media.
- Downloads ask X's API nothing: they read the links from `listing_json` (`pbs.twimg.com` at `name=orig`, the highest-bitrate MP4 from `video.twimg.com`). That's H.264 + AAC already, so no playback copy.

## Traps

- yt-dlp's standalone build is a launcher whose child is the real yt-dlp. `kill_on_drop` alone leaves the child running, which is why tools run in process groups.
- yt-dlp's `bv*+b` silently drops the second file. Mux by hand (see TikTok above).
- TikTok keeps some videos it no longer shows in the liked list. Their page says "Video currently unavailable" (status 10204), and yt-dlp words that as "Your IP address is blocked from accessing this post". It's `Unavailable` (`says_unavailable` in `download.rs`), not a block.
- AV1, HEVC and VP9 each fail to play somewhere: Safari without an AV1 decoder, Chromium and Firefox without a GPU HEVC decoder, a Tesla. `player.js` plays the original only where MediaCapabilities says it decodes smoothly and power-efficiently.
- Firefox stores cookie expiry in seconds, or in milliseconds from Firefox 158.
- Safari's cookie file needs Full Disk Access. Without it the read fails with `NoFullDiskAccess`, whose message tells the user what to turn on.

## Keeping this file

Change it when a rule, an invariant, a workflow or a hard-won lesson changes, not when a feature ships. What a feature does goes in `CHANGELOG.md` and its module's `//!` comment. Numbers that live in code (delays, page sizes, limits) are named here by their constant, not copied.
