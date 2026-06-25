// Installs Cursor's project-scoped custom slash commands into
// `<project>/.cursor/commands/`. These markdown files appear in the
// Cursor `/` slash-menu as user-invocable commands.
//
// We ship four:
//   - daruma-tasks.md  — read-only task list
//   - daruma-plan.md   — active plan checklist + progress bar
//   - daruma-next.md   — claim next ready task with briefing
//   - daruma-mine.md   — tasks currently claimed by this session

import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { resolvePath } from "./paths.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const SOURCE_DIR = join(__dirname, "..", "cursor", "commands");

export const COMMAND_FILES = [
  "daruma-tasks.md",
  "daruma-plan.md",
  "daruma-next.md",
  "daruma-mine.md",
];

export async function installCommands({ projectDir, overwrite = false } = {}) {
  const dir = projectDir ? resolvePath(projectDir) : process.cwd();
  const targetDir = join(dir, ".cursor", "commands");
  await fs.mkdir(targetDir, { recursive: true });

  const results = [];
  for (const name of COMMAND_FILES) {
    const src = join(SOURCE_DIR, name);
    const dst = join(targetDir, name);
    const exists = await fs.access(dst).then(() => true).catch(() => false);
    if (exists && !overwrite) {
      results.push({ path: dst, name, action: "skipped-exists" });
      continue;
    }
    const content = await fs.readFile(src, "utf8");
    await fs.writeFile(dst, content);
    results.push({ path: dst, name, action: exists ? "overwritten" : "installed" });
  }
  return results;
}
