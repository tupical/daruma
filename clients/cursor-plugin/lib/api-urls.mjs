/** Production cloud origin for hosted Daruma MCP — the "Add to Cursor" default. */
export const CLOUD_API_URL =
  process.env.DARUMA_CLOUD_URL ?? "https://daruma.mcpbox.ru";

/**
 * Generic default target: an explicit `DARUMA_API_URL`/`DARUMA_BASE_URL`
 * (self-host) wins, otherwise the hosted cloud.
 */
export const DEFAULT_API_URL =
  process.env.DARUMA_API_URL ??
  process.env.DARUMA_BASE_URL ??
  CLOUD_API_URL;

/** Self-host / local default origin (bare `daruma-server`). */
export const SELFHOST_URL_DEFAULT =
  process.env.DARUMA_SELFHOST_URL ??
  process.env.DARUMA_API_URL ??
  process.env.DARUMA_BASE_URL ??
  "http://localhost:8080";

/** @typedef {"prod" | "cloud" | "self-host" | "local" | "auto"} ApiPreset */

/**
 * @param {ApiPreset | undefined} preset
 * @returns {string | undefined} fixed URL for cloud/self-host; undefined for auto
 */
export function urlForApiPreset(preset) {
  if (preset === "prod" || preset === "cloud") return CLOUD_API_URL;
  if (preset === "self-host" || preset === "local") return SELFHOST_URL_DEFAULT;
  return undefined;
}
