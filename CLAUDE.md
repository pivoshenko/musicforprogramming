# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

A terminal player for [musicforprogramming.net](https://musicforprogramming.net), written in Rust. A
three-crate cargo workspace: `mfp-core` (shared model, wire protocol, config, paths), `mfp-daemon`
(the `mfp-daemon` binary), `mfp-tui` (the `mfp` binary - both the interface and the scriptable CLI).

macOS and Linux only. The client reaches the daemon over a Unix domain socket and the state directory
is chmodded through `PermissionsExt`, so there is no Windows target and CI has no Windows leg.
`AGENTS.md` is a symlink to this file.

`plugins/herdr/mfp.player` is a Herdr plugin, not Rust: a `herdr-plugin.toml` manifest over a few
scripts in `plugin/`. Each action runs an `mfp` subcommand and then reports the state it left behind,
because a plugin command's stdout goes to Herdr's command log rather than the screen. The pane starts
the daemon through a throwaway client before launching the interface, so the daemon is never a child
of a pane somebody dismisses. It is outside the workspace, so `just check` does not see it - changing
a CLI verb or an exit code is what would break it.

## Commands

`just` is the task runner - `just --list` for the full set, or the recipe table in `CONTRIBUTING.md`.
`just check` is `lint` + `audit` + `test` + `build`, and CI runs those same recipes, so a green
`just check` locally means a green CI.

```bash
cargo test -p mfp-core                  # one crate
cargo test seek                         # by test-name substring
cargo test -- --nocapture               # keep stdout
cargo test -p mfp-daemon -- --ignored   # the tests skipped by default
```

The default run is hermetic - no network, no audio device - so a failure is a real failure rather
than a sandbox artifact. Anything that needs the real world is `#[ignore]`d with its reason in the
attribute: an audio output device, the network, or a benchmark. `mfp-daemon/tests/end_to_end.rs`
spawns the binary and is audible, and the player has no volume of its own.

`just generate-changelog` regenerates `CHANGELOG.md` from the commit history with git-cliff.
`just generate-social-preview` rasterizes `assets/preview_social_dark.svg` into the 1280x640 PNG for
GitHub's social preview; that SVG's colours come from `crates/mfp-tui/src/ui/theme.rs`, so a palette
change belongs in both.

## Architecture

**The split.** The daemon is the sole authority over playback and download state and outlives every
client - closing a pane must not interrupt audio, which is why `ctrl+c` in the interface only
detaches and `q` is the one key that shuts the daemon down. It exits only on an explicit
`shutdown` or the configured `idle_timeout_secs` (unset by default, meaning never). `mfp`
autostarts `mfp-daemon` when nothing is listening on the socket, looking beside itself and then on
`PATH`; `$MFP_DAEMON` overrides.

**Concurrency.** `rodio` blocks and wants an OS thread of its own, while the socket server and the
downloads want `tokio`. `audio/` runs on one dedicated thread consuming a command channel and never
awaits; everything else runs on the runtime and never blocks on audio. They meet at
`state::SharedState` (an `Arc<Mutex<StateSnapshot>>`), with `StateStore` (`state.json`) as the durable
layer beneath it. `state::lock` recovers from poisoning rather than propagating it.

**Transport.** `ipc/` is a Unix domain socket speaking newline-delimited JSON: a client writes
`Request` lines and reads `Frame` lines, each either a `Response` echoing a request's `id` or an
unsolicited `EventFrame` carrying a whole `StateSnapshot`, never a delta. Pushes are level-triggered
on a poll of the shared state, not edge-triggered per change, so socket traffic stays bounded however
chatty playback becomes. `mfp-tui/src/client.rs` is a synchronous `UnixStream` with read timeouts on
purpose - the interface is a blocking `ratatui` loop, not a second async runtime.

**Dispatch seam.** `ipc/server.rs` defines the `Player` trait and dispatches against it rather than
against the engine, so framing, ordering, error codes, and the lifecycle are testable over a real
socket with no audio device. Every method returns as soon as a command is *accepted*; clients learn
the consequence from the next snapshot.

**Catalog.** `catalog/` resolves `feed` (authoritative, a failure fails the catalog) then `enrich`
(best-effort from the site's client bundle, the only source of track listings; every failure is
logged at debug and skipped), behind the disk cache in `cache` with a 6-hour TTL. A stale cache is
served when a refresh fails. This is what lets the player start with no network.

**Analyser.** `audio/spectrum.rs` reproduces Web Audio's `getByteFrequencyData` - mono downmix, Hann
window, 2048-point transform, exponential smoothing against the previous frame, linear scaling from
`SPECTRUM_MIN_DB` to `SPECTRUM_MAX_DB` onto `0..=255` - so a client can run the site's own analyser
arithmetic unchanged.

**Updates.** `mfp-tui/src/update/` is the only part of the client that reaches the network, and it
reaches GitHub rather than the site. `mod.rs` resolves the latest release and decides whether this
copy is ours to replace, `install.rs` verifies an archive and swaps `mfp` and `mfp-daemon` as a pair,
and `notice.rs` is the once-a-day background check whose answer lands in `~/.cache/mfp` for a later
run to mention. Nothing here is on any playback path, and `reqwest`'s blocking client is used on
purpose: the client is synchronous and two HTTP requests do not justify a runtime.

**Exit codes.** `0` ok, `1` the daemon rejected the command, `2` usage error, `3` the daemon was
unreachable. They are part of the CLI's contract with whatever scripts it.

## Invariants

- Never rename or respell anything already named in `protocol.rs`. The newline-delimited JSON is a
  contract: fields may be added and readers must ignore what they do not recognise, but an existing
  name is frozen
- Never branch on an error's human-readable message. `ErrorCode` in `error.rs` is the stable wire
  value, and it is `#[non_exhaustive]` with an `Unknown` catch-all so a newer daemon's added code
  costs a client one code rather than the whole line
- Never edit `App::snapshot` locally in the interface. A keypress sends a command and waits for the
  daemon to report the consequence - drawing what was asked for is how an interface starts lying
  about what is playing
- Never seek in place on a live decoder. A backward seek fails *and wedges the decoder permanently*,
  after which `get_pos` keeps advancing and the position readout silently lies. `audio/seek.rs`
  rebuilds the chain at the target instead, which is why position is `base_offset + decoder position`
  and why a seek is an observable state rather than a synchronous call
- Never await on the audio thread, and never block the tokio runtime on it
- Never scope `$MFP_SOCKET` alone to isolate an instance. Isolation takes all four of `$MFP_SOCKET`,
  `$MFP_CONFIG_DIR`, `$MFP_CACHE_DIR`, and `$MFP_STATE_DIR` - scoping the socket leaves two daemons
  writing one `state.json` and one cache
- Never publish a partial transfer under its final name. `download/` writes `<identifier>.mp3.part`
  and renames only once the length matches what the feed declares
- Never add a second single-instance lock. The socket path *is* the lock; with no PID file beside it,
  the lock and the endpoint cannot disagree
- Never fetch the catalog on a timer or in the background. Every catalog request is reached from
  `cache::load`, called only when a user action needs data the cache cannot satisfy. The once-a-day
  version check is the one background request, and it touches nothing the player reads
- Never bump the version by hand. Releases are `workflow_dispatch`-only, and git-cliff resolves the
  version, bumps `Cargo.toml`, tags, and publishes
- Never let the version check make a command slower or make one fail. It runs on a detached thread,
  every error is dropped, and what is ever displayed is what a *previous* check recorded
- Never write over a binary a package manager placed. `self update` refuses under Homebrew and cargo
  and names their upgrade command instead, because a swapped binary is an install that reports itself
  as healthy while no longer matching the hash its manager recorded
- Never unpack a release archive to a path the archive chose. An entry is matched by file name, read
  into memory, and written only beside the destination the client resolved itself

## Cross-Cutting Changes

**Adding a command** touches the `Commands` enum in `mfp-tui/src/cli.rs`, a `Command` variant in
`mfp-core/src/protocol.rs`, the `Player` trait and its dispatch arm in `mfp-daemon/src/ipc/server.rs`,
the README's command list, and a key binding in `mfp-tui/src/ui/app.rs` if it should also be reachable
from the interface.

**Adding a field to the snapshot** means `StateSnapshot` in `protocol.rs`, whatever publishes it in
`mfp-daemon/src/state.rs`, and the pane in `mfp-tui/src/ui/panes.rs` that renders it. Adding is safe;
renaming is not.

**Adding a subcommand that never reaches the daemon** - `self update` is the only one - touches
`mfp-tui/src/cli.rs` and nothing in `protocol.rs`. The two predicates in `cli.rs` decide whether it
gets an update notice wrapped around it and whether its stdout is a document.

**Adding a path** goes in `mfp-core/src/paths.rs` with its `$MFP_*` override, then into the Paths
table in the README. A path with no override cannot be isolated, which breaks the suite's ability to
run against a scratch instance.

## Conventions

- Every module opens with a `//!` doc comment stating what it owns and, where the answer is not
  obvious, why it is built that way. The existing ones carry real constraints; keep that bar
- The workspace lint set in `Cargo.toml` is deliberately narrow: lints that catch a defect, not lints
  that enforce a house style. The pedantic and nursery groups are off on purpose
- `unwrap`, `expect`, `panic`, and `float_cmp` are denied in production code and re-allowed per crate
  under `#![cfg_attr(test, allow(...))]`, where a panic is the failure report
- Tests live both inline as `#[cfg(test)] mod tests` and in each crate's `tests/` directory; the
  latter is for what needs a process or a real socket (`mfp-daemon/tests/end_to_end.rs`,
  `mfp-tui/tests/render.rs`, `mfp-core/tests/paths_env.rs`)
- Test names are sentences describing the behaviour, not the function under test:
  `a_corrupt_cache_file_is_a_miss_rather_than_an_error`
- `ui/panes.rs` functions are pure draws from `App` and a rectangle. A pane that decided anything
  would be a second place the interface's behaviour lived
- `ui/anim.rs` reads no clock: every function maps a tick count the event loop owns to that tick's
  frame, so the loop can stop ticking and still draw a correct resting frame
- `ui/theme.rs` is the only file holding hex values; widgets ask for a role, never a hue
- Commits and branches follow `CONTRIBUTING.md`: Conventional Commits with a crate or module scope
