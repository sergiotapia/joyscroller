# Joyscroller

![Joyscroller's pages: the list of likes, a search typed in and its results sorted by length, a YouTube video playing, and a TikTok photo post's slideshow](docs/joyscroller.gif)

Automatically download the videos you like on TikTok, YouTube and Instagram, and keep them forever.

You've liked thousands of videos. Some get deleted, some go private, and the rest sink to the bottom of a feed you'll never scroll back through. Joyscroller saves a copy of each one to your own computer, so the videos you loved are still yours long after the app moves on.

- **Everything you've liked, in one place.** Your YouTube likes, TikTok likes and Instagram saves, side by side in one library you can browse and watch.
- **The best quality there is.** Each video is saved at the highest quality the platform offers, with its real sound. TikTok photo slideshows are saved too, images and music included.
- **Neatly organized.** Videos land in plain folders, sorted by platform and by the day you liked them, each named after its title. No mystery file names, and nothing locked inside an app.
- **Nothing to set up.** Joyscroller signs in with the browser you already use. No accounts to connect, no passwords to type, no API keys.
- **It keeps up on its own.** Leave it running and new likes show up in your library within minutes of tapping the heart.
- **Private by design.** It all runs on your computer. Your likes and videos never go anywhere else.

## How to get it and run it

You need a Mac or Linux, and [Brave](https://brave.com) signed in to YouTube, TikTok and Instagram.

### On a Mac

1. Download the latest `.dmg` from [Releases](https://github.com/sergiotapia/joyscroller/releases/latest): `macos-arm64` for Apple Silicon (M1 and later), `macos-x86_64` for an Intel Mac.
2. Open it and drag **Joyscroller** onto **Applications**.
3. Open Joyscroller. It isn't signed by Apple, so the first time macOS says *"Joyscroller" Not Opened* because Apple could not verify it. Click **Done**, not **Move to Trash**. Then open **System Settings → Privacy & Security**, scroll to the bottom and click **Open Anyway**.
4. Joyscroller opens in your browser at **http://127.0.0.1:4774**. It has no window or Dock icon of its own; open the app again to get back to it.
5. macOS asks for your Mac's password so Joyscroller can use your browser's "Safe Storage" key in the Keychain. That's how it reads your logins: type your password and click **Always Allow**. After each update it asks once more.

Your videos are saved in `Movies/Joyscroller` in your home folder, and everything else in `Library/Application Support/Joyscroller`. To stop it, click **Turn off** at the end of its taskbar.

### On Linux

1. Download the latest `joyscroller-…-linux-x86_64.tar.gz` from [Releases](https://github.com/sergiotapia/joyscroller/releases/latest) (`aarch64` if your computer has an ARM chip).
2. Unpack it wherever you'd like to keep your videos, and start it:

   ```sh
   tar -xzf joyscroller-*-linux-x86_64.tar.gz
   cd joyscroller-*-linux-x86_64
   ./joyscroller
   ```

3. Open **http://127.0.0.1:4774** and watch your likes roll in.

Keep it running to stay up to date. To stop it, click **Turn off** at the end of its taskbar, or press Ctrl-C. Your library is saved right next to the app: `joyscroller.db` and a `downloads` folder. Move the folder and your library moves with it.

### Either way

Videos are saved as the platform serves them, at their best quality, and most come in formats only some browsers play. So Joyscroller also makes a `playback.mp4` beside each of those, in H.264 at the same resolution, which plays in any browser: on a phone, a tablet or a car. It works on one video at a time in the background, at low priority.

The first time it starts, Joyscroller fetches the helpers it downloads videos with (yt-dlp, plus ffmpeg and bun if you don't already have them) into a `tools` folder beside its database, and keeps them up to date. There's nothing else to install.

### Building from source

With [Rust](https://rustup.rs) installed:

```sh
git clone https://github.com/sergiotapia/joyscroller.git
cd joyscroller
cargo run --release
```

Built this way, your library is saved in the `data` folder inside `joyscroller`.

## Developers

```sh
cargo run
```

Then open **http://127.0.0.1:4774**. That runs against your real archive in `data/`. To experiment without touching it, run a copy of the app from a throwaway folder, which keeps its own database, downloads and tools there:

```sh
mkdir -p /tmp/joyscroller-dev && cp target/debug/joyscroller /tmp/joyscroller-dev/ && /tmp/joyscroller-dev/joyscroller
```

The debugging subcommands (`likes`, `sync`, `download`, `cookies`, `tools`) are listed in [AGENTS.md](AGENTS.md). They use the app's own yt-dlp and ffmpeg once it has fetched them, and otherwise whatever is on your `PATH`.

CI checks every pull request on Linux, macOS and Windows, and builds and smoke-tests the Linux and macOS downloads. Pushing a version tag runs the same checks and builds and publishes them as a release (see [.github/workflows/release.yml](.github/workflows/release.yml)). Run `just precommit` before every commit. [CONTRIBUTING.md](CONTRIBUTING.md) has the conventions, and [AGENTS.md](AGENTS.md) has the architecture and data model.

## License

[MIT](LICENSE).
