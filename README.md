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

You need Linux and [Brave](https://brave.com), signed in to YouTube, TikTok and Instagram.

1. Download the latest `joyscroller-…-linux-x86_64.tar.gz` from [Releases](https://github.com/sergiotapia/joyscroller/releases/latest) (`aarch64` if your computer has an ARM chip).
2. Unpack it wherever you'd like to keep your videos, and start it:

   ```sh
   tar -xzf joyscroller-*-linux-x86_64.tar.gz
   cd joyscroller-*-linux-x86_64
   ./joyscroller
   ```

3. Open **http://127.0.0.1:4774** and watch your likes roll in.

Keep it running to stay up to date, and press Ctrl-C to stop it. Your library is saved right next to the app: `joyscroller.db` and a `downloads` folder. Move the folder and your library moves with it.

The first time it starts, Joyscroller fetches the helpers it downloads videos with (yt-dlp, plus ffmpeg and deno if you don't already have them) into a `tools` folder beside it, and keeps them up to date. There's nothing else to install.

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

The debugging subcommands (`likes`, `sync`, `download`, `cookies`) are listed in [AGENTS.md](AGENTS.md). They use the app's own yt-dlp and ffmpeg once it has fetched them, and otherwise whatever is on your `PATH`.

Pushing a version tag builds and publishes a release (see [.github/workflows/release.yml](.github/workflows/release.yml)). Run `just precommit` before every commit. [CONTRIBUTING.md](CONTRIBUTING.md) has the conventions, and [AGENTS.md](AGENTS.md) has the architecture and data model.

## License

[MIT](LICENSE).
