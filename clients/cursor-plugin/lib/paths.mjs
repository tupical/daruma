import { homedir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";

export function expandHome(input, home = homedir()) {
  if (!input) return input;
  if (input === "~") return home;
  if (input.startsWith("~/") || input.startsWith("~\\")) {
    return join(home, input.slice(2));
  }
  return input;
}

export function resolvePath(input, { base = process.cwd(), home = homedir() } = {}) {
  const expanded = expandHome(input, home);
  if (isAbsolute(expanded)) return expanded;
  return resolve(base, expanded);
}

export function resolveCursorAssetRoot({
  scope = "project",
  projectDir,
  rulesDir,
  cwd = process.cwd(),
  home = homedir(),
} = {}) {
  if (rulesDir) {
    const base = scope === "global" ? home : cwd;
    return resolvePath(rulesDir, { base, home });
  }
  if (scope === "global") return home;
  return projectDir ? resolvePath(projectDir, { base: cwd, home }) : cwd;
}
