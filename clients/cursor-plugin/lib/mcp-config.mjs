// Read/write Cursor's mcp.json — both ~/.cursor/mcp.json (global) and
// <project>/.cursor/mcp.json (project-scoped).
//
// Cursor uses the same shape as Claude Desktop / Claude Code:
//   { "mcpServers": { "<name>": { "type": "stdio", "command": "...", ... } } }
//
// We touch only the requested server entry — other servers are preserved.

import { promises as fs } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

export function globalMcpPath() {
  return join(homedir(), ".cursor", "mcp.json");
}

export function projectMcpPath(projectDir) {
  const dir = projectDir ? resolve(projectDir) : process.cwd();
  return join(dir, ".cursor", "mcp.json");
}

export function resolveMcpPath({ scope = "global", projectDir } = {}) {
  if (scope === "global") return globalMcpPath();
  if (scope === "project") return projectMcpPath(projectDir);
  throw new RangeError(`unknown scope: ${scope}`);
}

export async function readMcp(path) {
  try {
    const raw = await fs.readFile(path, "utf8");
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      throw new Error(`mcp.json at ${path} is not a JSON object`);
    }
    if (!parsed.mcpServers || typeof parsed.mcpServers !== "object") {
      parsed.mcpServers = {};
    }
    return parsed;
  } catch (err) {
    if (err.code === "ENOENT") return { mcpServers: {} };
    throw err;
  }
}

async function writeAtomic(path, payload) {
  await fs.mkdir(dirname(path), { recursive: true });
  const tmp = join(dirname(path), `.mcp-cursor.${process.pid}.${Date.now()}.json`);
  await fs.writeFile(tmp, payload);
  try {
    await fs.rename(tmp, path);
  } catch (err) {
    if (err && typeof err === "object" && "code" in err && err.code === "EXDEV") {
      await fs.copyFile(tmp, path);
      await fs.unlink(tmp);
      return;
    }
    throw err;
  }
}

// Adds (or replaces) the named server entry. Returns
//   { path, action: "added"|"replaced"|"unchanged", before, after }.
export async function upsertServer(path, name, entry) {
  if (!name || typeof name !== "string") throw new TypeError("name required");
  if (!entry || typeof entry !== "object") throw new TypeError("entry required");
  const doc = await readMcp(path);
  const before = doc.mcpServers[name] ?? null;
  const same = before && stableJson(before) === stableJson(entry);
  if (same) {
    return { path, action: "unchanged", before, after: before };
  }
  doc.mcpServers[name] = entry;
  await writeAtomic(path, JSON.stringify(doc, null, 2) + "\n");
  return {
    path,
    action: before ? "replaced" : "added",
    before,
    after: entry,
  };
}

export async function removeServer(path, name) {
  const doc = await readMcp(path);
  if (!doc.mcpServers[name]) {
    return { path, action: "missing", removed: null };
  }
  const removed = doc.mcpServers[name];
  delete doc.mcpServers[name];
  await writeAtomic(path, JSON.stringify(doc, null, 2) + "\n");
  return { path, action: "removed", removed };
}

export async function listServers(path) {
  const doc = await readMcp(path);
  return Object.entries(doc.mcpServers).map(([name, entry]) => ({ name, entry }));
}

function stableJson(obj) {
  return JSON.stringify(sortKeys(obj));
}

function sortKeys(value) {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === "object") {
    const out = {};
    for (const k of Object.keys(value).sort()) out[k] = sortKeys(value[k]);
    return out;
  }
  return value;
}
