default:
    @just --list

install: install-package

install-package:
    cargo fetch

format: format-package

format-package:
    cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets
    cargo fmt --all

lint: lint-package

lint-package:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

audit: audit-package

audit-package:
    cargo deny check

test: test-package

test-package:
    @[ -f .no-tests ] && echo "skipping (.no-tests sentinel)" || cargo test --workspace

check: lint audit test build

update: update-package

update-package:
    cargo update

build: build-package

build-package:
    cargo build --release --workspace

run-app:
    cargo run --bin mfp

generate-social-preview:
    rsvg-convert -b '#1f1f1e' --page-width 1280 --page-height 640 --top 146 \
      -w 1280 -h 348 assets/preview_social_dark.svg -o assets/preview_social_dark.png
