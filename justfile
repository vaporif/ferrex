default:
    @just --list

# Setup git hooks
setup:
    git config core.hooksPath .githooks

check: clippy test check-fmt lint

lint: lint-toml check-typos check-nix-fmt lint-actions

fmt: fmt-rust fmt-toml fmt-nix

fix:
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
    just fmt

build *args:
    cargo build --workspace {{args}}

clippy:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo nextest run --workspace

coverage:
    cargo llvm-cov nextest --workspace --lcov --output-path lcov.info

coverage-html:
    cargo llvm-cov nextest --workspace --html

check-fmt:
    cargo fmt --all -- --check

fmt-rust:
    cargo fmt --all

lint-toml:
    taplo check

fmt-toml:
    taplo fmt

check-nix-fmt:
    alejandra --check flake.nix

fmt-nix:
    alejandra flake.nix

check-typos:
    typos

lint-actions:
    actionlint
