// Cursor "Add to Cursor" deeplink generator.
//
// Official format (https://docs.cursor.com/en/deeplinks):
//   cursor://anysphere.cursor-deeplink/mcp/install?name=NAME&config=BASE64_JSON
//
// `config` is a base64-encoded JSON object matching a single entry of
// `mcpServers` in mcp.json — i.e. { command, args?, env?, type? } for stdio
// servers or { type: "http", url, headers? } for remote servers. The wrapping
// { "mcpServers": { ... } } object is NOT included.
//
// Web buttons should use this cursor:// URL directly. The old
// https://cursor.com/install-mcp wrapper is not a documented route and can 404.

import { resolveMcpEnvFromCredentials } from "./agent-credentials.mjs";
import {
  DEFAULT_API_URL,
  ALT_API_URL,
  SELFHOST_URL_DEFAULT,
  urlForApiPreset,
} from "./api-urls.mjs";
import { resolveMcpCommand } from "./resolve-mcp-command.mjs";

const SCHEME_DEEPLINK = "cursor://anysphere.cursor-deeplink/mcp/install";
const MCP_RESOURCE_PATH = "/v1/mcp";

export function encodeConfig(config) {
  if (!config || typeof config !== "object") {
    throw new TypeError("encodeConfig: config must be a non-null object");
  }
  const json = JSON.stringify(config);
  return Buffer.from(json, "utf8").toString("base64");
}

export function decodeConfig(b64) {
  if (typeof b64 !== "string" || b64.length === 0) {
    throw new TypeError("decodeConfig: b64 must be a non-empty string");
  }
  const json = Buffer.from(b64, "base64").toString("utf8");
  return JSON.parse(json);
}

function assertName(name) {
  if (typeof name !== "string" || name.length === 0) {
    throw new TypeError("name must be a non-empty string");
  }
  if (!/^[a-zA-Z0-9._-]+$/.test(name)) {
    throw new RangeError(`name must match [a-zA-Z0-9._-]+, got: ${name}`);
  }
}

export function buildCursorDeeplink(name, config) {
  assertName(name);
  const b64 = encodeConfig(config);
  const qs = new URLSearchParams({ name, config: b64 }).toString();
  return `${SCHEME_DEEPLINK}?${qs}`;
}

export function buildHttpsInstallUrl(name, config) {
  return buildCursorDeeplink(name, config);
}

export function mcpResourceUrl(serverUrl) {
  const base = (serverUrl || SELFHOST_URL_DEFAULT).replace(/\/$/, "");
  return `${base}${MCP_RESOURCE_PATH}`;
}

export function defaultTaskagentHttpConfig({ apiUrl, remote, token = null, workspaceId = null } = {}) {
  const serverUrl = apiUrl ?? urlForApiPreset(remote) ?? DEFAULT_API_URL;
  const entry = {
    type: "http",
    url: mcpResourceUrl(serverUrl),
  };
  const headers = {};
  if (token) headers.Authorization = `Bearer ${token}`;
  if (workspaceId) headers["X-TaskAgent-Workspace-Id"] = workspaceId;
  if (Object.keys(headers).length > 0) entry.headers = headers;
  return entry;
}

/**
 * Canonical mcpServers entry for the taskagent stdio shim.
 * Env uses `TASKAGENT_API_URL` (read by taskagent-mcp). After remote pair,
 * `resolveMcpEnvFromCredentials` fills URL, token, and workspace id.
 */
export async function defaultTaskagentStdioConfig({
  command = "taskagent-mcp",
  args = [],
  apiUrl,
  token = null,
  workspaceId = null,
  remote,
  env: extraEnv,
} = {}) {
  const credEnv = await resolveMcpEnvFromCredentials({
    apiUrl,
    token: token ?? undefined,
    workspaceId: workspaceId ?? undefined,
    remote,
  });
  const env = { ...credEnv, ...extraEnv };
  if (!env.TASKAGENT_API_URL) {
    env.TASKAGENT_API_URL =
      urlForApiPreset(remote) ?? SELFHOST_URL_DEFAULT;
  }
  const resolved = await resolveMcpCommand({ command });
  const entry = { type: "stdio", command: resolved.command };
  if (args.length > 0) entry.args = args;
  entry.env = env;
  return entry;
}

export async function defaultTaskagentConfig(opts = {}) {
  const transport = opts.transport ?? (opts.command ? "stdio" : "http");
  if (transport === "http" || transport === "remote" || transport === "remote-oauth") {
    return defaultTaskagentHttpConfig(opts);
  }
  if (transport !== "stdio") {
    throw new RangeError(`unknown Cursor MCP transport: ${transport}`);
  }
  return defaultTaskagentStdioConfig(opts);
}

/** Sync variant when credentials are not needed (tests / explicit env only). */
export function defaultTaskagentConfigSync({
  command,
  args = [],
  apiUrl = SELFHOST_URL_DEFAULT,
  token = null,
  workspaceId = null,
  transport,
} = {}) {
  const resolvedTransport = transport ?? (command ? "stdio" : "http");
  if (resolvedTransport === "http" || resolvedTransport === "remote" || resolvedTransport === "remote-oauth") {
    return defaultTaskagentHttpConfig({ apiUrl, token, workspaceId });
  }
  const resolvedCommand = command ?? "taskagent-mcp";
  const env = { TASKAGENT_API_URL: apiUrl };
  if (token) env.TASKAGENT_TOKEN = token;
  if (workspaceId) env.TASKAGENT_WORKSPACE_ID = workspaceId;
  const entry = { type: "stdio", command: resolvedCommand };
  if (args.length > 0) entry.args = args;
  entry.env = env;
  return entry;
}

// Convenience: returns both URLs + the underlying entry, ready to print or
// embed in a marketplace manifest.
export async function buildTaskagentInstallLinks(opts = {}) {
  const name = opts.name ?? "taskagent";
  const config = await defaultTaskagentConfig(opts);
  return {
    name,
    config,
    deeplink: buildCursorDeeplink(name, config),
    httpsUrl: buildCursorDeeplink(name, config),
    apiUrls: {
      prod: DEFAULT_API_URL,
      staging: ALT_API_URL,
      selfHost: SELFHOST_URL_DEFAULT,
    },
  };
}

export { DEFAULT_API_URL, ALT_API_URL, SELFHOST_URL_DEFAULT };
