# Contributing

- [Contributing](#contributing)
  - [Reporting Bugs](#reporting-bugs)
    - [How to Submit a Bug Report](#how-to-submit-a-bug-report)
  - [Suggesting Enhancements](#suggesting-enhancements)
    - [How to Submit an Enhancement](#how-to-submit-an-enhancement)
  - [Code Contributions](#code-contributions)
    - [Local Development](#local-development)
    - [CI/CD](#cicd)
    - [Branches](#branches)
    - [Commits](#commits)
    - [Pull Requests](#pull-requests)

Thank you for taking the time to contribute.

These guidelines are intended to make contributions consistent and easy to review across repositories. They are guidance, not hard instructions, and maintainers may adapt them when needed.

## Reporting Bugs

Before creating a bug report, search existing issues to avoid duplicates.

When opening a bug report, include enough context for someone else to reproduce the issue and understand the impact.

> [!NOTE]
> If you find a closed issue that looks similar, open a new issue and link the previous one.

### How to Submit a Bug Report

Open a bug report and provide the following:

- A clear, descriptive title
- Reproduction steps (minimal and reliable if possible)
- Current behavior and expected behavior
- Relevant environment details (for example OS, runtime, browser, framework versions)
- Logs, stack traces, screenshots, or recordings when useful

If the issue is intermittent, describe how often it happens and known triggers.
If the issue appeared after a change, mention the last known working version or commit if available.

## Suggesting Enhancements

Before submitting an enhancement, check whether a similar request already exists.

Enhancement requests can include new features, changes to existing behavior, usability improvements, or performance improvements.

### How to Submit an Enhancement

Open a feature request and provide the following:

- A clear problem statement
- The proposed solution
- Alternatives considered or current workarounds
- Expected impact (who benefits and how)

Concrete examples, API sketches, UI mockups, or references are helpful when relevant.

## Code Contributions

### Layout

| Crate        | What it holds                                                           |
| ------------ | ----------------------------------------------------------------------- |
| `mfp-core`   | Episode and catalog model, the wire protocol, config, every path        |
| `mfp-daemon` | Audio engine, streaming and downloads, the socket server, durable state |
| `mfp-tui`    | The `mfp` binary: argument parsing, the socket client, the interface    |

The workspace is these three crates; `plugins/herdr/mfp.player` sits outside it, so `just check`
does not cover it.

### Local Development

This project needs a Rust toolchain (`cargo`) matching `rust-toolchain.toml`, and `libasound2-dev` on Linux - `rodio` links against ALSA. macOS needs nothing beyond the toolchain.
This project uses [`just`](https://github.com/casey/just) as its task runner. Run `just --list` for the full set; these are the ones you need day to day:

| Command                   | What it does                                                             |
| ------------------------- | ------------------------------------------------------------------------ |
| `install`                 | Fetches the crate dependencies into the cargo cache                      |
| `format`                  | Reformats the Rust sources in place                                      |
| `lint`                    | Lints every target with Clippy, failing on any warning                   |
| `audit`                   | Checks licences, advisories, and banned crates with cargo-deny           |
| `test`                    | Runs the workspace test suite                                            |
| `check`                   | Runs `lint`, `audit`, `test`, and `build`                                |
| `update`                  | Upgrades the lockfile to the newest compatible versions                  |
| `build`                   | Builds the optimized release binaries                                    |
| `run`                     | Runs the interface from the working tree                                 |
| `generate-changelog`      | Regenerates `CHANGELOG.md` from the commit history with git-cliff        |
| `generate-social-preview` | Rasterizes the social preview SVG into a 1280x640 PNG via `rsvg-convert` |

1. Fork the repository and create a branch for your change
2. Set up the project with `just install`, then make your change
3. Run `just check` and fix anything it reports before opening a pull request

The default run is hermetic - nothing issues a network request and nothing opens an audio device - so a failure is a real failure rather than a sandbox artifact. Tests that need a daemon bring up their own against a scratch instance, which is why every path in `mfp-core/src/paths.rs` carries an `$MFP_*` override.

The tests that do touch the real world are `#[ignore]`d with the reason in the attribute, and are run explicitly:

```sh
cargo test -p mfp-daemon --test end_to_end -- --ignored --nocapture
```

That one streams a real episode and is audible; the player has no volume of its own, so turn the system volume down first.

The lint set in `Cargo.toml` is deliberately narrow: lints that catch a defect, not lints that enforce a house style. `unwrap`, `expect`, and `panic` are denied in production code and allowed in tests, where a panic is the failure report. Clippy runs with `-D warnings`, so a warning is a build failure.

> [!IMPORTANT]
> Behavioral code changes should include or update tests.

### CI/CD

Workflows live in `.github/workflows`:

| Workflow | Trigger                                            | What it does                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| -------- | -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| CI       | Push to `main`, pull requests, `workflow_dispatch` | `ci-rs` runs on `ubuntu-24.04-arm` and `macos-15`: stable toolchain with rustfmt and Clippy over a cached cargo registry, ALSA headers on Linux, then format check, lint, test, and build. `audit` runs cargo-deny on `ubuntu-24.04-arm`. There is no Windows leg - the client speaks over a Unix domain socket                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| Release  | `workflow_dispatch` (optional `version` input)     | Five chained jobs: `tag` resolves the next version from the commit history with git-cliff or from the optional input, bumps `Cargo.toml` and the lockfile, regenerates `CHANGELOG.md`, then commits and pushes `main` with the `v<version>` tag; `build` cross-compiles release binaries for four targets (`x86_64` and `aarch64`, each for `unknown-linux-gnu` and `apple-darwin`), packaging `mfp` and `mfp-daemon` into a per-target `.tar.gz`; `release` publishes the GitHub Release with the generated notes, every archive, and a `checksums.txt`; then `publish-crates` publishes `mfp-core`, `mfp-daemon`, and `mfp-tui` to Crates.io in dependency order, and `update-homebrew` pushes a refreshed `Formula/musicforprogramming.rb` to `pivoshenko/homebrew-tap` |

CI must be green before a pull request is merged.

### Branches

Branch names follow the pattern `<type>/<short-description>` using the same type prefixes as commits.
The description should be lowercase kebab-case, brief, and specific enough to identify the change at a glance.

Examples:

```
feat/random-episode-key
fix/backward-seek-decoder-wedge
docs/document-socket-overrides
refactor/split-spectrum-smoothing
```

A branch covering multiple unrelated changes should be split. One concern per branch makes review and bisect much easier.

### Commits

Use clear, focused commits with descriptive messages.

This project follows [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).

**Format**

```
<type>(<scope>): <subject>

[optional body]
```

- **type** - one of the prefixes from the table below
- **scope** - the crate, module, or command being changed (e.g. `audio`, `catalog`, `ipc`, `tui`, `download`, `seek`); omit when the change is truly cross-cutting
- **subject** - imperative mood, lowercase, no trailing period, 72 characters or fewer
- **body** - optional; use it to explain _why_, not _what_; wrap at 72 characters

**Type prefixes**

| Prefix     | When to use                                                                                             |
| ---------- | ------------------------------------------------------------------------------------------------------- |
| `feat`     | A new feature or user-facing capability                                                                 |
| `fix`      | A bug fix that corrects incorrect behavior                                                              |
| `docs`     | Changes to documentation only (README, comments, guides)                                                |
| `refactor` | Code restructuring that does not change external behavior (renaming, extracting functions, simplifying) |
| `test`     | Adding, updating, or fixing tests without changing production code                                      |
| `chore`    | Maintenance tasks that don't affect source code or tests (dependency bumps, config tweaks, .gitignore)  |
| `ci`       | Changes to CI/CD configuration and scripts (GitHub Actions, workflows, pipelines)                       |
| `build`    | Changes to the build system or external dependencies (Cargo.toml, justfile)                             |
| `perf`     | A code change that improves performance without altering functionality                                  |
| `style`    | Formatting-only changes (whitespace, semicolons, linting) with no logic changes                         |
| `design`   | Changes to visual or UI design assets and layout                                                        |
| `revert`   | Reverts a previous commit (reference the reverted commit hash in the body)                              |

**Examples**

```
feat(tui): add a random-episode key to the catalog pane
fix(seek): rebuild the decoder chain instead of seeking in place
refactor(catalog): fold enrichment failures into a partial catalog
docs(paths): document the $MFP_* overrides needed to isolate an instance
```

### Pull Requests

- Fill out the pull request template completely
- Keep the pull request focused and scoped to one change set
- Ensure tests and checks pass before requesting review
- Update documentation when behavior or interfaces change; a change to the wire protocol, the paths, or the key bindings also changes `README.md`
- Respond to review feedback and keep the branch up to date with the target branch

Maintainers may ask for changes, additional tests, or scope adjustments before merging.
