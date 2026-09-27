#!/bin/sh
# Launches the interface in a Herdr pane, with the daemon started outside it.
#
# `mfp` autostarts the daemon itself, which would make the daemon a child of this pane. Closing
# the pane then takes the daemon down with it - and playback with it - which is the opposite of
# what the split exists for: the daemon is the authority over playback and is meant to outlive
# every client.
#
# So warm the daemon through a throwaway client first. That client spawns it in its own process
# group and exits immediately, leaving the daemon reparented to init, and the interface below then
# finds a daemon already listening and spawns nothing. Closing the pane kills only the interface.

set -eu

mfp status > /dev/null 2>&1 || true

exec mfp
