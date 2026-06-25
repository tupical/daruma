/** Default local Daruma API origin. */
export const DEFAULT_API_URL =
  process.env.DARUMA_API_URL ?? "http://localhost:8080";

/**
 * Alternate API origin for testing hosted or remote environments.
 */
export const ALT_API_URL =
  process.env.DARUMA_STAGING_URL ??
  "http://127.0.0.1:8081";

export const SELFHOST_URL_DEFAULT =
  process.env.DARUMA_SELFHOST_URL ??
  process.env.DARUMA_API_URL ??
  process.env.DARUMA_BASE_URL ??
  "http://localhost:8080";

/** @typedef {"prod" | "staging" | "self-host" | "auto"} ApiPreset */

/**
 * @param {ApiPreset | undefined} preset
 * @returns {string | undefined} fixed URL for prod/staging/self-host; undefined for auto
 */
export function urlForApiPreset(preset) {
  if (preset === "prod") return DEFAULT_API_URL;
  if (preset === "staging") return ALT_API_URL;
  if (preset === "self-host") return SELFHOST_URL_DEFAULT;
  return undefined;
}
