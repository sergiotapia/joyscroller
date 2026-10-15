# Run everything before committing: `just precommit`.
precommit: fmt deps lint deny slop test

fmt:
    cargo fmt --all

# Dependencies listed in Cargo.toml that the code never uses.
deps:
    cargo machete

# Compiler warnings + clippy (lint config lives in Cargo.toml and clippy.toml).
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Advisories, licenses, banned and duplicate crates (see deny.toml).
deny:
    cargo deny check

# Placeholder comments, hedging, deferrals and stubs left in the code.
slop:
    antislop src

# No doctests: this is a binary crate, and nextest doesn't run them anyway.
test:
    cargo nextest run --all-features

# Rebuild assets/videojs.js, the Video.js 10 elements the player uses, from
# the versions pinned in assets/videojs/package-lock.json. Needs Node.
videojs:
    cd assets/videojs && npm ci && npm run build

# The SHA-256 of each platform's Chrome headless shell zip at `version`, for
# ASSETS and CHROME_VERSION in src/tools.rs. Downloads about 600 MB.
chrome-hashes version:
    for platform in linux64 linux-arm64 mac-arm64 mac-x64 win64; do \
      echo "$platform $(curl -fsSL https://storage.googleapis.com/chrome-for-testing-public/{{version}}/$platform/chrome-headless-shell-$platform.zip | sha256sum | cut -d' ' -f1)"; \
    done
