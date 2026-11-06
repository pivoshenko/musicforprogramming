default:
    @just --list

install:
    cargo fetch

format:
    cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets
    cargo fmt --all

lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

audit:
    cargo deny check

test:
    @[ -f .no-tests ] && echo "skipping (.no-tests sentinel)" || cargo test --workspace

check: lint audit test build

update:
    cargo update

build:
    cargo build --release

run:
    cargo run --bin mfp

generate-changelog:
    git-cliff --output CHANGELOG.md

generate-social-preview:
    rsvg-convert -b '#1f1f1e' --page-width 1280 --page-height 640 --top 146 \
      -w 1280 -h 348 assets/preview_social_dark.svg -o assets/preview_social_dark.png
