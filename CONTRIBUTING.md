# Contributing

## Prerequisites

### With Nix (recommended)

The project includes a Nix flake with a complete dev shell. If you have Nix with flakes enabled:

```bash
# Enter the dev shell (or use direnv with the included .envrc)
nix develop

# Build via Nix
nix build
```

The dev shell provides the stable Rust toolchain (cargo, clippy, rustfmt, rust-analyzer), plus `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`, `taplo`, and `typos`.

### Without Nix

Requires a stable Rust toolchain. Install via [rustup](https://rustup.rs).

You'll also need these tools (provided automatically by the Nix dev shell):

- [just](https://github.com/casey/just) - command runner
- [cargo-nextest](https://nexte.st) - test runner
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) - code coverage
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) - dependency auditing
- [taplo](https://taplo.tamasfe.dev) - TOML formatter/linter
- [typos](https://github.com/crate-ci/typos) - spell checker
- [actionlint](https://github.com/rhysd/actionlint) - GitHub Actions linter

## Setup

After cloning, configure git hooks:

```bash
just setup
```

This sets up:
- **pre-commit** - runs `just fmt` to check formatting
- **pre-push** - runs `just check` for full lint + test suite

## Building and testing

A `justfile` wraps all common commands:

```bash
just check        # Run all checks (clippy, test, fmt, lint)
just test         # Run tests
just clippy       # Run clippy
just check-fmt    # Format check
just fmt          # Auto-format everything (Rust, TOML, Nix)
just lint         # Run linters (TOML, typos, nix-fmt, actionlint)
just coverage     # Generate lcov coverage report
just coverage-html # Generate HTML coverage report
just check-typos  # Spell check
just lint-toml    # TOML lint
```


### Formatting

- TOML - `taplo`
- Nix - `alejandra`

### AI-assisted contributions

AI assistants are fine to use. You're responsible for every line you submit - correctness, licensing, review. If you used AI to generate code, read and verify it yourself before opening a PR. Unreviewed AI output will be declined.
