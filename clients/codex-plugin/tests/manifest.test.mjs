import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

async function readJson(rel) {
  return JSON.parse(await fs.readFile(join(ROOT, rel), "utf8"));
}

async function listDir(rel) {
  return (await fs.readdir(join(ROOT, rel))).sort();
}

async function exists(rel) {
  try {
    await fs.access(join(ROOT, rel));
    return true;
  } catch {
    return false;
  }
}

// Words a manifest may only use when the matching skill actually ships.
const ADVERTISED_SKILLS = ["setup", "research", "doctor"];

test("manifests only advertise skills the plugin ships", async () => {
  const skills = await listDir("skills");
  const plugin = await readJson(".codex-plugin/plugin.json");
  const pkg = await readJson("package.json");
  const texts = [
    plugin.description,
    plugin.interface.shortDescription,
    plugin.interface.longDescription,
    pkg.description,
    ...pkg.keywords,
  ].join("\n");
  for (const name of ADVERTISED_SKILLS) {
    if (skills.includes(name)) continue;
    assert.doesNotMatch(
      texts,
      new RegExp(`\\b${name}\\b`, "i"),
      `manifest advertises "${name}" but skills/${name} does not exist`,
    );
  }
});

test("package bin entries exist", async () => {
  const pkg = await readJson("package.json");
  for (const [name, rel] of Object.entries(pkg.bin)) {
    assert.ok(await exists(rel), `bin ${name} -> ${rel} is missing`);
  }
});

test("skills and commands never hand off to the Claude-only CLI", async () => {
  const files = [];
  for (const skill of await listDir("skills")) {
    files.push(join("skills", skill, "SKILL.md"));
  }
  for (const cmd of await listDir("commands")) {
    files.push(join("commands", cmd));
  }
  for (const rel of files) {
    const body = await fs.readFile(join(ROOT, rel), "utf8");
    assert.doesNotMatch(body, /taskagent-claude|daruma-claude|claude mcp /, rel);
  }
});
