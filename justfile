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
