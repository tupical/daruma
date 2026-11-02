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
    if (/\bstatus\s*[:=]\s*\[/i.test(line)) {
      violations.push(`${where} passes status as an array; daruma_list takes one comma-separated string`);
    }
    if (/\bowner\s*[:=]\s*["']?self\b/i.test(line)) {
      violations.push(`${where} passes owner=self; daruma_list has no owner filter`);
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

  assert.equal(lintRecipeText(bad).length, 6);

  const badArgs = lintRecipeText('`daruma_list status=["in_progress"] owner="self" project_id=p limit=5`');
  assert.equal(badArgs.length, 2);
  assert.match(badArgs[0], /status as an array/);
  assert.match(badArgs[1], /owner=self/);
});

const MINE_RECIPES = [
  join(root, "commands", "mine.md"),
  join(root, "..", "codex-plugin", "commands", "mine.md"),
  join(root, "..", "cursor-plugin", "cursor", "commands", "daruma-mine.md"),
];

test("mine recipes filter by the caller's live claim, not an owner list filter", async () => {
  for (const file of MINE_RECIPES) {
    const body = await readFile(file, "utf8");
    const argViolations = lintRecipeText(body, file).filter((v) => /array|owner=self/.test(v));
    assert.deepEqual(argViolations, [], file);
    assert.match(body, /status = "in_progress"/, file);
    assert.match(body, /daruma_get id=<task id>/, file);
    assert.match(body, /current_claim\.is_mine == true/, file);
    assert.match(body, /current_claim\.agent_id == <mcp_agent_id>/, file);
    assert.match(body, /capture `mcp_agent_id`/, file);
  }
});

const ID_RECIPES = [join(root, "commands", "close.md"), join(root, "commands", "status.md")];

test("close/status recipes pass id to id-keyed tools and resolve short ids from a list, not search", async () => {
  for (const file of ID_RECIPES) {
    const body = await readFile(file, "utf8");
    assert.doesNotMatch(body, /\bdaruma_(?:get|complete|set_status)\s+task_id\b/, file);
    assert.match(body, /`daruma_get id=/, file);
    assert.doesNotMatch(body, /\bdaruma_search\s+query=/, file);
    assert.match(body, /`daruma_list status="[a-z_,]+" project_id=<id> limit=\d+`/, file);
    assert.match(body, /ends with/, file);
    assert.match(body, /Several matches → list them/, file);
    assert.match(body, /`has_more: true`/, file);
    assert.match(body, /`cursor=<next_cursor>`/, file);
    assert.match(body, /Only after the last page/, file);
    assert.match(body, /If a page fails to load, decide nothing/, file);
  }
  const close = await readFile(ID_RECIPES[0], "utf8");
  assert.match(close, /`daruma_complete id=<id>`/);
  assert.match(close, /`daruma_set_status id=<id> status=cancelled`/);
});
