# Contributing

## Setup

```sh
rustup toolchain install stable
cargo install just cargo-machete cargo-deny cargo-nextest antislop --locked
```

To run the app you also need `yt-dlp` on your `PATH` and a Chromium-family browser signed in to YouTube and TikTok. See `AGENTS.md` for the details and the developer subcommands.

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

## Writing code

- **Fix lints, don't silence them.** If a lint really is wrong for one spot, put `#[expect(clippy::name, reason = "...")]` on that item. Don't add a crate-wide `allow` without a good reason, written as a comment in `Cargo.toml`.
- **No panics in app code.** Return `anyhow::Result`, add `.context()` saying what was being attempted, and let the caller decide. `unwrap` and `expect` are only allowed in tests.
- **Test what you add.** Pure logic (parsing, decoding, formatting, SQL) gets unit tests in the same file. Tests can't touch the network, your browser, the keyring or your real database. Use in-memory SQLite.
- **Keep modules to one job.** SQL lives in `db.rs`, browser and cookie handling in `browser.rs`, driving a headless browser in `headless.rs`, yt-dlp in `youtube.rs`, TikTok's API in `tiktok.rs`. A new platform gets its own module next to `youtube.rs`.
- **Comments say why, not what.** Write down the non-obvious constraint: a file-format quirk, an ordering requirement, a yt-dlp flag's side effect.

## Adding a dependency

Use `cargo add <crate>`, then run `just deny`. If the crate's license isn't in `deny.toml`'s `allow` list, check that it's acceptable before adding it there. Remove dependencies you stop using. `just deps` will flag them anyway.

## Committing

- Commit straight to `master`. There are no feature branches.
- Each commit should be one logical change that passes `just precommit` by itself.
- Message format: an imperative subject (`Store liked videos in SQLite`), a blank line, then a body explaining what changed and why.
