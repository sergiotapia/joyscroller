# Contributing

## Setup

```sh
rustup toolchain install stable
cargo install just cargo-machete cargo-deny cargo-nextest antislop --locked
```

To run the app you need a supported browser (Chrome, Firefox, Safari, Edge, Brave and others; see `AGENTS.md`) signed in to the platforms you want archived. The app fetches its own yt-dlp, ffmpeg and bun on launch. `AGENTS.md` has the developer subcommands and how to try the app without touching your real archive.

## The one rule: `just precommit` passes

Run it before every commit. It's the Rust version of a `mix precommit` alias:

| Step | Command | What it catches |
|---|---|---|
| `fmt` | `cargo fmt --all` | Formatting. It rewrites files, so look at the diff afterward. |
| `deps` | `cargo machete` | Dependencies in `Cargo.toml` that the code never uses |
| `lint` | `cargo clippy --all-targets --all-features -- -D warnings` | Compiler warnings and clippy lints (pedantic, plus `unwrap`/`expect`/`todo!`/`dbg!`), all treated as errors |
| `deny` | `cargo deny check` | Security advisories, unmaintained crates, disallowed licenses, unknown sources |
| `slop` | `antislop src` | Placeholder comments, "for now"/"should work" hedging, leftover stubs |
| `test` | `cargo nextest run` | Unit tests |

You can run any step alone, e.g. `just lint`.

## Other operating systems

CI runs clippy and the tests on Linux, macOS and Windows for every pull request, and builds and smoke-tests the Linux and macOS downloads, so code behind `#[cfg(...)]` for another OS gets checked even if you only have Linux. Every build uses the Rust release in `rust-toolchain.toml`, so CI's lints match yours.

To catch another OS's compile errors before pushing, you can type-check for it locally with [zig](https://ziglang.org) as the C compiler, since a few dependencies compile C. Add the target (`rustup target add x86_64-pc-windows-gnu` or `aarch64-apple-darwin`), point `CC_<target>` at a script running `zig cc -target x86_64-windows-gnu` (or `aarch64-macos`) that drops the `--target=` and `-arch` flags cc passes, and run `cargo clippy --target <target>`.

## Writing code

- **Fix lints, don't silence them.** If a lint really is wrong for one spot, put `#[expect(clippy::name, reason = "...")]` on that item. Don't add a crate-wide `allow` without a good reason, written as a comment in `Cargo.toml`.
- **Start tools through `tools.rs`.** `tools::Running::spawn` and `tools::output` run yt-dlp, ffmpeg and the rest in a process group that's killed if dropped; clippy rejects tokio's `spawn` and `output` elsewhere.
- **No panics in app code.** Return `anyhow::Result`, add `.context()` saying what was being attempted, and let the caller decide. `unwrap` and `expect` are only allowed in tests.
- **Test what you add.** Pure logic (parsing, decoding, formatting, SQL) gets unit tests in the same file. Tests can't touch the network, your browser, the keyring or your real database. Use in-memory SQLite.
- **Keep modules to one job.** SQL lives in `db.rs` and the modules under `db/` (one per table or concern), browser and cookie handling in `browser.rs`, driving a headless browser in `headless.rs`, each platform's listing in its own module (`youtube.rs`, `tiktok.rs`, `instagram.rs`, `x.rs`), downloading media in `download.rs`, the web UI in `web.rs` and its player in `player.rs`. A new platform gets its own module next to those. `AGENTS.md` has the full map.
- **Comments say why, not what.** Write down the non-obvious constraint: a file-format quirk, an ordering requirement, a yt-dlp flag's side effect.

## Adding a dependency

Use `cargo add <crate>`, then run `just deny`. If the crate's license isn't in `deny.toml`'s `allow` list, check that it's acceptable before adding it there. Remove dependencies you stop using. `just deps` will flag them anyway.

## Committing

- Commit straight to `master`. There are no feature branches.
- Each commit should be one logical change that passes `just precommit` by itself.
- Message format: an imperative subject (`Store liked videos in SQLite`), a blank line, then a body explaining what changed and why.
- If users would notice the change, add a line under `## [Unreleased]` in `CHANGELOG.md` in the same commit.
