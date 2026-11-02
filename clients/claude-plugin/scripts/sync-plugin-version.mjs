#!/usr/bin/env node
// npm `version` lifecycle hook: copy the bumped package.json version into
// .claude-plugin/plugin.json so the npm package and the Claude Code plugin
// manifest never drift apart.
import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const pkg = JSON.parse(await fs.readFile(join(ROOT, "package.json"), "utf8"));
const manifestPath = join(ROOT, ".claude-plugin", "plugin.json");
const manifest = await fs.readFile(manifestPath, "utf8");

const VERSION_FIELD = /("version"\s*:\s*")[^"]*(")/;
if (!VERSION_FIELD.test(manifest)) {
  console.error(`sync-plugin-version: no "version" field in ${manifestPath}`);
  process.exit(1);
}
await fs.writeFile(manifestPath, manifest.replace(VERSION_FIELD, `$1${pkg.version}$2`));
console.log(`plugin.json version -> ${pkg.version}`);
