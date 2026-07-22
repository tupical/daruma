// Intake strictness mode: how aggressively raw input gets decomposed into a
// daruma plan (`daruma_plan_materialize`, ADR-0007 plan-only intake) vs.
// tracked directly without going through planning first.
//
//   off   — no decomposition nudging; every request is worked directly.
//   lite  — materialize a plan only on explicit request or for obviously
//           multi-step work. (default)
//   full  — assess every substantive request for "rawness": raw ideas /
//           hypotheses / undetermined direction get materialized into a
//           plan first; concrete bounded tasks are worked directly.
//
// daruma OSS has no pipeline (torii/satori/.../fujin) — that's the mcpbox
// SaaS layer. Here the mode gates `daruma_plan_materialize` only.
//
// Two consumers: the `daruma-codex mode` CLI writes it, the policy block
// (read by the agent at session start) documents how to read and honor it
// — hence a shared module, not duplicated read/write.

import { promises as fs, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export const MODES = ["off", "lite", "full"];
export const DEFAULT_MODE = "lite";

// ponytail: shared across all daruma clients (Claude, Codex, Cursor, …), so
// the file lives outside any single client's config dir.
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
