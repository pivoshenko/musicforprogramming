# mfp.player

A [Herdr](https://herdr.dev) plugin that drives the player from the workspace it is already in:
the interface in a pane, and the transport on actions.

## Install

```sh
herdr plugin install pivoshenko/musicforprogramming/plugins/herdr/mfp.player
```

Local development, from a checkout of this repository:

```sh
herdr plugin link plugins/herdr/mfp.player
```

`link` registers the directory in place and skips the build step, so an edit to the manifest or a
script takes effect on the next invocation.

## What It Provides

| Kind   | Id            | What it does                                |
| ------ | ------------- | ------------------------------------------- |
| Pane   | `player`      | The interface, as an overlay pane           |
| Action | `open`        | Opens that pane, so a key can reach it      |
| Action | `play`        | Start playing, or resume                    |
| Action | `toggle`      | Play / pause the loaded episode             |
| Action | `next`        | Play the next episode                       |
| Action | `prev`        | Play the previous episode                   |
| Action | `stop`        | Stop playback and unload the episode        |
| Action | `now-playing` | Reports the current state as a notification |

Every action is `workspace` and `global`: playback is not scoped to the workspace a key was pressed
in, and a transport key that only worked in some panes would read as broken.

`toggle` acts on the loaded episode, so on a daemon that has just started with nothing loaded it is
correctly a no-op - `play` is the verb that starts from cold.

## What It Reports

Every transport action announces where it left the player, so a keypress is answered without opening
anything:

```
Playing: Episode 76: Material Object
0:00:00 / ~2:14:02
```

This is read back from `mfp status --json` rather than assumed from the command that was sent. A
transport command returns as soon as the daemon *accepts* it and the consequence lands in a later
snapshot, so the wrapper records the state before acting and waits for it to change - up to 2.5
seconds - before reporting. Reporting the first snapshot read instead announces the previous episode,
or the previous action's.

A command that changes nothing, such as `play` on a player already playing, costs that wait and then
reports the truth. An unreachable daemon, a missing `mfp`, or a refused command is reported as itself
rather than as a state.

```sh
herdr plugin pane open --plugin mfp.player --entrypoint player
herdr plugin action invoke mfp.player.toggle
herdr plugin action list --plugin mfp.player
```

## Dismissing The Pane

`prefix+x` - Herdr's own `close_pane` - dismisses it and leaves playback running. `q` inside the
interface is the other thing entirely: quitting the player stops playback and ends the daemon with
it, which is what quitting a player should mean.

That only holds because the pane runs [`plugin/open-player.sh`](plugin/open-player.sh) rather than
`mfp` directly. `mfp` autostarts the daemon, which would make the daemon a child of the pane, and
closing the pane would take playback down with it. The script warms the daemon through a throwaway
client first, so it is reparented outside the pane before the interface starts and the interface
spawns nothing.

## Keybindings

Plugin v1 declares no keys of its own, so these go in your own `config.toml`. Suggested defaults,
which assume the stock `prefix = "ctrl+b"`:

```toml
[[keys.command]]
key = "prefix+ctrl+m"
type = "plugin_action"
command = "mfp.player.open"
description = "mfp: player"

[[keys.command]]
key = "prefix+ctrl+t"
type = "plugin_action"
command = "mfp.player.toggle"
description = "mfp: play/pause"

[[keys.command]]
key = "prefix+ctrl+f"
type = "plugin_action"
command = "mfp.player.next"
description = "mfp: next"

[[keys.command]]
key = "prefix+ctrl+comma"
type = "plugin_action"
command = "mfp.player.prev"
description = "mfp: previous"

[[keys.command]]
key = "prefix+ctrl+i"
type = "plugin_action"
command = "mfp.player.now-playing"
description = "mfp: now playing"
```

`m` for music, `t` for toggle, `f` forward, `comma` for the `<` key, `i` for info. Then:

```sh
herdr config check          # reports a reserved or duplicated key rather than failing silently
herdr server reload-config
```

Do not bind anything to `prefix+ctrl+b` while the prefix is `ctrl+b`: pressing the prefix twice sends
a literal prefix key, and `config check` reports the binding as disabled.

## Requirements

`mfp` and `mfp-daemon` must be on `PATH`, and `node` must be too - every action but `open` runs a
script. The player autostarts its daemon when nothing is listening on the socket, so nothing here
needs it to already be running.

`herdr plugin install` handles the player for you: a `[[build]]` step runs
[`plugin/install-mfp.sh`](plugin/install-mfp.sh), which installs it from
<https://pivoshenko.dev/mfp.sh> only when `mfp` is not already on `PATH`, so a Homebrew or cargo copy
is left alone. A failure there warns rather than aborting the install, because the actions already
report a missing player at runtime.

Two things that step cannot do. `herdr plugin link` skips build commands, so a linked development
copy needs the player installed already. And the installer writes to `~/.local/bin`, which is not
necessarily on the `PATH` the Herdr server hands to plugin commands - if the actions still report
`mfp is not on PATH` afterwards, that is why, and Herdr needs restarting once it is.

macOS and Linux only, which is what the player itself supports.
