// Intake strictness mode: how aggressively raw input gets routed through
// `daruma_plan_materialize` (plan-only intake, ADR-0007) vs. tracked
// directly as a task. daruma OSS has no maturity pipeline (that's a
// mcpbox-only concept) — this only gates plan decomposition.
//
//   off   — direct daruma work; never force decomposition into a plan.
//   lite  — decompose into a plan only on explicit request or for work
//           that is obviously multi-step. (default)
//   full  — assess every substantive request for "rawness": raw ideas
//           get materialized into a plan first, concrete bounded tasks
//           go to daruma directly.
//
// Two consumers: the `daruma-cursor mode` subcommand writes it, the
// daruma-policy.mdc rule (read by the agent) reads it — hence a shared
// module, not duplicated read/write.

import { promises as fs, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export const MODES = ["off", "lite", "full"];
export const DEFAULT_MODE = "lite";

// ponytail: single global file shared by every daruma client (Claude,
// Cursor, ...), not scoped to this plugin — key it on cwd if per-repo
// strictness is ever needed.
export const MODE_FILE = join(homedir(), ".daruma", "mode");

export function readMode() {
  try {
    const v = readFileSync(MODE_FILE, "utf8").trim();
    return MODES.includes(v) ? v : DEFAULT_MODE;
  } catch {
    return DEFAULT_MODE; // no file / unreadable → default
  }
}

export async function writeMode(mode) {
  if (!MODES.includes(mode)) {
    throw new Error(`invalid mode "${mode}" — want one of: ${MODES.join(" | ")}`);
  }
  await fs.mkdir(join(homedir(), ".daruma"), { recursive: true });
  await fs.writeFile(MODE_FILE, mode + "\n");
  return mode;
}
