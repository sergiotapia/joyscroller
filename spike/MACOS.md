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

## Turn 2: the app end to end, after the macOS port

Use the release download (or the build named in the PR that lands the port),
**downloaded through the browser**. A browser download carries the same
quarantine flag a user's would, which matters for step 1.

1. **Gatekeeper.** Unzip and double-click `joyscroller`. Expect macOS to block
   it ("Apple could not verify..."). Note the wording, then allow it in System
   Settings → Privacy & Security → Open Anyway. Write down every step it
   took.
2. **Launch.** A Terminal window should open with the startup log, and
   http://127.0.0.1:4774 should load. `tools/`, `downloads/` and
   `joyscroller.db` should be next to the executable.
3. **Keychain.** A dialog should ask whether *joyscroller* may use your
   browser's Safe Storage key. Click **Always Allow** this time, since it's the
   app. Quit and relaunch: there should be no second prompt. Then replace the
   binary with a newer build and launch again, and note whether it prompts
   again. That tells us whether we need to sign the app.
4. **Tools.** Run `xattr -l tools/*`. The fetched yt-dlp, ffmpeg and bun
   shouldn't have `com.apple.quarantine`. Note anything the app logged about
   them.
5. **Sync.** YouTube likes, TikTok likes and Instagram saves all appear, with
   counts.
6. **Download and play.** Download now on a YouTube video, a TikTok video, a
   TikTok photo post and an Instagram reel. Play each one in Safari and in
   Chrome: does it play, can you seek, does the photo post's sound play?
7. **Live updates.** Like a new video on your phone. Within one 5-minute sync
   it should appear in the open page without a reload.
8. **Sleep.** Close the lid for a few minutes, open it, and check the next sync
   still runs.

Post the results on #1 as a checklist, with the version tested.
