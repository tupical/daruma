import { access, copyFile, mkdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

import {
  DEFAULT_API_URL,
  SELFHOST_URL_DEFAULT,
  urlForApiPreset,
} from "./api-urls.mjs";

const AGENT_DIR_NAME = "taskagent";
const CREDENTIALS_FILE = "credentials.json";
const MODE_REMOTE = "remote";
const MODE_CLOUD = "cloud";
const MODE_SELF_HOST = "self-host";

function isRemoteMode(mode) {
  return mode === MODE_REMOTE || mode === MODE_CLOUD;
}

function isSelfHostUrl(url) {
  if (!url) return false;
  try {
    const { hostname } = new URL(url);
    return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "::1";
  } catch {
    return false;
  }
}

/** Canonical agent data root (`~/.agents/taskagent` or `%USERPROFILE%\.agents\taskagent`). */
export function agentDirRoot() {
  const override = process.env.TASKAGENT_AGENT_DIR?.trim();
  if (override) return override.replace(/\/$/, "");
  if (process.platform === "win32") {
    const base =
      process.env.USERPROFILE ?? join(homedir(), "AppData", "Local");
    return join(base, ".agents", AGENT_DIR_NAME);
  }
  return join(homedir(), ".agents", AGENT_DIR_NAME);
}

export function credentialsPath() {
  return join(agentDirRoot(), CREDENTIALS_FILE);
}

/** Retired XDG path (`~/.config/taskagent/credentials.json`). */
export function legacyCredentialsPath() {
  if (process.platform === "win32") {
    const base = process.env.APPDATA ?? join(homedir(), "AppData", "Roaming");
    return join(base, AGENT_DIR_NAME, CREDENTIALS_FILE);
  }
  const xdg = process.env.XDG_CONFIG_HOME?.trim();
  const configRoot = xdg ? xdg.replace(/\/$/, "") : join(homedir(), ".config");
  return join(configRoot, AGENT_DIR_NAME, CREDENTIALS_FILE);
}

async function fileExists(path) {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * On first use: copy legacy `~/.config/taskagent/credentials.json` into agent dir.
 * @returns {Promise<boolean>} true if migration ran
 */
export async function migrateLegacyCredentialsIfNeeded() {
  const target = credentialsPath();
  if (await fileExists(target)) return false;

  const legacy = legacyCredentialsPath();
  if (!(await fileExists(legacy))) return false;

  await mkdir(join(target, ".."), { recursive: true });
  await copyFile(legacy, target);
  return true;
}

export async function loadCredentials() {
  await migrateLegacyCredentialsIfNeeded();
  try {
    const raw = await readFile(credentialsPath(), "utf8");
    return JSON.parse(raw);
  } catch (err) {
    if (err && typeof err === "object" && "code" in err && err.code === "ENOENT") {
      return null;
    }
    throw err;
  }
}

/**
 * @param {object} creds
 * @param {{ profile?: string, mode?: string }} [opts]
 */
export function resolveActiveProfile(creds, opts = {}) {
  if (!creds?.profiles || typeof creds.profiles !== "object") {
    return null;
  }
  const preferred =
    opts.profile ??
    creds.active_profile ??
    Object.keys(creds.profiles)[0];
  let profile = creds.profiles[preferred];
  if (!profile?.token) {
    const mode = opts.mode;
    const fallback = Object.entries(creds.profiles).find(
      ([, p]) => p?.token && (!mode || p.mode === mode),
    );
    if (fallback) {
      return { name: fallback[0], ...fallback[1] };
    }
    return null;
  }
  return { name: preferred, ...profile };
}

export function profileServerUrl(profile) {
  if (profile?.server_url) return String(profile.server_url).replace(/\/$/, "");
  if (profile?.mode === MODE_SELF_HOST) return SELFHOST_URL_DEFAULT;
  if (isRemoteMode(profile?.mode)) return DEFAULT_API_URL;
  return SELFHOST_URL_DEFAULT;
}

/**
 * Pick the credentials profile that matches an install target.
 * @param {object | null} creds
 * @param {{ apiUrl?: string, token?: string, remote?: import("./api-urls.mjs").ApiPreset, mode?: string, profile?: string }} [overrides]
 */
export function resolveProfileForInstall(creds, overrides = {}) {
  if (!creds?.profiles || typeof creds.profiles !== "object") {
    return null;
  }

  const entries = Object.entries(creds.profiles).filter(([, p]) => p?.token);
  if (entries.length === 0) return null;

  const explicit = overrides.profile
    ? entries.find(([name]) => name === overrides.profile)
    : null;
  if (explicit) return { name: explicit[0], ...explicit[1] };

  const targetUrl =
    overrides.apiUrl?.replace(/\/$/, "") ??
    urlForApiPreset(overrides.remote);

  if (targetUrl) {
    const exact = entries.find(
      ([, p]) => profileServerUrl(p).replace(/\/$/, "") === targetUrl,
    );
    if (exact) return { name: exact[0], ...exact[1] };

    const preferredMode = isSelfHostUrl(targetUrl) ? MODE_SELF_HOST : MODE_CLOUD;
    const byMode = entries.find(([, p]) => p.mode === preferredMode);
    if (byMode) return { name: byMode[0], ...byMode[1] };
  }

  if (overrides.mode) {
    const byMode = entries.find(([, p]) => p.mode === overrides.mode);
    if (byMode) return { name: byMode[0], ...byMode[1] };
  }

  return resolveActiveProfile(creds, overrides);
}

/**
 * Build MCP stdio `env` for taskagent-mcp from stored credentials (after remote pair).
 * @param {{ apiUrl?: string, token?: string, workspaceId?: string, remote?: import("./api-urls.mjs").ApiPreset }} [overrides]
 */
export async function resolveMcpEnvFromCredentials(overrides = {}) {
  const presetUrl = urlForApiPreset(overrides.remote);
  const env = {};

  const explicitUrl =
    overrides.apiUrl?.replace(/\/$/, "") ??
    presetUrl;

  if (explicitUrl) {
    env.TASKAGENT_API_URL = explicitUrl;
  }

  if (overrides.token) {
    env.TASKAGENT_TOKEN = overrides.token;
  }
  if (overrides.workspaceId) {
    env.TASKAGENT_WORKSPACE_ID = overrides.workspaceId;
  }

  const creds = await loadCredentials();
  const profile = creds ? resolveProfileForInstall(creds, overrides) : null;
  if (!profile?.token) {
    if (!env.TASKAGENT_API_URL) {
      env.TASKAGENT_API_URL = SELFHOST_URL_DEFAULT;
    }
    return env;
  }

  if (!env.TASKAGENT_API_URL) {
    env.TASKAGENT_API_URL = profileServerUrl(profile);
  }
  if (!env.TASKAGENT_TOKEN) {
    env.TASKAGENT_TOKEN = profile.token;
  }
  if (
    !env.TASKAGENT_WORKSPACE_ID &&
    isRemoteMode(profile.mode) &&
    profile.workspace_id
  ) {
    env.TASKAGENT_WORKSPACE_ID = profile.workspace_id;
  }

  return env;
}

/** HTTP probe URL: credentials profile, then env, then self-host default. */
export async function resolveHttpProbeUrl(overrides = {}) {
  const presetUrl = urlForApiPreset(overrides.remote);
  if (overrides.apiUrl) return overrides.apiUrl.replace(/\/$/, "");
  if (presetUrl) return presetUrl;

  const fromEnv =
    process.env.TASKAGENT_API_URL ??
    process.env.TASKAGENT_BASE_URL;
  if (fromEnv?.trim()) return fromEnv.trim().replace(/\/$/, "");

  const creds = await loadCredentials();
  const profile = creds ? resolveActiveProfile(creds) : null;
  if (profile?.token) return profileServerUrl(profile);

  return SELFHOST_URL_DEFAULT;
}

export function credentialsLocationHint() {
  return credentialsPath();
}
