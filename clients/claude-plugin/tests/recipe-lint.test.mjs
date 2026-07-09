import { test } from "node:test";
import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function contextAt(lines, index) {
  const start = Math.max(0, index - 1);
  const end = Math.min(lines.length, index + 9);
  return lines.slice(start, end + 1).join("\n");
}

function lintRecipeText(text, file = "<fixture>") {
  const violations = [];
  const lines = text.split(/\r?\n/);

  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    const context = contextAt(lines, index);
    const where = `${file}:${index + 1}`;

    if (/\b["']?status["']?\s*[:=]\s*["']?all\b/i.test(line)) {
      violations.push(`${where} uses archive-wide status=all`);
    }
    if (/\b(?:daruma_)?workspacegraph(?:_|\b)/i.test(line)) {
      violations.push(`${where} uses workspacegraph`);
    }
    if (/\bdaruma_plan_list\b/i.test(line) && /\b["']?status["']?\s*[:=]\s*(?:["']?completed\b|\[[^\]]*["']completed["'])/i.test(context)) {
      violations.push(`${where} lists completed plans`);
    }
    if (/\bdaruma_(?:list|search)\b/i.test(line) && !/\berror\b/i.test(line)) {
      if (!/\b(?:project_id|project_scope|scope)\b/i.test(context) || !/\blimit\b/i.test(context)) {
        violations.push(`${where} daruma_list/search recipe needs scope and limit`);
      }
    }
  }

  return violations;
}

async function recipeFiles() {
  const commandDir = join(root, "commands");
  const skillDir = join(root, "skills");
  const commandFiles = (await readdir(commandDir))
    .filter((name) => name.endsWith(".md"))
    .map((name) => join(commandDir, name));
  const skillNames = await readdir(skillDir);
  const skillFiles = [];

  for (const name of skillNames) {
    const file = join(skillDir, name, "SKILL.md");
    const body = await readFile(file, "utf8");
    if (/\bdaruma_/.test(body)) skillFiles.push(file);
  }

  return [...commandFiles, ...skillFiles];
}

test("daruma command recipes avoid archive, graph, and unbounded list/search calls", async () => {
  const violations = [];

  for (const file of await recipeFiles()) {
    const body = await readFile(file, "utf8");
    violations.push(...lintRecipeText(body, file));
  }

  assert.deepEqual(violations, []);
});

test("recipe lint catches forbidden fixture examples", () => {
  const bad = `
\`daruma_list status=all\`
\`daruma_workspacegraph_search query=x\`
\`daruma_plan_list status=["completed"]\`
\`daruma_search query=branch\`
`;

  assert.equal(lintRecipeText(bad).length, 5);
});
