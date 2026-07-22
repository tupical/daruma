import { test } from "node:test";
import assert from "node:assert/strict";

import { DEFAULT_MODE, MODES, writeMode } from "../lib/mode.mjs";

test("MODES contains DEFAULT_MODE", () => {
  assert.ok(MODES.includes(DEFAULT_MODE));
});

test("writeMode rejects an invalid mode without writing", async () => {
  await assert.rejects(() => writeMode("bogus"), /invalid mode/);
});
