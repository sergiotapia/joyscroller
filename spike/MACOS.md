# Testing on a real Mac

Two turns. The first can happen now, against this branch. The second waits
until the macOS port has landed (#1).

## Turn 1: the cookie probe, now

CI showed that macOS stores Chromium cookies the way we expect, but that
reading the key out of the Keychain waits for someone to click Allow. CI can't
click, so this turn checks what a real user sees.

The probe runs on your **real default profiles**. For each browser installed
(Chrome, Edge, Brave, Firefox), it:

1. opens the browser,
2. visits a local page that sets one harmless cookie, `joyscroller_probe`, for
   `127.0.0.1`,
3. force-quits the browser after about a minute,
4. reads and decrypts that one cookie.

It never prints a key or password, only their lengths.

### Before you start

- **Quit every browser** and save anything open in them. The probe force-quits
  each browser, and that closes all of its windows.
- You need Python 3. If `python3 --version` asks to install the command line
  developer tools, let it.

### Run it

```sh
git clone https://github.com/sergiotapia/joyscroller.git
cd joyscroller
git switch spike/cookie-probe
python3 -m venv .venv
.venv/bin/pip install cryptography
.venv/bin/python spike/cookie_probe.py            # every browser installed
.venv/bin/python spike/cookie_probe.py brave      # or just the ones named
```

While it runs, a Keychain dialog appears for each Chromium browser, along the
lines of: *"security wants to use your confidential information stored in
'Brave Safe Storage' in your keychain."* `security` is Apple's command-line
tool. The probe uses it the way joyscroller will use the Keychain.

- Click **Allow**, not **Always Allow**. Always Allow would let any script
  that runs `security` read that browser's key without asking.
- Write down exactly what the dialog says, and whether it asks for your login
  password. A screenshot is ideal.
- You have 3 minutes per dialog before the probe gives up.

### What to post on #13

- The Markdown table the probe prints at the end, which is also saved in
  `report-macos.json`.
- The Keychain dialog's wording or a screenshot, and whether it wanted your
  password.
- Your macOS version and chip (Apple menu → About This Mac).
- Anything else odd, such as a browser that didn't open or a dialog that came
  up twice.

Afterwards you can delete the `joyscroller` folder. The `joyscroller_probe`
cookie is harmless, but you can clear it in each browser's settings.

## Before turn 2: what's done, and what's yours

Done in this PR and passing in CI on real Macs (both chips): the app
builds as a DMG holding `Joyscroller.app`, installs from it, fetches its own
yt-dlp, ffmpeg and bun, keeps its data in the right folders, opens its UI,
hands a second launch over to the running copy, and turns off from the UI.
The `Package` workflow's smoke test checks all of that on every push.

Also done: **reading browser logins on macOS**, in `src/browser.rs`.
It finds the profile under `~/Library/Application Support`, reads the key
from the browser's "Safe Storage" Keychain item through Security.framework
(so the dialog names Joyscroller, not `security`) and keeps it for the rest of
the run. It also finds the browser for the headless listing in
`/Applications` or `~/Applications`, and starts it with `--use-mock-keychain`.

## Turn 2 results (2026-10-14, Apple Silicon, macOS 26, Brave)

Run from the PR's DMG with a browser's quarantine flag, then from a local
build of the second-launch fix installed over it.

- [x] Gatekeeper: *"Joyscroller" Not Opened. Apple could not verify
  "Joyscroller" is free of malware that may harm your Mac or compromise your
  privacy.* Buttons: **Move to Trash** (highlighted, the default) and **Done**.
  Pressing Return deletes the app, so the docs say to click Done. Then
  System Settings → Privacy & Security → Open Anyway works.
- [x] Keychain: asks for the Mac's password. After installing a new build
  over it, it asked again (two dialogs, likely use + change access). After
  Always Allow, a relaunch read the key without asking.
- [x] Launch: the UI opens in the browser; the data is in Application Support
  and the videos in `~/Movies/Joyscroller`. macOS runs a quarantined app from
  an AppTranslocation folder, which changes nothing here.
- [x] Second launch: **was broken**, opening the app again did nothing,
  because the bundle's first process stayed running so macOS only
  re-activated it. Fixed (the app now runs detached and the launcher exits);
  checked on the Mac, and the smoke test now opens the app a second time
  with `open`.
- [x] Tools: yt-dlp fetched and not quarantined. ffmpeg and bun came from
  `PATH` (Homebrew, ~/.bun) because `open` from a terminal passes its `PATH`;
  a Finder launch has a bare `PATH` and would fetch them.
- [x] Sync: 4,989 YouTube likes. TikTok and Instagram weren't signed in on
  that Mac, so not checked.
- [x] Download and play: YouTube videos downloaded in 11 to 29 s, AV1 and VP9
  turned into H.264 playback copies, served with range requests; plays and
  seeks in Brave.
- [x] Turn off: nothing left running.
- [ ] Not checked: TikTok and Instagram, the Intel build, live updates (item 7)
  and sleep (item 8).

## Turn 2: the app end to end

The Mac release is a `.dmg` per chip (`macos-arm64` for Apple Silicon,
`macos-x86_64` for Intel) holding `Joyscroller.app`. Like
[obs-rust's](https://github.com/obs-rust/obs-studio/releases), it's unsigned:
no Apple Developer ID and no notarization, ever. Apple Silicon only runs code
with some signature, so the build gives it the free ad-hoc one, which needs
no account. macOS blocks it the first time. The app has no window and no Dock
icon: it opens its UI in the browser. Downloads go to `~/Movies/Joyscroller`,
and everything else to `~/Library/Application Support/Joyscroller`.

Get the DMG from this PR: on the PR's **Checks** tab, open the latest
**Package** run, and under **Artifacts** download
`joyscroller-<version>-pr13-<commit>-macos-arm64` (or `-x86_64` on an Intel
Mac). Do it **in the browser**, not with `gh run download`: a browser download
carries the same quarantine flag a user's would, which matters for step 1.
GitHub wraps it in a zip; double-click that to get the `.dmg`.

1. **Install and Gatekeeper.** Open the `.dmg` and drag Joyscroller to
   Applications. Double-click it and expect macOS to block it ("Apple could not
   verify..."). Note the wording, then allow it in System Settings → Privacy &
   Security → Open Anyway, and write down every step it took. Also check
   whether right-click → Open still offers a way through on your macOS version.
2. **Launch.** The UI should open in your default browser at
   http://127.0.0.1:4774, with no Dock icon. Launch the app again while it's
   running: it should just reopen the UI, not start a second copy.
   `~/Library/Application Support/Joyscroller` should hold `joyscroller.db`,
   `tools/` and the log file.
3. **Keychain.** A dialog should ask whether *Joyscroller* may use your
   browser's Safe Storage key. Click **Always Allow** this time, since it's the
   app. Quit from the UI and relaunch: there should be no second prompt. Then
   install a newer build over it and launch again, and note whether it prompts
   again. An unsigned app probably prompts after every update. That's
   expected; note it so the release notes can say so.
4. **Tools.** Run `xattr -l ~/Library/Application\ Support/Joyscroller/tools/*`.
   The fetched yt-dlp, ffmpeg and bun shouldn't have `com.apple.quarantine`.
   Check the log for tool errors.
5. **Sync.** YouTube likes, TikTok likes and Instagram saves all appear, with
   counts.
6. **Download and play.** Download now on a YouTube video, a TikTok video, a
   TikTok photo post and an Instagram reel. They should land in
   `~/Movies/Joyscroller`. Play each one in Safari and in Chrome: does it play,
   can you seek, does the photo post's sound play?
7. **Live updates.** Like a new video on your phone. Within one 5-minute sync
   it should appear in the open page without a reload.
8. **Sleep.** Close the lid for a few minutes, open it, and check the next sync
   still runs.
9. **Quit.** The UI's Quit button stops the app: the page says so, and
   `pgrep -l joyscroller` finds nothing.

Post the results on #1 as a checklist, with the version and chip tested.
