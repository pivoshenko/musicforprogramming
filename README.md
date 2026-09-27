# musicforprogramming

A terminal player for [musicforprogramming.net](https://musicforprogramming.net), written in
Rust. A background daemon owns playback and downloads; `mfp` is the interface you look at and
the CLI you script against.

## Install

Requires a Rust toolchain matching `rust-toolchain.toml` (stable, 1.96+).

```sh
cargo install --path crates/mfp-daemon
cargo install --path crates/mfp-tui
```

Both binaries must be on `PATH`: `mfp` autostarts `mfp-daemon` when nothing is listening on
the socket, looking for it beside itself and then on `PATH`. `$MFP_DAEMON` overrides that.

## Use

Run `mfp` with no arguments for the interface. With a subcommand it prints one line and
exits, so it composes with anything:

```sh
mfp play              # resume, or `mfp play <slug>` for a named episode
mfp toggle
mfp next / mfp prev
mfp seek +30          # or -30, or an absolute H:MM:SS
mfp download <slug>   # returns immediately; the daemon transfers in the background
mfp list --json
mfp status --json
mfp shutdown
```

Exit codes: `0` ok, `1` the daemon rejected the command, `2` usage error, `3` the daemon was
unreachable.

### Keys

| Key | Action |
| --- | --- |
| `space` | Play / pause |
| `u` | Stop |
| `n` / `p` | Next / previous episode |
| `h` / `l` | Seek back / forward |
| `H` / `L` | Seek back / forward, longer |
| `j` / `k` | Move the selection |
| `g` / `G` | Jump to the top / bottom |
| `ctrl+d` / `ctrl+u` | Half-page down / up |
| `r` | Play a random episode |
| `f` | Favourite the selection |
| `d` / `x` | Download / delete the selection |
| `/` | Search |
| `?` | Help |
| `q` | Quit |

## Configuration

`config.toml` is optional - every setting has a default, and a missing file is not an error.
A file that exists but does not parse is.

```toml
# Seconds the daemon may idle with nothing playing, nothing downloading, and no client
# attached before it exits. Omitted, it idles indefinitely
idle_timeout_secs = 900

# Downloads that may transfer at once; the rest queue
max_concurrent_downloads = 2
```

## Paths

| What | Where | Override |
| --- | --- | --- |
| `config.toml` | `~/.config/mfp` | `$MFP_CONFIG_DIR` |
| Catalog cache and downloaded audio | `~/.cache/mfp` | `$MFP_CACHE_DIR` |
| `state.json` and `daemon.log` | `~/.local/state/mfp` | `$MFP_STATE_DIR` |
| Daemon socket | `$XDG_RUNTIME_DIR/mfp/daemon.sock`, else `$TMPDIR/mfp-$UID/daemon.sock` | `$MFP_SOCKET` |

Downloads live under the cache directory rather than `~/Library/Caches` on macOS: they are a
library you browse, not something the system should reclaim behind your back.

## Layout

| Crate | What it holds |
| --- | --- |
| `mfp-core` | Episode and catalog model, the wire protocol, config, every path |
| `mfp-daemon` | Audio engine, streaming and downloads, the socket server, durable state |
| `mfp-tui` | The `mfp` binary: argument parsing, the socket client, the interface |

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

The lint set in `Cargo.toml` is deliberately narrow: lints that catch a defect, not lints that
enforce a house style. `unwrap`, `expect`, and `panic` are denied in production code and
allowed in tests, where a panic is the failure report.

## License

MIT - see [LICENSE](LICENSE).

Audio and catalog belong to [musicforprogramming.net](https://musicforprogramming.net); this
is an unofficial client. It identifies itself in every request, upstream being a small
independent host.
