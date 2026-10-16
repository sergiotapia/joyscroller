# AGENTS.md

Notes for coding agents working in this repo. Read this, then `CONTRIBUTING.md`.

## What this is

A Rust app that archives the videos a user has liked. Plain `joyscroller` starts one long-running process that syncs YouTube and TikTok likes and Instagram saves (the bookmark) and likes into SQLite right away and then every 5 minutes. It also serves a web UI at http://127.0.0.1:4774 for browsing the archive, where newly synced likes appear live over Server-Sent Events. The sidebar lists each platform's post types under it (TikTok: videos, slideshows; Instagram: videos, photos, carousels; `POST_TYPES` in `web.rs`, `post_type_sql!` in `db/videos.rs`), and the list sorts by any column (the sort and type live in the URL) and has a search box with magnetissimo's operators (`query.rs`), answered by two FTS5 indexes over each video's title, author and description (`db/search.rs`). `download.rs` saves a liked video's media at the best quality available into `downloads/<platform>/<YYYY-MM-DD>/<title> [<video id>]/`, by the local date it was liked (so the first sync's day holds the whole backlog). `filename.rs` makes titles safe as folder names on every OS. That folder, the database (`joyscroller.db`) and `tools/` sit next to the app's executable, where people will look for them. A binary running from this repo's `target/` (so `cargo run`) uses `data/` at the repo root instead, which survives `cargo clean` and is gitignored. The macOS app (`Joyscroller.app`) can't hold them, so it keeps downloads in `~/Movies/Joyscroller` and everything else, its log included, in `~/Library/Application Support/Joyscroller` (`desktop.rs`). On Windows and in the macOS app, launching opens the UI in the browser; launching while it's already running opens the running copy's UI. The taskbar's Turn off button stops the app, killing any yt-dlp, ffmpeg or headless browser it started; so do Ctrl-C, SIGTERM and SIGHUP (`stop_signal` in `main.rs`). Videos download when the user clicks "Download now" on a video's page: the download pipeline (`pipeline.rs`) starts it right away on a track of its own, several at once if asked. The pipeline also has a lane per platform that works through the whole backlog newest first, one video at a time, waiting 30 s between YouTube videos, a random 15–30 s between TikTok ones and 30–60 s between Instagram ones (yt-dlp's wiki puts YouTube's logged-in limit at ~2,000 videos/hour; TikTok and Instagram publish none, and Instagram is quick to lock out accounts that look automated). The lanes are on (`AUTOMATIC` in `pipeline.rs`) and wait for `tools.rs` to have fetched yt-dlp and ffmpeg before their first video. After each download, the pipeline's converter (`transcode.rs`) makes a playback copy, `playback.mp4` (H.264 + AAC, at the original's resolution and frame rate), for any video that isn't already H.264 and AAC in MP4, since AV1, HEVC and VP9 each fail to play somewhere (Safari without an AV1 decoder, Chromium and Firefox without a GPU HEVC decoder, a Tesla). It runs one video at a time under `nice`, newest first, and a page that can't play a video's original moves that video to the front. `/downloading` shows it all live. Once downloaded, a video plays on its page (`player.rs`): a video in a player, a photo post (TikTok's, or an Instagram photo or carousel) as a slideshow with its sound playing, a carousel's videos among its slides. With a copy, `player.js` plays the original only where MediaCapabilities says it decodes smoothly and power-efficiently, and the copy otherwise. The page loads the files from `/videos/<platform>/<id>/media/<file>`, which serves only the files the download recorded and the playback copy. `/tv` plays what's downloaded as television, on a channel for all platforms and one for each (`tv.rs`): a live schedule kept in memory, so tuning in joins a show partway through, dealt from a shuffled deck per platform so nothing repeats until everything has aired, with creators spaced apart, the all-platforms channel sharing airtime by each platform's number of downloads (not by count of shows, or YouTube's long videos would take most of it), long videos airing a 10-minute stretch and photo posts 5 s a picture, and a station break every 4 minutes of airtime: one of the bumpers (`assets/blender/bumpers.py`, `bumpers.json`), on the schedule like a show. The page is a TV set rendered in the clock radio's plastic (`assets/blender/tv.py`, laid out from `tv.json`) whose working buttons, knob and LCD sit over the render. Under its cut-out glass it stacks two of the video page's players, one on air and one loading the next show, and draws the picture from whichever is on air through a WebGL2 picture tube (`assets/crt.js`): an NTSC composite encode and decode, then the tube's scanlines, grille and curve, calibrated against off-air recordings of the day. It levels every show's sound through Web Audio at the gain `loudness.rs` works out from its measured EBU R128 loudness, ahead of a limiter, and synthesizes the set's own sounds (`assets/tvsound.js`). The `download` subcommand downloads by hand. Every table row has a `platform` column.

**Product rules:**
- End users never run subcommands. Launching `joyscroller` must start everything. The `cookies`, `likes` and `sync` subcommands are developer and debugging tools only. New user-facing features go inside the long-running process, as a background task or in the web UI, not as new CLI flags.
- The app runs natively, not in Docker. It has to read the host browser's cookie database and the desktop keyring, which a container can't reach.
- Logging in means reading the user's browser cookies. Don't add OAuth or API-key flows.
- Users install nothing but the app. `tools.rs` fetches yt-dlp, ffmpeg/ffprobe and bun (yt-dlp needs a JavaScript runtime for YouTube; bun rather than deno, whose standalone runtime winds down after Cloudflare bought it) into `tools/` on launch and runs `yt-dlp --update` daily. yt-dlp is always our own copy, since distro packages lag behind site changes; ffmpeg and bun already on `PATH` are used as they are. Chrome's headless shell (Chrome for Testing, pinned to `CHROME_VERSION` with each zip's SHA-256 recorded, since Google publishes none; `just chrome-hashes <version>` makes new ones) is fetched only when the user's browser can't run headless (Firefox, Safari, Arc) and no other installed one can, the first time TikTok or Instagram needs it. Run the tools through `tools::yt_dlp()`, `tools::ffmpeg()` (`tools::ffmpeg_in_background()`, under `nice`, for long background work) and `tools::ffprobe()`, never `Command::new("yt-dlp")`, and start them with `tools::Running::spawn` or `tools::output`, never `.spawn()` or `.output()`: those put the tool in a process group of its own and kill the whole group if it's dropped unfinished. yt-dlp's standalone build is a launcher whose child, the real yt-dlp, `kill_on_drop` alone leaves running.
- We don't call YouTube's API ourselves. `yt-dlp` does the scraping, and we parse its JSON.
- yt-dlp can't list TikTok likes, and TikTok rejects API requests its own page script didn't sign. So we load tiktok.com in a headless copy of the user's browser, logged in with their cookies, and call the API with `fetch` from inside the page, where TikTok's script signs it. Don't try to reimplement TikTok's request signing.
- yt-dlp can't list Instagram saves or likes either. Instagram's saved-posts endpoint (`/api/v1/feed/saved/posts/`) needs no signing, only the cookies plus the `x-ig-app-id` and `x-csrftoken` headers, but it's called the same way, from a headless instagram.com tab. The liked list is the exception to calling from a tab: it exists only in the phone app's API (`i.instagram.com/api/v1/feed/liked/`), which answers a browser's user agent with "useragent mismatch", and a page's `fetch` can't set one. So `instagram.rs` calls it directly over HTTP with the browser's cookies and the Android app's user agent and app id, more slowly than the saved list's page a second: a random 5–10 s a page (`LIKED_PAGE_PAUSE`), since a page a second got Instagram's block about 1,300 posts into a full read. When Instagram says to slow down (HTTP 429, or its `feedback_required` block), that list sits out 3 hours, doubling up to a day while it keeps happening (`Pauses` in `main.rs`, kept for the run only), and the other list syncs on. Videos (`media_type` 2), photos (1) and carousels (8) are all kept; photos and carousels are `photo` posts. yt-dlp downloads the videos. It can't fetch photos, and both lists only offer them at 720 or 1080 px wide, so at download time `instagram::photo_post` reads every slide at full size from the post's own page (its server-rendered JSON) and the music from the web API's `/api/v1/media/<pk>/info/`, and `download.rs` fetches the files with ureq (the CDN links are signed and need no login).

## Layout

```
src/main.rs     CLI (clap), the app loop (run_app), and sync_youtube / sync_tiktok, which tie the modules together
src/desktop.rs  Fitting into each desktop: the macOS app bundle's data folders and log file, opening the UI
                in the browser, and recognizing a copy that's already running
src/web.rs      The localhost UI (axum): server-rendered HTML list and video pages (with a Download button),
                /downloading, and /events, an SSE stream of newly stored videos, counts, sync status and
                the download pipeline's board. run_app feeds it sync status through a watch channel.
                Also serves downloaded files (with range requests, for seeking) to the player
src/web/tv.rs   /tv's page (the set from tv.json, two stages for players under its glass, the remote) and
                /tv/<channel>/lineup, the JSON of what's on and next that assets/tv.js plays, each show with
                its player, and each break with its bumper (bumpers.json) and the show it leads into
src/tv.rs       The TV channels' schedules: the decks, the rules for what airs next, airtimes, station
                breaks, and the live timeline (in memory, restarted after an hour with nobody watching)
src/player.rs   The video page's player markup, from Video.js 10's elements: a video player (naming its
                playback copy too, when there is one), or a photo post's slideshow over its sound, which
                autoplays and loops
src/browser.rs  Finds the browser profile, copies its Cookies SQLite DB, decrypts values
                (AES-128-CBC; Linux: v10 = fixed key, v11 = key from the keyring; macOS: v10 = key from
                the Keychain, read once per run), writes Netscape cookies.txt. `sniff` picks the browser
                logged in to the most platforms
src/browser/firefox.rs  Firefox's cookies: finds the profile in use through profiles.ini, reads cookies.sqlite
                (unencrypted; expiry in seconds or, from Firefox 158, milliseconds)
src/browser/safari.rs   Safari's cookies (macOS): parses Cookies.binarycookies (unencrypted, but only readable
                with Full Disk Access; without it the read fails with NoFullDiskAccess, whose message says
                what to turn on and reaches the UI as the sync's error)
src/youtube.rs  Runs `yt-dlp --flat-playlist --lazy-playlist --dump-json` on the Liked playlist and
                streams JSON Lines; a callback can stop the listing early
src/tiktok.rs   Pages through TikTok's liked list (/api/favorite/item_list/) from inside a headless
                tiktok.com tab and parses the items; a callback can stop the listing early. Also
                fetches a photo post's images and sound from inside its page (yt-dlp only gets the sound)
src/instagram.rs Pages through Instagram's saved list (/api/v1/feed/saved/posts/) from inside a headless
                instagram.com tab, and its liked list from the phone app's API, and parses the posts; a
                callback can stop the listing early. Also finds a photo post's full-size slides and music
src/headless.rs Minimal Chrome DevTools Protocol client: launches the browser headless with a throwaway
                profile, loads cookies, navigates, evaluates JS in the page
src/tools.rs    Fetches the app's own yt-dlp, ffmpeg/ffprobe and bun from their GitHub releases (checksum-
                verified) into tools/ on Linux, macOS and Windows (macOS's ffmpeg comes from
                ffmpeg.martin-riedl.de), keeps yt-dlp updated, and builds the commands that run them.
                Also fetches Chrome's headless shell when no installed browser can run headless
src/pipeline.rs The download pipeline: one lane per platform working through its backlog (lanes run in
                parallel, one video at a time each, paced by `pace`), an immediate track per video asked
                for in the UI, the converter that makes playback copies one at a time, the meter that
                measures each download's loudness, and the Board of what's running and finished that
                the UI watches
src/loudness.rs Measures a download's sound with ffmpeg's ebur128 (integrated loudness, true peak) and
                turns it into the gain /tv plays it at
src/transcode.rs Makes a downloaded video's playback copy (`playback.mp4`, H.264 + AAC, same resolution) when
                the original won't play in every browser: probes it with ffprobe, decides (none, remux or
                encode), runs ffmpeg at low priority, and describes the original for MediaCapabilities
src/download.rs Downloads one video's media into its folder: with yt-dlp (`video.<ext>`, `thumbnail.<ext>`;
                muxes TikTok's 1080p picture with its real audio; merges Instagram's DASH video and audio), or for photo posts through
                tiktok.rs or instagram.rs (`image-NN.<ext>`, a carousel's `video-NN.<ext>`, `sound.<ext>`,
                Instagram's cut to the part the post plays); returns trimmed metadata
src/filename.rs safe_name: text to a file/folder name valid on Linux, macOS and Windows (forbidden characters,
                reserved device names, trailing dots, invisible characters, byte-length limits)
src/db.rs       The archive database: `open` (backup, migrations) and re-exports of the modules below, which
                hold every query; the only code that touches the archive DB
src/db/migrate.rs   Brings a database up to date from the SQL files in src/db/migrations/ (0000_schema.sql,
                then one numbered file per step), with a backup first; refuses a newer database
src/db/videos.rs    Storing synced likes (insert-only) and reading them back
src/db/listings.rs  Like order: each listing's band, and the listings under way (cursor, how far they got)
src/db/syncs.rs     The log of sync runs
src/db/downloads.rs The download queue (with retry back-off) and what each download saved
src/db/playback.rs  The playback-copy queue and what each try made
src/db/loudness.rs  The loudness queue and what each measurement found
src/db/tv.rs        What /tv can air: every download, with its length and loudness
src/db/list.rs      The UI's pages: platform filter, search, and the column sorts with their indexes
src/db/search.rs    Turns a parsed search into SQL against the FTS5 indexes (tests in db/search/tests.rs)
src/db/test_support.rs  Fixtures the db tests share (in-memory database, sample rows, a search helper)
src/query.rs    Parses what's typed in the search box: words, "quoted text", -exclusions, a|b alternatives,
                prefix* and *patterns (magnetissimo's operators); splits text into tokens the way unicode61 does
assets/         The UI's look, built into the binary by web.rs (`ASSETS`) and served at /assets/<file>:
                style.css (Windows XP desktop + Nickelodeon clock radio), lab.webp (the page background), splat.svg (favicon),
                player.css (the player's skin) and player.js (the slideshow, and picking the original or
                the playback copy; `setUp` sets up a player put on the page later, as /tv does),
                tv.js and tv.css (/tv: the set and its controls, the stacked players, the schedule, levels,
                the OSD, the up-next card's text), crt.js (/tv's picture tube in WebGL2), tvsound.js (the
                set's sounds, synthesized), the set's render (tv.webp, tv-glass.webp, the tv-*.webp
                sprites, tv.json; `just tv`, blender/tv.py) and its bumpers (bumper-*.mp4, bumpers.json;
                `just bumpers`, blender/bumpers.py; served with range requests), videojs.js (Video.js 10, bundled
                by `just videojs` from the versions pinned in videojs/; don't edit it by hand),
                radio.webp (rendered by blender/radio.py; the page overlays a live clock on its LCD),
                and its hover animation (`just radio`, blender/radio_motion.py): radio-lit.webp, the
                radio-dome*.webp and radio-knob.webp sprite sheets, radio.json saying where they go
                (web.rs builds the radio's markup from it), and radio.js, which plays them,
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
cargo run -- likes --platform instagram --limit 5   # Instagram's saved posts, then its liked ones
cargo run -- sync --db /tmp/t.db  # one sync into a throwaway DB (add --platform tiktok for TikTok)
cargo run -- sync --full          # read the whole liked list, not just until known videos
cargo run -- download --db /tmp/t.db --media /tmp/m --limit 3   # download the next 3 queued videos
cargo run -- cookies --out c.txt  # export cookies.txt (contains live credentials)
cargo run -- tools                # fetch the app's yt-dlp, ffmpeg, ffprobe and bun and print their versions
```

Runtime needs: Linux or macOS (Windows isn't released yet: the tools and desktop fit work there and CI tests it, but reading browser logins doesn't, see #2), a browser (Chrome, Safari, Edge, Firefox, Opera, Brave, Opera GX, Arc, Chromium or Vivaldi; Safari, Opera GX and Arc are macOS-only here) whose `Default` profile (for Firefox, the profile whose cookies changed last) is signed in to YouTube, TikTok and Instagram. On each launch `browser::sniff` picks the installed browser logged in to the most of the three, most widely used first on a tie, from cookie names alone (no decryption, so no keyring or Keychain prompt). Safari's can't be checked without Full Disk Access, so then it's picked only when no other browser is logged in to anything. The subcommands do the same unless given `--browser`, and that browser's executable (TikTok and Instagram run it headless; Firefox, Safari and Arc can't run headless, so for them another installed one runs instead, or Chrome's headless shell from `tools::headless_chrome`, see `Browser::installed_headless`): on Linux on `PATH`, on macOS in `/Applications` or `~/Applications`. On Linux, v11 cookies need a Secret Service keyring (gnome-keyring or KWallet). On macOS, the first cookie read of each run asks the user to let Joyscroller use the browser's "Safe Storage" Keychain item, unless they clicked Always Allow. Safari needs Full Disk Access instead, for Joyscroller.app or, under `cargo run`, the terminal.

A full TikTok listing takes about 2 minutes for 5,000 likes (one request per 30). In testing, TikTok's liked list ended after exactly 200 pages (about 5,100 likes, back to January 2026). That looks like a server-side cap rather than the true end of the list.

The real archive is `joyscroller.db` and `downloads/` in `data/` at the repo root (for `cargo run`). Don't write to it while experimenting. Pass `--db` (and `--media`) with a temp path, or run a copy of the binary from a temp folder.

## Data model invariants

- `videos` is keyed by `(platform, video_id)`. A row existing means the video is liked.
- Rows are insert-only. A row keeps what the video looked like when first seen. Only `available` is updated on later syncs, so a video that later goes private keeps its title on file. Don't overwrite other columns.
- Like order is `videos.like_order` (higher = more recently liked), not rowid: the platforms give no like times, only list order, newest first. Each listing takes a band of like order above everything stored and above listings under way (`db/listings.rs`, `BAND`), its first post the band's top and each later one a step down, so rows can be written in any order. Rows from before migration 6 have `like_order = rowid`. The list's default and Added sorts, every column sort's tie-break and the download and playback queues go by it; rowid is only insertion order, which the UI's live stream (`videos_after`) follows. TikTok's and Instagram's lists are stored page by page as they're read, each page in one transaction with the `listings` row that says how far the listing got and the platform's cursor for the next page. A listing cut short (a restart, a failed page) carries on from that cursor in its own band on the next sync, and is deleted once it reaches the end or stops at stored posts; one whose last page was read more than a day ago (`STALE_AFTER`, from `listings.read_at`, since that's when the cursor was given out; never for Instagram's liked list, whose cursor is a post id) reads its list again from the top, in full, in the same band. YouTube's listing (yt-dlp has no cursor to carry on from) is stored whole when it ends, through `insert_new`.
- `videos.listing_json` holds the full entry from the liked/saved list (yt-dlp's for YouTube, the API item for TikTok and Instagram), so fields we don't parse yet aren't lost. For TikTok, every TikTok-hosted URL and the per-quality format lists are stripped first (`without_media_links` in `tiktok.rs`): they expire within about two days and the media is downloaded separately, and dropping them takes an item from ~23 KB to ~5 KB. Instagram's items get the same treatment (`without_media_links` in `instagram.rs`: CDN URLs, format lists and DASH manifests), ~45 KB down to ~13 KB.
- Instagram rows are keyed by the post's shortcode (`instagram.com/reel/<code>/`), not its numeric `pk`; a private account's posts have long ones (38 characters). Like TikTok, they're always `available = 1`, `title` is the caption (NULL when empty), and `author_name` is the @handle. A saved post and a liked one are the same kind of row, and a post both saved and liked is one row; `listing_json` is the entry from whichever list found it first. Each sync reads the saved list, then the liked list, each stored page by page.
- TikTok leaves deleted and private videos out of the liked list instead of listing them stripped down, so TikTok rows are always stored with `available = 1`. `title` holds the caption, which is often empty (stored as NULL).
- `syncs` logs each run. `finished_at IS NULL` means the run crashed or is still going.
- An incremental sync stops after `STOP_AFTER_KNOWN` (100) already-stored likes in a row. Stopping at a single known video would be wrong, because re-liking an old video moves it to the top of the list. A list in `full_syncs_due` is read in full instead until a read gets all the way through: migration 5 puts Instagram's liked list there (new, and its newest likes are often stored saves) and, for libraries made before saved photos and carousels were kept, its saved list. A read that fails partway carries on from its cursor next time (see the like-order rule below), so nothing it stored is lost and the stop rule can't skip what it never reached.
- Schema changes are migrations: add the next numbered SQL file in `src/db/migrations/` and append it to `MIGRATIONS` in `db/migrate.rs`. Never edit `0000_schema.sql` (the original tables) or a step that has shipped. `PRAGMA user_version` counts the steps a database has had, and `open` applies the rest. Before applying any, `open` saves a copy of the database in `backups/` beside it (`VACUUM INTO`), and it refuses a database whose `user_version` is past the steps it knows (made by a newer Joyscroller). Naming: a timestamp column ends in `_at` (unix seconds), a JSON column in `_json`, a size in `_bytes`, and failed tries are `failures`. `videos.added_at` is when a sync first found the like; the platforms don't say when it was liked.
- `kind` is `video`, or `photo` for a TikTok slideshow or an Instagram photo or carousel (a carousel's videos included). `posted_at` is NULL until known: TikTok rows get it at sync, YouTube rows when their media is downloaded. It may be filled in once, never overwritten.
- `downloads` has a row once a video's download has been tried. `downloaded_at IS NULL` means not downloaded yet. A failed try counts in `failures`, and the video is retried after 1h, 2h, 4h and so on, capped at a week (`MAX_RETRY_DELAY`). Videos are never given up on, since most failures (a broken yt-dlp, an expired login, an outage) aren't the video's fault, except one the platform says is gone (`download::Unavailable`): TikTok keeps a video it no longer shows in the liked list, and yt-dlp words its status 10204 ("Video currently unavailable" on the site) as "Your IP address is blocked from accessing this post". Its download row gets `unavailable_at`, and it's never tried again or offered for download; the like stays. It's on the download row because a sync refreshes `videos.available` from the list. The queue (`pending_downloads`) is newest-liked first (by `like_order`) and skips unavailable videos.
- Search reads `search_text` (title, author, description; one row per video under its rowid), which triggers fill from `videos` and `downloads` through the `search_text_source` view, and the FTS5 tables `search_trigrams` and `search_words` (unicode61) index it as external content. To search more fields, add a migration that replaces the view and refills `search_text`. The FTS5 tables name `search_text` in their own definition, so renaming it (or anything they read) means dropping and recreating them, as migration 4 does. Each column sort has two indexes, and `order_by` in `db/list.rs` must match them term for term (a test checks the query plans).
- `downloads.metadata_json` is yt-dlp's metadata (for photo posts, TikTok's item or Instagram's `/media/<pk>/info/` item without its media links) minus `INFO_DROP` in `download.rs`. That covers format lists, URLs and per-stream fields, and `cookies`, which yt-dlp fills with the user's login. Never store that field.
- `loudness` has a row once a download's sound has been measured or tried, for the download in its `download_folder`, like `playback_copies`: a new download deletes it, and a measurement of an earlier download isn't recorded. `integrated_lufs` and `true_peak_dbtp` are NULL for a post with no sound or only silence. The gain comes from them when /tv asks (`loudness::gain_db`), so changing the target needs no new measurements.
- `playback_copies` has a row once a downloaded video's playback copy has been tried, for the download in its `download_folder`. `finished_at IS NULL` means not done yet, and failures back off like downloads. `file_name` is NULL when the original already plays everywhere (H.264 and AAC/MP3 in MP4). A new download of the video deletes the row, a copy of an earlier download that finishes afterwards isn't recorded, and the copy is only served while its `download_folder` matches the download's `folder`. The copy never changes the resolution; the only exception is cropping an odd width or height by one pixel, which 4:2:0 H.264 requires. HDR is tone-mapped to SDR.
- "Best quality" means one file per video at the highest quality it has: no second copies at lower qualities. The playback copy is the one exception: it's there for compatibility, at the original's resolution, and the original is never replaced. TikTok's 1080p streams have no audio, and its audio-only format is the post's *sound* (the song), not the video's audio. So `download.rs` fetches the best video and the best combined file and muxes the picture of one with the audio of the other. yt-dlp can't do this itself: with `bv*+b` it silently drops the second file.

## Code conventions

- Errors: functions return `anyhow::Result` and add context with `.context(...)` / `.with_context(...)`. Don't use `unwrap`/`expect` outside tests (clippy denies them). Don't swallow errors with `map_err(|_| ...)`.
- A failed sync in the app loop is logged and retried on the next tick. It must never kill the process. The sidebar shows `advice` (`main.rs`) for it, in plain words, and the error itself on hover: give an error the user can act on its own type (like `browser::LoggedOut`) and a line in `advice`.
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
