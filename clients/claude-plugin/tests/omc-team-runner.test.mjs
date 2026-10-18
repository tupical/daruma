import { test } from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { runOmcTeam } from "../lib/omc-team-runner.mjs";

// Stub `omc` on a private PATH: logs every invocation, starts a team by
// creating its state dir, reports it as still running, records shutdowns.
async function withFakeOmc({ startExit = 0, startDelay = 0 } = {}, fn) {
  const dir = await mkdtemp(join(tmpdir(), "daruma-omc-runner-test-"));
  const bin = join(dir, "bin");
  const cwd = join(dir, "repo");
  const log = join(dir, "omc.log");
  await mkdir(bin);
  await mkdir(cwd);
  await writeFile(join(bin, "omc"), `#!/bin/sh
echo "$*" >> '${log}'
if [ "$1 $2" = "team api" ]; then
  case "$3" in
    get-summary) echo '{"ok":true,"data":{"summary":{"tasks":{"total":1,"in_progress":1}}}}' ;;
    *) echo '{"ok":true,"data":{"tasks":[]}}' ;;
  esac
  exit 0
fi
if [ "$1 $2" = "team shutdown" ]; then exit 0; fi
mkdir -p .omc/state/team/fake-team
sleep ${startDelay}
exit ${startExit}
`);
  await chmod(join(bin, "omc"), 0o755);
  const savedPath = process.env.PATH;
  process.env.PATH = `${bin}:${savedPath}`;
  const calls = async () => (await readFile(log, "utf8").catch(() => "")).split("\n").filter(Boolean);
  try {
    return await fn({ cwd, calls });
  } finally {
    process.env.PATH = savedPath;
    await rm(dir, { recursive: true, force: true });
  }
}

test("already-aborted signal starts no omc team", { skip: process.platform === "win32" }, async () => {
  await withFakeOmc({}, async ({ cwd, calls }) => {
    await assert.rejects(
      runOmcTeam({ prompt: "noop", cwd, abortSignal: AbortSignal.abort() }),
      /omc team start aborted/,
    );
    assert.deepEqual(await calls(), []);
  });
});

test("abort while polling shuts the running team down promptly", { skip: process.platform === "win32" }, async () => {
  await withFakeOmc({}, async ({ cwd, calls }) => {
    const controller = new AbortController();
    const started = Date.now();
    await assert.rejects(
      runOmcTeam({
        prompt: "long job",
        cwd,
        pollIntervalMs: 60_000,
        abortSignal: controller.signal,
        onProgress: (e) => { if (e.kind === "counts") controller.abort(); },
      }),
      /omc team poll aborted/,
    );
    // The 60 s poll pause must not delay the shutdown.
    assert(Date.now() - started < 20_000);
    const log = await calls();
    assert.equal(log.at(-1), "team shutdown fake-team");
    assert(!log.some((line) => line.startsWith("team api list-tasks")));
  });
});

test("abort during a failing start shuts down the team it left behind", { skip: process.platform === "win32" }, async () => {
  await withFakeOmc({ startExit: 130, startDelay: 0.3 }, async ({ cwd, calls }) => {
    const controller = new AbortController();
    setTimeout(() => controller.abort(), 50);
    await assert.rejects(
      runOmcTeam({ prompt: "interrupted start", cwd, abortSignal: controller.signal }),
      /omc team exited with code 130/,
    );
    assert.equal((await calls()).at(-1), "team shutdown fake-team");
  });
});

test("failing start without abort does not shut anything down", { skip: process.platform === "win32" }, async () => {
  await withFakeOmc({ startExit: 1 }, async ({ cwd, calls }) => {
    await assert.rejects(runOmcTeam({ prompt: "broken", cwd }), /omc team exited with code 1/);
    assert(!(await calls()).some((line) => line.startsWith("team shutdown")));
  });
});
