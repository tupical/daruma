import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const HOOKS_JSON = join(__dirname, "..", "hooks", "hooks.json");
const PLUGIN_JSON = join(__dirname, "..", ".claude-plugin", "plugin.json");

test("hooks/hooks.json parses as valid JSON", () => {
  const raw = readFileSync(HOOKS_JSON, "utf8");
  assert.doesNotThrow(() => JSON.parse(raw), "hooks.json must be valid JSON");
});

test("hooks/hooks.json has required top-level keys", () => {
  const manifest = JSON.parse(readFileSync(HOOKS_JSON, "utf8"));
  assert.ok(manifest.hooks, "must have 'hooks' key");
  assert.equal(typeof manifest.description, "string", "'description' must be a string");
});

test("hooks/hooks.json registers SessionStart hook", () => {
  const manifest = JSON.parse(readFileSync(HOOKS_JSON, "utf8"));
  assert.ok(Array.isArray(manifest.hooks.SessionStart), "SessionStart must be an array");
  assert.ok(manifest.hooks.SessionStart.length > 0, "SessionStart must have at least one entry");
  const entry = manifest.hooks.SessionStart[0];
  assert.ok(Array.isArray(entry.hooks), "SessionStart[0].hooks must be an array");
  const cmd = entry.hooks[0];
  assert.equal(cmd.type, "command", "hook type must be 'command'");
  assert.match(cmd.command, /session-start\.mjs/, "command must reference session-start.mjs");
  assert.ok(typeof cmd.timeout === "number", "SessionStart hook should have a timeout");
});

test("hooks/hooks.json registers UserPromptSubmit hook", () => {
  const manifest = JSON.parse(readFileSync(HOOKS_JSON, "utf8"));
  assert.ok(Array.isArray(manifest.hooks.UserPromptSubmit), "UserPromptSubmit must be an array");
  const cmd = manifest.hooks.UserPromptSubmit[0].hooks[0];
  assert.equal(cmd.type, "command");
  assert.match(cmd.command, /user-prompt-submit\.mjs/);
});

test("hooks/hooks.json registers Stop hook with asyncRewake", () => {
  const manifest = JSON.parse(readFileSync(HOOKS_JSON, "utf8"));
  assert.ok(Array.isArray(manifest.hooks.Stop), "Stop must be an array");
  const entry = manifest.hooks.Stop[0];
  const cmd = entry.hooks[0];
  assert.equal(cmd.type, "command");
  assert.match(cmd.command, /stop\.mjs/);
  assert.equal(cmd.asyncRewake, true, "Stop hook must have asyncRewake: true");
  assert.ok(typeof cmd.rewakeMessage === "string", "Stop hook must have a rewakeMessage");
});

test("plugin.json relies on the standard auto-loaded hooks file", () => {
  const plugin = JSON.parse(readFileSync(PLUGIN_JSON, "utf8"));
  assert.equal(plugin.hooks, undefined, "explicit hooks path loads hooks/hooks.json twice");
});

test("plugin.json version is semver-like", () => {
  const plugin = JSON.parse(readFileSync(PLUGIN_JSON, "utf8"));
  assert.match(plugin.version, /^\d+\.\d+\.\d+$/, "version must be semver");
});

test("hooks/hooks.json commands reference CLAUDE_PLUGIN_ROOT", () => {
  const manifest = JSON.parse(readFileSync(HOOKS_JSON, "utf8"));
  for (const [event, entries] of Object.entries(manifest.hooks)) {
    for (const entry of entries) {
      for (const hook of entry.hooks ?? []) {
        if (hook.type === "command") {
          assert.match(
            hook.command,
            /CLAUDE_PLUGIN_ROOT/,
            `${event} hook command must use CLAUDE_PLUGIN_ROOT`
          );
        }
      }
    }
  }
});
