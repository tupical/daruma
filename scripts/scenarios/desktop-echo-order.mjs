#!/usr/bin/env node
// Real-binary scenario for the order of events on a device: daruma-desktop
// marks a task done while someone else's change of it is still on the way
// down, against a live daruma-server.
//
//   GIT_SHA=$(git rev-parse HEAD) cargo build -p daruma-server -p daruma-desktop
//   node scripts/scenarios/desktop-echo-order.mjs \
//     --server "$CARGO_TARGET_DIR/debug/daruma-server" \
//     --desktop "$CARGO_TARGET_DIR/debug/daruma-desktop" \
//     --old-desktop /path/to/daruma-desktop-before-the-fix --out /tmp/trace.jsonl
//
// Every round compares the device with the server and with a reference device
// that only ever saw the server's order. Writes one JSON object per line and
// exits 1 if any check failed. Passing the old binary as --desktop is the
// negative control.
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { setTimeout as sleep } from 'node:timers/promises';
import { parseArgs } from 'node:util';

const { values: args } = parseArgs({
  options: {
    server: { type: 'string' },
    desktop: { type: 'string' },
    'old-desktop': { type: 'string' },
    out: { type: 'string' },
    port: { type: 'string', default: '18955' },
    'expect-sha': { type: 'string' },
  },
});
if (!args.server || !args.desktop || !args['old-desktop'] || !args.out) {
  console.error(
    'usage: desktop-echo-order.mjs --server <path> --desktop <path> --old-desktop <path> --out <trace.jsonl>',
  );
  process.exit(2);
}

const dir = mkdtempSync(join(tmpdir(), 'daruma-echo-order-'));
const base = `http://127.0.0.1:${args.port}/v1`;
const trace = [];
const secrets = new Map();
let failed = 0;
let adminToken = '';
let projectId = '';

function record(entry) {
  trace.push({ n: trace.length + 1, at: new Date().toISOString(), ...entry });
}

function check(name, ok, detail = {}) {
  if (!ok) failed += 1;
  record({ check: name, result: ok ? 'PASS' : 'FAIL', ...detail });
}

async function http(step, method, path, body) {
  const started = performance.now();
  const response = await fetch(`${base}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${adminToken}`,
      ...(body ? { 'content-type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  const json = text ? JSON.parse(text) : null;
  record({
    step,
    http: `${method} ${path}`,
    status: response.status,
    req_id: response.headers.get('x-request-id'),
    code: json?.error?.code ?? json?.code ?? undefined,
    ms: Math.round(performance.now() - started),
  });
  return { status: response.status, json };
}

const proxyFree = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => !/^(https?|no|all)_proxy$/i.test(key)),
);

class Device {
  constructor(name) {
    this.name = name;
    this.dir = join(dir, name);
    mkdirSync(this.dir);
    this.db = join(this.dir, 'replica.sqlite');
  }

  env() {
    return {
      ...proxyFree,
      DARUMA_DATA_DIR: this.dir,
      DARUMA_API_URL: `http://127.0.0.1:${args.port}`,
      DARUMA_TOKEN: adminToken,
    };
  }

  run(binary, step, ...argv) {
    const child = spawnSync(binary, argv, { env: this.env(), encoding: 'utf8' });
    record({
      step,
      device: this.name,
      process: `daruma-desktop ${argv.join(' ')}`,
      binary: binary === args.desktop ? 'under_test' : 'old',
      exit: child.status,
      stdout: child.stdout.trim().slice(0, 300),
      stderr: child.stderr.trim().slice(-200),
    });
    return child;
  }

  sync(step, binary = args.desktop, limit = 1000) {
    return this.run(binary, step, 'sync', '--limit', String(limit));
  }

  /** Sync until the cursor reaches the tip and a sync finds nothing new. */
  settle(step, tip, binary = args.desktop, limit = 1000) {
    for (let round = 0; round < 400; round += 1) {
      if (this.sync(step, binary, limit).status !== 0) return false;
      if (this.cursor() >= tip) return true;
    }
    return false;
  }

  query(sql, ...params) {
    const db = new DatabaseSync(this.db, { readOnly: true });
    try {
      return db.prepare(sql).all(...params);
    } finally {
      db.close();
    }
  }

  cursor() {
    return existsSync(this.db) ? this.query('SELECT server_seq FROM desktop_replica_state')[0].server_seq : 0;
  }

  status(id) {
    return this.query('SELECT status FROM tasks WHERE id = ?', id)[0]?.status;
  }

  state() {
    return {
      tasks: this.query('SELECT id, title, description, status FROM tasks ORDER BY id'),
      comments: this.query('SELECT id, task_id, body FROM comments ORDER BY id'),
      projects: this.query('SELECT id, title FROM projects ORDER BY id'),
      history: this.query(
        'SELECT entity_id, version_number, event_type FROM entity_versions ORDER BY entity_id, version_number',
      ),
    };
  }

  feed() {
    return this.query('SELECT event_id FROM activity WHERE task_id IS NOT NULL ORDER BY event_id');
  }

  duplicates() {
    return this.query('SELECT event_id FROM events GROUP BY event_id HAVING COUNT(*) > 1').length;
  }

  pending() {
    return this.query('SELECT COUNT(*) AS n FROM desktop_outbox WHERE flushed_at IS NULL')[0].n;
  }
}

const EXPECTED = [
  'a device that marks a task done while another client\'s in_progress is on the way down ends done, as the server does, after one sync',
  'the same holds when the change and the echo arrive in separate pages, at limits 1, 2 and 3',
  'a change made after the flush and before the pull stays on the device and reaches the server once',
  'another client\'s work on a different task leaves the device with the delta alone, no rebuild of the log',
  'a replica the old binary left on the wrong order, its cursor already past the echo, is repaired by the first sync of the new binary and keeps what it has not delivered yet',
  'in every case a fresh reference device gets the same rows, a further sync changes nothing, and no event is logged twice',
];

async function seedTask(step, title) {
  const planned = await http(step, 'POST', '/commands', {
    command: {
      type: 'materialize_plan',
      plan: {
        title: `plan of ${title}`,
        project_id: projectId,
        owner: { kind: 'user' },
        source_brief: 'scenario',
        source_ref: `https://example.test/echo-order/${encodeURIComponent(title)}`,
      },
      tasks: [{ title }],
    },
  });
  if (planned.status !== 200) throw new Error(`seed ${title}: ${planned.status} ${JSON.stringify(planned.json)}`);
  const event = planned.json.data.find((e) => e.payload.type === 'task_created');
  return `tsk_${event.payload.task.id}`.replace(/^tsk_tsk_/, 'tsk_');
}

async function setStatus(step, id, status) {
  const answer = await http(step, 'POST', '/commands', {
    command: { type: 'set_status', id: id.replace(/^tsk_/, ''), status },
  });
  if (answer.status !== 200) throw new Error(`set_status: ${answer.status}`);
}

async function tip(step) {
  let since = 0;
  for (;;) {
    const { status, json } = await http(step, 'GET', `/events?since=${since}&limit=1000`);
    if (status !== 200) throw new Error(`events: ${status}`);
    if (json.length === 0) return since;
    since = json.at(-1).seq;
  }
}

const serverStatus = async (step, id) => (await http(step, 'GET', `/tasks/${id}`)).json?.status;

/** Same rows as a device that saw the server's order, and stable from here. */
async function assertSettled(step, device, id) {
  const reference = new Device(`reference-${step}`);
  const head = await tip(step);
  check(`${step}: reference device syncs`, reference.settle(step, head));
  const ours = device.state();
  const theirs = reference.state();
  const same = {};
  for (const key of Object.keys(ours)) same[key] = JSON.stringify(ours[key]) === JSON.stringify(theirs[key]);
  check(`${step}: device and server agree on the task`, device.status(id) === (await serverStatus(step, id)), {
    device: device.status(id),
    server: await serverStatus(step, id),
  });
  check(`${step}: device matches the reference`, Object.values(same).every(Boolean), {
    same,
    tasks: [ours.tasks.length, theirs.tasks.length],
    history: [ours.history.length, theirs.history.length],
  });
  const cursor = device.cursor();
  const again = device.sync(`${step}_again`);
  check(`${step}: another sync changes nothing`, again.status === 0
    && device.cursor() === cursor
    && JSON.stringify(device.state()) === JSON.stringify(ours), { cursor });
  check(`${step}: every event logged once`, device.duplicates() === 0);
  const feed = device.feed();
  check(`${step}: feed holds each event once`, new Set(feed.map((row) => row.event_id)).size === feed.length, {
    rows: feed.length,
  });
}

/** A device on the tip, a task it knows, and a change of it still on the way. */
async function overtakenDone(step, binary = args.desktop) {
  const device = new Device(step);
  const id = await seedTask(step, `task of ${step}`);
  if (!device.settle(step, await tip(step), binary)) throw new Error(`${step}: first sync failed`);
  await setStatus(step, id, 'in_progress');
  const done = device.run(binary, step, 'done', id);
  check(`${step}: done works offline`, done.status === 0 && device.status(id) === 'done', {
    status: device.status(id),
  });
  return { device, id };
}

async function scenario() {
  const project = await http('seed', 'POST', '/commands', {
    command: { type: 'create_project', title: 'echo order' },
  });
  if (project.status !== 200) throw new Error(`seed: ${project.status}`);
  projectId = project.json.data[0].payload.project.id;

  // One sync, one page.
  {
    const { device, id } = await overtakenDone('one_page');
    check('one_page: sync exits 0', device.sync('one_page').status === 0);
    check('one_page: the device stays done', device.status(id) === 'done', { status: device.status(id) });
    await assertSettled('one_page', device, id);
  }

  // The change and the echo split across pages.
  for (const limit of [1, 2, 3]) {
    const step = `pages_${limit}`;
    const { device, id } = await overtakenDone(step);
    check(`${step}: flushes and settles`, device.settle(step, device.cursor() + 4, args.desktop, limit)
      && device.settle(step, await tip(step), args.desktop, limit));
    check(`${step}: the device ends done`, device.status(id) === 'done', { status: device.status(id) });
    await assertSettled(step, device, id);
  }

  // Someone else's task: the delta alone.
  {
    const step = 'other_task';
    const device = new Device(step);
    const other = await seedTask(step, 'someone else\'s task');
    const mine = await seedTask(step, 'my task');
    if (!device.settle(step, await tip(step))) throw new Error(`${step}: first sync failed`);
    await setStatus(step, other, 'in_progress');
    device.run(args.desktop, step, 'done', mine);
    const synced = device.sync(step).stdout;
    check(`${step}: sync read the delta alone`, /applied 1\/4 remote/.test(synced), { synced: synced.slice(0, 160) });
    await assertSettled(step, device, mine);
  }

  // The old binary settles on the wrong order; the new one finds that later.
  {
    const step = 'old_replica';
    const { device, id } = await overtakenDone(step, args['old-desktop']);
    check(`${step}: old sync exits 0`, device.sync(step, args['old-desktop']).status === 0);
    const stale = device.status(id);
    const server = await serverStatus(step, id);
    check(`${step}: the old binary diverged`, stale !== server, { device: stale, server });
    const held = device.run(args['old-desktop'], step, 'add', 'written before the upgrade');
    check(`${step}: a local task waits`, held.status === 0 && device.pending() > 0, { pending: device.pending() });
    // A replica from before bindings waits for this word; desktop-server-binding.mjs covers why.
    check(`${step}: the replica is adopted by its server`, device.run(args.desktop, step, 'replica', 'adopt').status === 0);
    check(`${step}: new sync exits 0`, device.sync(step).status === 0);
    check(`${step}: the replica is repaired behind its cursor`, device.status(id) === 'done', {
      status: device.status(id),
    });
    check(`${step}: nothing left to deliver`, device.pending() === 0, { pending: device.pending() });
    await assertSettled(step, device, id);
  }
}

const server = spawn(args.server, [], {
  env: {
    ...proxyFree,
    DARUMA_AGENT_DIR: join(dir, 'server'),
    DARUMA_PORT: args.port,
    DARUMA_TLS_PORT: String(Number(args.port) + 1),
    DARUMA_MDNS_DISABLE: '1',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});
let log = '';
server.stdout.on('data', (chunk) => { log += chunk; });
server.stderr.on('data', (chunk) => { log += chunk; });
const exited = new Promise((resolve) => server.on('close', (code, signal) => resolve({ code, signal })));

async function waitHealthy() {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (server.exitCode !== null) throw new Error(`server exited: ${server.exitCode}`);
    try {
      const response = await fetch(`${base}/healthz`);
      if (response.ok) return response.json();
    } catch {
      await sleep(100);
    }
  }
  throw new Error('server never answered /healthz');
}

const digest = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');
let thrown = null;
try {
  const health = await waitHealthy();
  record({
    meta: 'build',
    server_sha256: digest(args.server),
    desktop_sha256: digest(args.desktop),
    old_desktop_sha256: digest(args['old-desktop']),
    healthz_git_sha: health.git_sha,
    version: health.version,
    node: process.version,
  });
  record({ meta: 'expected_before_run', expected: EXPECTED });
  if (args['expect-sha']) {
    check('server reports the expected SHA', health.git_sha === args['expect-sha'], { healthz_git_sha: health.git_sha });
  }
  adminToken = readFileSync(join(dir, 'server', 'data', 'bootstrap.token'), 'utf8').trim();
  secrets.set(adminToken, 'bootstrap');
  await scenario();
} catch (error) {
  thrown = error;
  failed += 1;
  record({ error: String(error?.message ?? error) });
} finally {
  server.kill('SIGTERM');
  const stop = await Promise.race([exited, sleep(10000).then(() => null)]);
  if (!stop) server.kill('SIGKILL');
  await exited;
  const serverErrors = log.split('\n').filter((line) => /\bERROR\b|panicked/.test(line));
  check('server log free of errors', serverErrors.length === 0, { error_lines: serverErrors.length });
  record({ meta: 'cleanup', server_stop: stop, server_log_lines: log.split('\n').length });
  rmSync(dir, { recursive: true, force: true });
  check('scenario directory removed', !existsSync(dir));
}

const passes = trace.filter((entry) => entry.result === 'PASS').length;
record({ meta: 'summary', pass: passes, fail: failed });
const text = `${trace.map((entry) => JSON.stringify(entry)).join('\n')}\n`;
if ([...secrets.keys()].some((secret) => text.includes(secret))) {
  console.error('secret in trace, not written');
  process.exit(3);
}
writeFileSync(args.out, text);
console.log(`checks: ${passes} passed, ${failed} failed; trace ${args.out}`);
if (thrown) console.error(thrown);
process.exit(failed === 0 ? 0 : 1);
