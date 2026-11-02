import { test } from "node:test";
import assert from "node:assert/strict";
import { promises as fs } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const REPO_ROOT = join(ROOT, "..", "..");

async function readText(path) {
  return fs.readFile(path, "utf8");
}

async function readJson(path) {
  return JSON.parse(await readText(path));
}

async function listDir(rel) {
  return (await fs.readdir(join(ROOT, rel))).sort();
}

async function shippedCommands() {
  return (await listDir("commands"))
    .filter((f) => f.endsWith(".md"))
    .map((f) => f.slice(0, -".md".length));
}

async function marketplaceEntry() {
  const market = await readJson(join(REPO_ROOT, ".claude-plugin", "marketplace.json"));
  const entry = market.plugins.find((p) => p.name === "daruma-claude");
  assert.ok(entry, "marketplace.json must list daruma-claude");
  return entry;
}

// Words a manifest may only use when the matching command or skill ships.
const ADVERTISED_FEATURES = ["research"];

test("README slash commands exist in commands/", async () => {
  const commands = await shippedCommands();
  for (const readme of ["README.md", "README.ru.md"]) {
    const body = await readText(join(ROOT, readme));
    for (const [, name] of body.matchAll(/\/daruma-claude:([a-z][a-z-]*)/g)) {
      assert.ok(commands.includes(name), `${readme} lists /daruma-claude:${name} but commands/${name}.md does not exist`);
    }
  }
});

test("README bundled-skills table lists only shipped skills", async () => {
  const skills = await listDir("skills");
  const body = await readText(join(ROOT, "README.md"));
  const section = body.split("Bundled skills:")[1]?.split("\n---")[0];
  assert.ok(section, "README.md must keep its Bundled skills table");
  const listed = [...section.matchAll(/^\| `([a-z][a-z-]*)` \|/gm)].map((m) => m[1]);
  assert.ok(listed.length > 0, "Bundled skills table must list at least one skill");
  for (const name of listed) {
    assert.ok(skills.includes(name), `README.md lists skill "${name}" but skills/${name} does not exist`);
  }
});

test("plugin.json command list matches commands/", async () => {
  const commands = await shippedCommands();
  const plugin = await readJson(join(ROOT, ".claude-plugin", "plugin.json"));
  const list = plugin.description.match(/\/daruma-claude:\{([^}]+)\}/);
  assert.ok(list, "plugin.json description must keep its /daruma-claude:{...} list");
  for (const name of list[1].split(",").map((s) => s.trim())) {
    assert.ok(commands.includes(name), `plugin.json advertises ${name} but commands/${name}.md does not exist`);
  }
});

test("manifests only advertise commands and skills the plugin ships", async () => {
  const commands = await shippedCommands();
  const skills = await listDir("skills");
  const plugin = await readJson(join(ROOT, ".claude-plugin", "plugin.json"));
  const pkg = await readJson(join(ROOT, "package.json"));
  const market = await marketplaceEntry();
  const texts = [
    plugin.description,
    ...plugin.keywords,
    pkg.description,
    ...pkg.keywords,
    market.description,
  ].join("\n");
  for (const [, name] of texts.matchAll(/\/daruma-claude:([a-z][a-z-]*)/g)) {
    assert.ok(commands.includes(name), `manifest advertises /daruma-claude:${name} but commands/${name}.md does not exist`);
  }
  for (const name of ADVERTISED_FEATURES) {
    if (commands.includes(name) || skills.includes(name)) continue;
    assert.doesNotMatch(
      texts,
      new RegExp(`\\b${name}\\b`, "i"),
      `manifest advertises "${name}" but neither commands/${name}.md nor skills/${name} exists`,
    );
  }
});

test("README links into .github/ point at files that exist", async () => {
  for (const readme of ["README.md", "README.ru.md"]) {
    const body = await readText(join(ROOT, readme));
    for (const [, rel] of body.matchAll(/\]\(((?:\.\.\/)*\.github\/[^)#\s]+)/g)) {
      await assert.doesNotReject(fs.access(join(ROOT, rel)), `${readme} links to ${rel}, which does not exist`);
    }
    assert.doesNotMatch(body, /publish\.yml/, `${readme} mentions publish.yml, but no publish workflow exists`);
  }
});

test("release scripts only bump the version: no git commit, tag or push", async () => {
  const pkg = await readJson(join(ROOT, "package.json"));
  const release = Object.entries(pkg.scripts).filter(([name]) => name.startsWith("release:"));
  assert.ok(release.length > 0, "package.json must keep its release:* scripts");
  for (const [name, cmd] of release) {
    assert.match(cmd, /--no-git-tag-version/, `${name} must not let npm commit or tag`);
    assert.doesNotMatch(cmd, /\bgit\s+(commit|tag|push)\b/, `${name} must not run git commit/tag/push`);
  }
});

test("plugin.json version matches package.json", async () => {
  const pkg = await readJson(join(ROOT, "package.json"));
  const plugin = await readJson(join(ROOT, ".claude-plugin", "plugin.json"));
  assert.equal(plugin.version, pkg.version);
});

test("version hook copies the bumped package.json version into plugin.json", async () => {
  const pkg = await readJson(join(ROOT, "package.json"));
  assert.equal(pkg.scripts.version, "node scripts/sync-plugin-version.mjs");
  const dir = await fs.mkdtemp(join(tmpdir(), "claude-plugin-version-"));
  try {
    await fs.mkdir(join(dir, "scripts"));
    await fs.mkdir(join(dir, ".claude-plugin"));
    await fs.copyFile(join(ROOT, "scripts", "sync-plugin-version.mjs"), join(dir, "scripts", "sync-plugin-version.mjs"));
    await fs.copyFile(join(ROOT, ".claude-plugin", "plugin.json"), join(dir, ".claude-plugin", "plugin.json"));
    await fs.writeFile(join(dir, "package.json"), JSON.stringify({ ...pkg, version: "9.8.7" }));
    execFileSync(process.execPath, [join(dir, "scripts", "sync-plugin-version.mjs")], { stdio: "pipe" });
    const synced = await readJson(join(dir, ".claude-plugin", "plugin.json"));
    const original = await readJson(join(ROOT, ".claude-plugin", "plugin.json"));
    assert.deepEqual(synced, { ...original, version: "9.8.7" });
  } finally {
    await fs.rm(dir, { recursive: true, force: true });
  }
});
