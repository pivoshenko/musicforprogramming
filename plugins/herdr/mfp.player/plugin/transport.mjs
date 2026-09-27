// Runs one transport subcommand, then reports where it left the player.
//
// The player's CLI already prints a state line, but a plugin command's stdout goes to the
// Herdr command log rather than the screen, so the state is read back and notified instead.
//
// Usage: node plugin/transport.mjs <play-stop|play|toggle|next|prev|stop>

import { mfp, notify, readState, report, signature } from "./player.mjs";

const VERB = process.argv[2];

if (!VERB) {
  process.stderr.write("usage: transport.mjs <play-stop|play|toggle|next|prev|stop>\n");
  process.exit(2);
}

// Read before acting: what the state was is the only way to recognise the command taking effect,
// since the daemon answers the moment it accepts one. It also decides what `play-stop` resolves to
const start = readState();
const before = signature(start);

/**
 * `play-stop` is one key for starting and stopping, so it has to look before it acts: nothing
 * loaded means play, anything else means stop. Every other verb passes straight through.
 */
function resolve(verb) {
  if (verb !== "play-stop") return verb;
  if (!start.ok) return "play";
  const idle = !start.state.episode || start.state.playback === "stopped";
  return idle ? "play" : "stop";
}

const resolved = resolve(VERB);
const result = mfp(resolved);

if (result.error) {
  notify("mfp is not on PATH", "Install the player, or put mfp on PATH, to drive it from Herdr.");
  process.exit(1);
}

// Exit 1 is the daemon refusing the command, 3 is a daemon that could not be reached. Either
// way the player did not move, so say what went wrong rather than report an unchanged state
if (result.status !== 0) {
  const reason = (result.stderr || result.stdout || "").trim();
  notify(`mfp could not ${resolved}`, reason || `The player exited ${result.status}.`);
  process.exit(1);
}

process.exit(report(before));
