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

The dev shell provides the stable Rust toolchain (cargo, clippy, rustfmt, rust-analyzer), plus `just`, `cargo-nextest`, `cargo-llvm-cov`, `cargo-deny`, `taplo`, `typos`, `actionlint`, `alejandra`, `lefthook`, and `qdrant`.

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

After cloning, install git hooks with [lefthook](https://github.com/evilmartians/lefthook) (included in the dev shell):

```bash
just setup
```

Hooks are defined in `lefthook.yml`:
- **pre-commit** - runs `just check-fmt` to check formatting
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
just lint-ast     # ast-grep rules and their tests
```

### Tests and coverage

Use `pretty_assertions::assert_eq` and `assert_matches::assert_matches` in tests.

Coverage runs on nightly so test code can be excluded with `coverage(off)`. Every `#[cfg(test)]` item under `crates/*/src` needs `#[cfg_attr(coverage_nightly, coverage(off))]` next to it. The ast-grep rules in `ast-rules/` enforce this in `just lint-ast` and `nix flake check`.

`just coverage` runs every test, including the ignored ones that need a `qdrant` binary and the ONNX models. The nightly toolchain and qdrant come from `nix develop .#coverage`.

### Formatting

- TOML - `taplo`
- Nix - `alejandra`

### AI-assisted contributions

AI assistants are fine to use. You're responsible for every line you submit - correctness, licensing, review. If you used AI to generate code, read and verify it yourself before opening a PR. Unreviewed AI output will be declined.
