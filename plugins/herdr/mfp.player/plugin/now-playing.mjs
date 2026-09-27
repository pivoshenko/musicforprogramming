// Reports what the player is doing, without changing anything.
//
// Unsettled on purpose: this answers "what is it doing right now", so a genuine `loading` is
// the honest answer rather than something to wait out.

import { report } from "./player.mjs";

process.exit(report());
