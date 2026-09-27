// Shared plumbing: read the player's state, and render it as a Herdr notification.
//
// Everything reads `mfp status --json` rather than the human-readable line. The JSON document
// is the player's machine-readable contract, and it stays a valid document even when the
// daemon is unreachable, which is a state to report rather than crash on.

import { spawnSync } from "node:child_process";

const HERDR = process.env.HERDR_BIN_PATH ?? "herdr";

/** How playback states are worded, matching what the player's own CLI prints. */
const VERBS = {
  playing: "Playing",
  paused: "Paused",
  loading: "Loading",
  seeking: "Seeking",
  stopped: "Stopped",
  error: "Playback error",
};

/**
 * A transport command returns as soon as the daemon *accepts* it; the consequence arrives in a
 * later snapshot. The snapshot in between is usually the state the command was meant to change,
 * so reporting the first one read announces the previous episode - or worse, the previous
 * action's. Poll until the state actually differs from what it was before the command.
 */
const SETTLE_TIMEOUT_MS = 2500;
const SETTLE_INTERVAL_MS = 100;

/** A true synchronous sleep, so polling costs no CPU in a one-shot process. */
function sleep(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/**
 * What a transport command is expected to change: which episode is loaded, and what playback is
 * doing with it. Position deliberately excluded - it moves on its own while playing, so it
 * would make every state look new.
 */
export function signature(result) {
  if (!result.ok) return "";
  return `${result.state.playback}:${result.state.episode?.slug ?? ""}`;
}

export function notify(title, body) {
  const args = ["notification", "show", title];
  if (body) args.push("--body", body);
  spawnSync(HERDR, args, { stdio: ["ignore", "ignore", "inherit"] });
}

/**
 * Formats seconds as `H:MM:SS`, clamped to non-negative, matching the player's own output.
 */
export function hms(seconds) {
  const total = Math.round(Math.max(0, Number(seconds) || 0));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
}

/**
 * Runs an `mfp` subcommand, returning the spawn result. Never throws: a missing binary comes
 * back as `error`, which callers report rather than propagate.
 */
export function mfp(...args) {
  return spawnSync("mfp", args, { encoding: "utf8" });
}

/**
 * Reads the current state. Returns `{ ok: false, title, body }` for anything that should be
 * reported instead of a state - no binary, no document, unreachable daemon.
 */
export function readState() {
  const result = mfp("status", "--json");

  if (result.error) {
    return {
      ok: false,
      title: "mfp is not on PATH",
      body: "Install the player, or put mfp on PATH, to drive it from Herdr.",
    };
  }

  let state;
  try {
    state = JSON.parse(result.stdout);
  } catch {
    return {
      ok: false,
      title: "mfp returned no status",
      body: (result.stderr || "").trim() || "The status document did not parse.",
    };
  }

  // Exit code 3 with a valid document on stdout: the daemon could not be reached or started
  if (state.unreachable) {
    return {
      ok: false,
      title: "mfp daemon unreachable",
      body: state.error ?? "The daemon could not be started.",
    };
  }

  return { ok: true, state };
}

/**
 * Reads the state once it differs from `before`, or once the timeout runs out.
 *
 * Also waits out `loading`, which is a state on the way to the one worth reporting. A command
 * that changed nothing - `play` on an already-playing player - simply costs the timeout and then
 * reports the truth, which is better than reporting a stale snapshot quickly.
 */
export function readChangedState(before) {
  const deadline = Date.now() + SETTLE_TIMEOUT_MS;
  let latest = readState();

  while (
    latest.ok &&
    Date.now() < deadline &&
    (latest.state.playback === "loading" || signature(latest) === before)
  ) {
    sleep(SETTLE_INTERVAL_MS);
    latest = readState();
  }

  return latest;
}

/** Turns a state into `{ title, body }`, the two halves of the notification. */
export function describe(state) {
  if (!state.episode) {
    return { title: "mfp", body: "Nothing is loaded." };
  }

  const verb = VERBS[state.playback] ?? "Unknown";
  const elapsed = hms(state.position_secs);
  const total =
    state.duration_secs == null
      ? "unknown"
      : `${state.duration_approximate ? "~" : ""}${hms(state.duration_secs)}`;

  // A seek in flight reports its target, never the position it is moving away from
  const seeking =
    state.seek_target_secs == null ? "" : `, seeking to ${hms(state.seek_target_secs)}`;

  return {
    title: `${verb}: ${state.episode.title}`,
    body: `${elapsed} / ${total}${seeking}`,
  };
}

/**
 * Notifies the current state and returns the process exit code: 0 when a state was read, 1 when
 * something had to be reported instead.
 *
 * `before` is the signature the player had prior to a transport command, and waits for the state
 * to move off it. Omitted, the first snapshot read is reported as-is.
 */
export function report(before = null) {
  const result = before === null ? readState() : readChangedState(before);

  if (!result.ok) {
    notify(result.title, result.body);
    return 1;
  }

  const { title, body } = describe(result.state);
  notify(title, body);
  process.stdout.write(`${title} - ${body}\n`);
  return 0;
}
