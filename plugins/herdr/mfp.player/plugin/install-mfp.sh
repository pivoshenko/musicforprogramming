#!/bin/sh
# Ensures the player is installed, because every action in this plugin shells out to it.
#
# Runs as a `[[build]]` step, so it happens once during `herdr plugin install` rather than on
# every action. `herdr plugin link` skips build steps, so a linked development copy is expected
# to have the player already.
#
# Never aborts the plugin install. A missing player is something the actions already report at
# runtime, and refusing to install a plugin because a machine is offline is worse than
# installing one that says what it needs.

set -eu

INSTALLER="https://pivoshenko.dev/mfp.sh"

log() {
    printf 'mfp.player: %s\n' "$1" >&2
}

if command -v mfp > /dev/null 2>&1; then
    log "found $(command -v mfp), leaving it alone"
    exit 0
fi

if ! command -v curl > /dev/null 2>&1; then
    log "mfp is not installed and curl is not available - install it yourself: ${INSTALLER}"
    exit 0
fi

log "mfp is not on PATH, installing it from ${INSTALLER}"

if ! curl -fsSL "${INSTALLER}" | sh; then
    log "the installer failed - install mfp yourself and the actions will start working"
    exit 0
fi

# The installer's directory - ~/.local/bin unless $MFP_INSTALL_DIR says otherwise - is not
# necessarily on the PATH the Herdr server hands to plugin commands. Saying so now beats every
# action failing with "not on PATH" later
if command -v mfp > /dev/null 2>&1; then
    log "installed $(command -v mfp)"
else
    log "installed, but mfp is still not on PATH - add the directory above to PATH, then restart Herdr"
fi
