// Self-update helpers for the `omo` CLI.

import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";

const exec = promisify(execFile);
const WIN_SHELL = process.platform === "win32";

export async function fetchLatestVersion(pkgName = "oh-my-ouroboros") {
  const { stdout } = await exec("npm", ["view", pkgName, "version"], {
    timeout: 15_000,
    shell: WIN_SHELL,
    windowsHide: true,
  });
  return stdout.trim();
}

// PyPI counterpart for ouroboros-ai. No `pip` shell-out — we hit the
// public JSON endpoint so the user doesn't need pip on PATH (they may
// have installed via uv/pipx).
export async function fetchLatestPypiVersion(pkgName = "ouroboros-ai") {
  const url = `https://pypi.org/pypi/${encodeURIComponent(pkgName)}/json`;
  const res = await fetch(url, { signal: AbortSignal.timeout(15_000) });
  if (!res.ok) throw new Error(`pypi.org returned HTTP ${res.status}`);
  const json = await res.json();
  if (!json?.info?.version) throw new Error("pypi.org response missing info.version");
  return json.info.version;
}

export function installLatest(pkgName = "oh-my-ouroboros") {
  return new Promise((resolve, reject) => {
    const child = spawn("npm", ["i", "-g", `${pkgName}@latest`], {
      stdio: "inherit",
      shell: WIN_SHELL,
      windowsHide: true,
    });
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code === 0) resolve(0);
      else reject(new Error(`npm install exited with code ${code}`));
    });
  });
}

// Compare two semver strings without pulling in a dependency. Pre-release tags
// are stripped — sufficient for "is registry newer than local" gating.
export function isNewer(candidate, baseline) {
  const parse = (v) => v.split("-")[0].split(".").map((n) => Number(n) || 0);
  const a = parse(candidate);
  const b = parse(baseline);
  for (let i = 0; i < 3; i++) {
    if ((a[i] ?? 0) > (b[i] ?? 0)) return true;
    if ((a[i] ?? 0) < (b[i] ?? 0)) return false;
  }
  return false;
}
