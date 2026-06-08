import { test } from "node:test";
import assert from "node:assert/strict";
import { join, resolve } from "node:path";

import {
  expandHome,
  resolveCursorAssetRoot,
  resolvePath,
} from "../lib/paths.mjs";

test("expandHome handles quoted macOS-style home paths", () => {
  assert.equal(expandHome("~/Library", "/Users/alice"), join("/Users/alice", "Library"));
  assert.equal(expandHome("~", "/Users/alice"), "/Users/alice");
});

test("resolveCursorAssetRoot defaults global installs to home", () => {
  assert.equal(
    resolveCursorAssetRoot({
      scope: "global",
      cwd: "/repo/project",
      home: "/Users/alice",
    }),
    "/Users/alice",
  );
});

test("resolveCursorAssetRoot resolves relative global rules-dir from home", () => {
  assert.equal(
    resolveCursorAssetRoot({
      scope: "global",
      rulesDir: "alice",
      cwd: "/repo/project",
      home: "/Users/alice",
    }),
    resolve("/Users/alice", "alice"),
  );
});

test("resolveCursorAssetRoot keeps project installs project-scoped", () => {
  assert.equal(
    resolveCursorAssetRoot({
      scope: "project",
      projectDir: "repo",
      cwd: "/Users/alice/work",
      home: "/Users/alice",
    }),
    resolve("/Users/alice/work", "repo"),
  );
});

test("resolvePath expands tilde before resolving", () => {
  assert.equal(
    resolvePath("~/repo", { base: "/tmp/ignored", home: "/Users/alice" }),
    join("/Users/alice", "repo"),
  );
});
