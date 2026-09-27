# CLAUDE.md

musicforprogramming is a terminal player for [musicforprogramming.net](https://musicforprogramming.net),
written in Rust. A background daemon owns playback, downloads, and durable state; `mfp` is both the
interface you look at and the CLI you script against, and it holds no authoritative state of its own.
The repo is a three-crate cargo workspace: `mfp-core` (shared model, wire protocol, config, paths),
`mfp-daemon` (the `mfp-daemon` binary), and `mfp-tui` (the `mfp` binary).

macOS and Linux only. The client reaches the daemon over a Unix domain socket and the state directory
is chmodded through `PermissionsExt`, so there is no Windows target and CI does not have a Windows leg.

## Never

- never rename or respell a field already named in `protocol.rs`. The newline-delimited JSON is a
  contract: new fields may be added and readers must ignore what they do not recognise, but an
  existing name is frozen
- never branch on an error's human-readable message. `ErrorCode` in `error.rs` is the stable wire
  value; the message is not, and is never to be parsed
- never edit `App::snapshot` locally in the interface. A keypress sends a command and waits for the
  daemon to report the consequence - drawing what was asked for is how an interface starts lying
  about what is playing
- never seek in place on a live decoder. A backward seek fails *and wedges the decoder permanently*,
  after which `get_pos` keeps advancing and the position readout silently lies. `audio/seek.rs`
  rebuilds the chain at the target instead, which is why position is `base_offset + decoder position`
  and why a seek is an observable state rather than a synchronous call
- never await on the audio thread, and never block the tokio runtime on it. `rodio` blocks and owns
  one dedicated OS thread; the two sides meet only at `state::SharedState`
- never scope `$MFP_SOCKET` alone to isolate an instance. Isolation takes all four of `$MFP_SOCKET`,
  `$MFP_CONFIG_DIR`, `$MFP_CACHE_DIR`, and `$MFP_STATE_DIR` - scoping the socket leaves two daemons
  writing one `state.json` and one cache
- never publish a partial transfer under its final name. `download/` writes `<identifier>.mp3.part`
  and renames only once the length matches what the feed declares
- never add a second single-instance lock. The socket path *is* the lock; with no PID file beside it,
  the lock and the endpoint cannot disagree
- never bump the version by hand. Releases are `workflow_dispatch`-only, and git-cliff resolves the
  version, bumps `Cargo.toml`, tags, and publishes
- never fetch on a timer or in the background. Every catalog request is reached from `cache::load`,
  called only when a user action needs data the cache cannot satisfy

## Commands

`just` is the task runner - `just --list`, or the recipe table in `CONTRIBUTING.md`, for the full set.
`just check` is lint + audit + test + build, and CI runs those same recipes, so a green `just check`
locally means a green CI.

```bash
cargo test -p mfp-core                         # one crate
cargo test seek                                # by test-name substring
cargo test -- --nocapture                      # keep stdout
```

The default run is hermetic - no network, no audio device - so a failure is a real failure, not a
sandbox artifact. Tests that touch the real world are `#[ignore]`d with the reason in the attribute
and run explicitly with `-- --ignored`; `mfp-daemon/tests/end_to_end.rs` is audible, and the player
has no volume of its own.

`just generate-changelog` regenerates `CHANGELOG.md` from the commit history with git-cliff and
`cliff.toml`. `just generate-social-preview` rasterizes `assets/preview_social_dark.svg` into the
1280x640 PNG for GitHub's Settings, Social preview - the SVG's colours come from
`crates/mfp-tui/src/ui/theme.rs`, so a palette change belongs in both.

## Conventions

- every module opens with a `//!` doc comment stating what it owns and, where the answer is not
  obvious, why it is built that way. The existing ones carry real constraints; keep that bar
- the workspace lint set in `Cargo.toml` is deliberately narrow: lints that catch a defect, not lints
  that enforce a house style. The pedantic and nursery groups are off on purpose
- `unwrap`, `expect`, and `panic` are denied in production code and re-allowed per crate under
  `#[cfg_attr(test, allow(...))]`, where a panic is the failure report
- tests live both inline as `#[cfg(test)] mod tests` and in each crate's `tests/` directory; the
  latter is for what needs a process or a real socket (`mfp-daemon/tests/end_to_end.rs`,
  `mfp-tui/tests/render.rs`, `mfp-core/tests/paths_env.rs`)
- test names are sentences describing the behaviour, not the function under test:
  `a_corrupt_cache_file_is_a_miss_rather_than_an_error`
- `ui/panes.rs` functions are pure draws from `App` and a rectangle. A pane that decided anything
  would be a second place the interface's behaviour lived
- `ui/anim.rs` reads no clock: every function maps a tick count the event loop owns to that tick's
  frame, so the loop can stop ticking and still draw a correct resting frame
- `ui/theme.rs` is the only file holding hex values; widgets ask for a role, never a hue
- commits and branches follow `CONTRIBUTING.md`: Conventional Commits with a crate or module scope
- `AGENTS.md` is a symlink to this file

## Cross-Cutting Changes

**Adding a command** touches the `Commands` enum in `mfp-tui/src/cli.rs`, a `Command` variant in
`mfp-core/src/protocol.rs`, a dispatch arm in `mfp-daemon/src/ipc/server.rs`, the README's command
list, and a key binding in `mfp-tui/src/ui/app.rs` if it should also be reachable from the interface.

**Adding a field to the snapshot** means `StateSnapshot` in `protocol.rs`, whatever publishes it in
`mfp-daemon/src/state.rs`, and the pane in `mfp-tui/src/ui/panes.rs` that renders it. Adding is safe;
renaming is not.

**Adding a path** goes in `mfp-core/src/paths.rs` with its `$MFP_*` override, and then into the Paths
table in the README. A path with no override cannot be isolated, which breaks the test suite's ability
to run against a scratch instance.

## Architecture

**The split.** The daemon is the sole authority over playback and download state and outlives every
client - closing a pane must not interrupt audio. It exits only on an explicit `shutdown` or a
configured idle timeout (`idle_timeout_secs`, unset means never). `mfp` autostarts `mfp-daemon` when
nothing is listening on the socket, looking beside itself and then on `PATH`; `$MFP_DAEMON` overrides.

**Concurrency.** `rodio` blocks and wants a thread of its own, while the socket server and the
downloads want `tokio`. `audio/` runs on one dedicated OS thread consuming a command channel and never
awaits; everything else runs on the runtime and never blocks on audio. They meet at
`state::SharedState`, with `StateStore` (`state.json`) as the durable layer beneath it.

**Transport.** `ipc/` is a Unix domain socket speaking newline-delimited JSON: a client writes
`Request` lines and reads `Frame` lines, each either a `Response` echoing a request's `id` or an
unsolicited `EventFrame` carrying a full `StateSnapshot`. `mfp-tui/src/client.rs` is synchronous on
purpose - the interface is a blocking `ratatui` loop, so the client is a plain `UnixStream` with read
timeouts rather than a second async runtime.

**Catalog.** `catalog/` resolves `feed` then `enrich`, behind the disk cache in `cache`. A feed
failure fails the catalog; an enrichment failure still yields one, without track listings. The cache
is what lets the player start with no network.

**Analyser.** `audio/spectrum.rs` reproduces Web Audio's `getByteFrequencyData` - mono downmix, Hann
window, 2048-point transform, exponential smoothing against the previous frame, linear scaling from
`SPECTRUM_MIN_DB` to `SPECTRUM_MAX_DB` onto `0..=255` - so a client can run the site's own analyser
arithmetic unchanged.

**Exit codes.** `0` ok, `1` the daemon rejected the command, `2` usage error, `3` the daemon was
unreachable. They are part of the CLI's contract with whatever scripts it.
