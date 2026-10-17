import assert from "node:assert/strict";
import test from "node:test";
import { DEFAULT_API_URL, resolveConnection } from "../dist/connection.js";

const unset = { defaultValue: DEFAULT_API_URL };
const env = { DARUMA_API_URL: "https://env.example", DARUMA_TOKEN: "env_token" };

test("env vars win over the manifest default when settings are unset", () => {
  const resolved = resolveConnection(unset, { defaultValue: "" }, env);

  assert.deepEqual(resolved, { apiUrl: "https://env.example", token: "env_token" });
});

test("an explicit setting wins over env vars", () => {
  const resolved = resolveConnection(
    { ...unset, globalValue: "https://user.example" },
    { defaultValue: "", workspaceValue: "ws_token" },
    env
  );

  assert.deepEqual(resolved, { apiUrl: "https://user.example", token: "ws_token" });
});

test("workspace folder beats workspace beats global", () => {
  const resolved = resolveConnection(
    { ...unset, globalValue: "https://g", workspaceValue: "https://w", workspaceFolderValue: "https://f" },
    undefined,
    {}
  );

  assert.equal(resolved.apiUrl, "https://f");
});

test("falls back to localhost and an empty token without settings or env", () => {
  const resolved = resolveConnection(unset, undefined, {});

  assert.deepEqual(resolved, { apiUrl: DEFAULT_API_URL, token: "" });
});

test("an explicitly empty setting does not shadow env vars", () => {
  const resolved = resolveConnection({ ...unset, globalValue: "" }, { defaultValue: "", globalValue: "" }, env);

  assert.deepEqual(resolved, { apiUrl: "https://env.example", token: "env_token" });
});
