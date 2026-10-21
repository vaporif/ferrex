default:
    @just --list

# Setup git hooks
setup:
    lefthook install --reset-hooks-path

check: clippy test check-fmt lint

lint: lint-toml check-typos check-nix-fmt lint-actions lint-ast

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

# Nightly so `coverage(off)` excludes test code from the report
coverage:
    nix develop .#coverage --command cargo llvm-cov nextest --workspace --profile coverage --run-ignored all --lcov --output-path lcov.info

coverage-html:
    nix develop .#coverage --command cargo llvm-cov nextest --workspace --profile coverage --run-ignored all --html

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

lint-ast:
    ast-grep test --skip-snapshot-tests
    ast-grep scan

e2e:
    cargo nextest run -p ferrex-core -p ferrex-server --test '*' --no-capture
