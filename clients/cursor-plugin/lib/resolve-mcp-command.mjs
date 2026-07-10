// Locate the `daruma` binary for Cursor's mcp.json `command` field (stdio MCP
// runs as `daruma mcp`; the old standalone `daruma-mcp` binary is gone).
//
// Cursor spawns the command by name — ENOENT if the Rust binary is built but
// not on PATH. We probe common locations before falling back to the bare name.

import { access, constants } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const DEFAULT_COMMAND = "daruma";
const __dirname = dirname(fileURLToPath(import.meta.url));

async function isExecutable(path) {
  try {
    await access(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function uniquePaths(paths) {
  const seen = new Set();
  const out = [];
  for (const p of paths) {
    if (!p || seen.has(p)) continue;
    seen.add(p);
    out.push(p);
  }
  return out;
}

function siblingDarumaReleasePaths(startDir) {
  const paths = [];
  let dir = resolve(startDir);
  for (let depth = 0; depth < 6; depth += 1) {
    paths.push(join(dir, "target", "release", DEFAULT_COMMAND));
    paths.push(join(dir, "daruma", "target", "release", DEFAULT_COMMAND));
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return paths;
}

function homeDir(env = process.env) {
  const fromEnv = env.HOME?.trim() || env.USERPROFILE?.trim();
  return fromEnv || homedir();
}

export function candidateMcpCommandPaths({
  cwd = process.cwd(),
  env = process.env,
} = {}) {
  const fromEnv = env.DARUMA_MCP_BIN?.trim();
  const home = homeDir(env);
  const cargoHome = env.CARGO_HOME?.trim() || join(home, ".cargo");

  return uniquePaths([
    fromEnv,
    join(home, ".local", "bin", DEFAULT_COMMAND),
    join(cargoHome, "bin", DEFAULT_COMMAND),
    ...siblingDarumaReleasePaths(cwd),
    ...siblingDarumaReleasePaths(__dirname),
    DEFAULT_COMMAND,
  ]);
}

export async function resolveMcpCommand({
  command,
  cwd = process.cwd(),
  env = process.env,
} = {}) {
  const requested = command?.trim() || DEFAULT_COMMAND;
  if (isAbsolute(requested)) {
    if (await isExecutable(requested)) {
      return { command: requested, resolved: true, source: "explicit-absolute" };
    }
    return { command: requested, resolved: false, source: "explicit-missing" };
  }

  for (const candidate of candidateMcpCommandPaths({ cwd, env })) {
    if (candidate === DEFAULT_COMMAND) continue;
    if (await isExecutable(candidate)) {
      return { command: candidate, resolved: true, source: "discovered" };
    }
  }

  return { command: requested, resolved: false, source: "name-only" };
}
