<p align="center">
  <a href="https://github.com/pivoshenko/musicforprogramming">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/pivoshenko/musicforprogramming/main/assets/preview_social_dark.svg" />
      <img alt="musicforprogramming - a terminal player for musicforprogramming.net, written in Rust" src="https://raw.githubusercontent.com/pivoshenko/musicforprogramming/main/assets/preview_social_light.svg" width="800" />
    </picture>
  </a>
</p>

<p align="center">
  <a href="https://github.com/pivoshenko/musicforprogramming/actions/workflows/ci.yaml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/pivoshenko/musicforprogramming/ci.yaml?style=flat-square&logo=github&logoColor=white&label=CI&color=0A6847"></a>
  <a href="https://github.com/pivoshenko/musicforprogramming/releases"><img alt="Release" src="https://img.shields.io/github/v/release/pivoshenko/musicforprogramming?style=flat-square&logo=github&logoColor=white&color=4856CD&label=Release"></a>
  <a href="https://crates.io/crates/mfp-tui"><img alt="Crates.io" src="https://img.shields.io/crates/v/mfp-tui?style=flat-square&logo=rust&logoColor=white&color=4856CD&label=Crates.io"></a>
  <img alt="Rust" src="https://img.shields.io/badge/Rust-Stable-F74C00?style=flat-square&logo=rust&logoColor=white">
  <a href="https://github.com/pivoshenko/musicforprogramming/blob/main/LICENSE"><img alt="License" src="https://img.shields.io/badge/License-MIT-0A6847?style=flat-square&logo=opensourceinitiative&logoColor=white"></a>
  <a href="https://stand-with-ukraine.pp.ua"><img alt="Stand with Ukraine" src="https://img.shields.io/badge/Stand_With-Ukraine-FFD700?style=flat-square&labelColor=0057B7"></a>
</p>

A terminal player for [musicforprogramming.net](https://musicforprogramming.net), written in Rust.

## Install

### Standalone Installer

```sh
curl -fsSL https://pivoshenko.dev/mfp.sh | sh
```

### Homebrew

```sh
brew install pivoshenko/tap/musicforprogramming
```

### Cargo

```sh
cargo install mfp-daemon
cargo install mfp-tui
```

### From Source

Requires a Rust toolchain matching `rust-toolchain.toml` (stable, 1.96+), and `libasound2-dev` on Linux.

```sh
cargo install --path crates/mfp-daemon
cargo install --path crates/mfp-tui
```

Both binaries must be on `PATH`: `mfp` autostarts `mfp-daemon` when nothing is listening on the socket, looking for it beside itself and then on `PATH`. `$MFP_DAEMON` overrides that.

## Use

Run `mfp` with no arguments for the TUI and with a subcommand it prints one line and exits, so it composes with anything:

```sh
mfp play
mfp toggle
mfp next / mfp prev
mfp seek +30
mfp download <slug>
mfp list --json
mfp status --json
mfp shutdown
```

Exit codes: `0` ok, `1` the daemon rejected the command, `2` usage error, `3` the daemon was unreachable.

### Keys

| Key                 | Action                          |
| ------------------- | ------------------------------- |
| `space`             | Play / pause                    |
| `u`                 | Stop                            |
| `n` / `p`           | Next / previous episode         |
| `h` / `l`           | Seek back / forward             |
| `H` / `L`           | Seek back / forward, longer     |
| `j` / `k`           | Move the selection              |
| `g` / `G`           | Jump to the top / bottom        |
| `ctrl+d` / `ctrl+u` | Half-page down / up             |
| `r`                 | Play a random episode           |
| `f`                 | Favourite the selection         |
| `d` / `x`           | Download / delete the selection |
| `/`                 | Search                          |
| `?`                 | Help                            |
| `q`                 | Quit                            |

## Configuration

`config.toml` is optional - every setting has a default, and a missing file is not an error. A file that exists but does not parse is.

```toml
# Seconds the daemon may idle with nothing playing, nothing downloading, and no client
# attached before it exits. Omitted, it idles indefinitely
idle_timeout_secs = 900

# Downloads that may transfer at once; the rest queue
max_concurrent_downloads = 2
```

### Paths

| What                               | Where                                                                   | Override          |
| ---------------------------------- | ----------------------------------------------------------------------- | ----------------- |
| `config.toml`                      | `~/.config/mfp`                                                         | `$MFP_CONFIG_DIR` |
| Catalog cache and downloaded audio | `~/.cache/mfp`                                                          | `$MFP_CACHE_DIR`  |
| `state.json` and `daemon.log`      | `~/.local/state/mfp`                                                    | `$MFP_STATE_DIR`  |
| Daemon socket                      | `$XDG_RUNTIME_DIR/mfp/daemon.sock`, else `$TMPDIR/mfp-$UID/daemon.sock` | `$MFP_SOCKET`     |

## Layout

| Crate        | What it holds                                                           |
| ------------ | ----------------------------------------------------------------------- |
| `mfp-core`   | Episode and catalog model, the wire protocol, config, every path        |
| `mfp-daemon` | Audio engine, streaming and downloads, the socket server, durable state |
| `mfp-tui`    | The `mfp` binary: argument parsing, the socket client, the interface    |
