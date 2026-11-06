#!/usr/bin/env node
// Real-binary scenario for an interrupted device sync: daruma-desktop stopped
// between the local log and the projectors, against a live daruma-server.
//
//   GIT_SHA=$(git rev-parse HEAD) cargo build -p daruma-server -p daruma-desktop
//   node scripts/scenarios/interrupted-desktop-sync.mjs \
//     --server "$CARGO_TARGET_DIR/debug/daruma-server" \
//     --desktop "$CARGO_TARGET_DIR/debug/daruma-desktop" \
//     --old-desktop /path/to/daruma-desktop-before-the-fix --out /tmp/trace.jsonl
//
// Every round compares the device with a reference device that synced the same
// log without interruption. Writes one JSON object per line and exits 1 if any
// check failed. Passing the old binary as --desktop is the negative control.
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
    port: { type: 'string', default: '18951' },
    rounds: { type: 'string', default: '12' },
    'expect-sha': { type: 'string' },
  },
});
if (!args.server || !args.desktop || !args['old-desktop'] || !args.out) {
  console.error(
    'usage: interrupted-desktop-sync.mjs --server <path> --desktop <path> --old-desktop <path> --out <trace.jsonl>',
  );
  process.exit(2);
}

const TASKS = 200;
const EXPECTED = [
  'a sync stopped by a failing projector exits non-zero and the next process brings the device level with the reference',
  'a sync killed with SIGKILL at any point of a batch leaves a device the next process brings level with the reference',
  'a device the old binary left with a logged, never projected event below its cursor is repaired by the first sync of the new binary, without losing work done offline',
  'no event is logged twice on any device',
];

const dir = mkdtempSync(join(tmpdir(), 'daruma-interrupted-sync-'));
const base = `http://127.0.0.1:${args.port}/v1`;
const trace = [];
const secrets = new Map();
let failed = 0;

function record(entry) {
  trace.push({ n: trace.length + 1, at: new Date().toISOString(), ...entry });
}

function check(name, ok, detail = {}) {
  if (!ok) failed += 1;
  record({ check: name, result: ok ? 'PASS' : 'FAIL', ...detail });
}

async function http(step, method, path, bearer, body) {
  const started = performance.now();
  const response = await fetch(`${base}${path}`, {
    method,
    headers: {
      ...(bearer ? { authorization: `Bearer ${bearer}` } : {}),
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
  constructor(name, token) {
    this.name = name;
    this.token = token;
    this.dir = join(dir, name);
    mkdirSync(this.dir);
    this.db = join(this.dir, 'replica.sqlite');
  }

  env() {
    return {
      ...proxyFree,
      DARUMA_DATA_DIR: this.dir,
      DARUMA_API_URL: `http://127.0.0.1:${args.port}`,
      DARUMA_TOKEN: this.token,
    };
  }

  run(binary, ...argv) {
    const child = spawnSync(binary, argv, { env: this.env(), encoding: 'utf8' });
    return { status: child.status, stdout: child.stdout.trim(), stderr: child.stderr.trim() };
  }

  sync(step, binary) {
    const result = this.run(binary, 'sync', '--limit', '1000');
    record({
      step,
      device: this.name,
      process: 'daruma-desktop sync',
      binary: binary === args.desktop ? 'under_test' : 'old',
      exit: result.status,
      stdout: result.stdout.slice(0, 200),
      stderr: result.stderr.slice(-200),
    });
    return result;
  }

  /** Sync until a round finds nothing new. */
  settle(step, binary) {
    for (let round = 0; round < 20; round += 1) {
      const before = this.cursor();
      const result = this.sync(step, binary);
      if (result.status === 0 && this.cursor() === before && round > 0) return true;
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

  write(sql) {
    const db = new DatabaseSync(this.db);
    try {
      db.exec(sql);
    } finally {
      db.close();
    }
  }

  cursor() {
    return existsSync(this.db) ? this.query('SELECT server_seq FROM desktop_replica_state')[0].server_seq : 0;
  }

  /** Everything two devices of one log must agree on. */
  state() {
    return {
      tasks: this.query('SELECT id, title, description, status FROM tasks ORDER BY id'),
      comments: this.query('SELECT id, task_id, body FROM comments ORDER BY id'),
      projects: this.query('SELECT id, title FROM projects ORDER BY id'),
      feed: this.query(
        'SELECT event_id, verb, task_id FROM activity WHERE task_id IS NOT NULL ORDER BY event_id',
      ),
      versions: this.query(
        'SELECT entity_id, version_number, event_type FROM entity_versions ORDER BY entity_id, version_number',
      ),
    };
  }

  duplicates() {
    return this.query('SELECT event_id FROM events GROUP BY event_id HAVING COUNT(*) > 1').length;
  }
}

function compare(step, device, reference, detail = {}) {
  const ours = device.state();
  const theirs = reference.state();
  const same = {};
  for (const key of Object.keys(ours)) {
    same[key] = JSON.stringify(ours[key]) === JSON.stringify(theirs[key]);
  }
  check(`${step}: ${device.name} matches the reference`, Object.values(same).every(Boolean), {
    ...detail,
    same,
    tasks: [ours.tasks.length, theirs.tasks.length],
    comments: [ours.comments.length, theirs.comments.length],
    feed: [ours.feed.length, theirs.feed.length],
    versions: [ours.versions.length, theirs.versions.length],
  });
  check(`${step}: ${device.name} logged every event once`, device.duplicates() === 0);
}

/** Start a sync and SIGKILL it once `events` rows reached its local log. */
async function killedSync(step, device, events) {
  const child = spawn(args.desktop, ['sync', '--limit', '1000'], {
    env: device.env(),
    stdio: 'ignore',
  });
  const closed = new Promise((resolve) => child.on('close', (code, signal) => resolve({ code, signal })));
  const finished = { value: null };
  closed.then((value) => { finished.value = value; });
  let logged = 0;
  while (!finished.value) {
    if (existsSync(device.db)) {
      try {
        logged = device.query('SELECT COUNT(*) AS n FROM events')[0].n;
      } catch {
        logged = 0;
      }
      if (logged >= events) {
        child.kill('SIGKILL');
        break;
      }
    }
    await sleep(1);
  }
  const exit = await closed;
  record({ step, device: device.name, kill_after_events: events, logged_at_kill: logged, exit });
  return exit.signal === 'SIGKILL';
}

async function seed(admin) {
  const command = async (body) => {
    const { status, json } = await http('seed', 'POST', '/commands', admin, { command: body });
    if (status !== 200) throw new Error(`seed: ${status} ${JSON.stringify(json)}`);
    return json.data;
  };
  const project = (await command({ type: 'create_project', title: 'interrupted sync' }))[0];
  const projectId = project.payload.project.id;
  const materialized = await command({
    type: 'materialize_plan',
    plan: { title: 'seed', project_id: projectId, owner: { kind: 'user' }, source_brief: 'scenario' },
    tasks: Array.from({ length: TASKS }, (_, i) => ({
      title: `task ${String(i).padStart(3, '0')}`,
    })),
  });
  const taskIds = materialized
    .filter((event) => event.payload.type === 'task_created')
    .map((event) => event.payload.task.id);
  for (const taskId of taskIds.slice(0, 5)) {
    await command({ type: 'add_comment', comment: { task_id: taskId, body: `about ${taskId}` } });
  }
  record({ step: 'seed', tasks: taskIds.length });
  return taskIds;
}

async function scenario(admin) {
  await seed(admin);
  const reference = new Device('reference', admin);
  check('reference device settles', reference.settle('reference', args.desktop));
  const { tasks } = reference.state();
  check('reference holds the whole log', tasks.length === TASKS, { tasks: tasks.length });

  // A projector fails on the very first write after the append.
  const stopped = new Device('stopped', admin);
  stopped.run(args.desktop, 'list');
  stopped.write(
    `CREATE TRIGGER stop_sync BEFORE INSERT ON tasks WHEN NEW.title = 'task 050'
     BEGIN SELECT RAISE(ABORT, 'sync stopped'); END`,
  );
  const failing = stopped.sync('projector_fails', args.desktop);
  check('a failing projector fails the sync', failing.status !== 0, { exit: failing.status });
  stopped.write('DROP TRIGGER stop_sync');
  check('the next process settles', stopped.settle('projector_fails', args.desktop));
  compare('projector_fails', stopped, reference);

  // The process dies for real, somewhere in a batch.
  const rounds = Number(args.rounds);
  let killed = 0;
  for (let round = 0; round < rounds; round += 1) {
    const device = new Device(`killed-${round}`, admin);
    const events = 20 + Math.floor(((TASKS - 40) * (round + 1)) / (rounds + 1));
    if (await killedSync('sigkill', device, events)) killed += 1;
    check(`round ${round} settles`, device.settle('sigkill', args.desktop));
    compare('sigkill', device, reference, { round, kill_after_events: events });
  }
  check('every round was really killed', killed === rounds, { killed, rounds });

  // A device the old binary damaged for real, then an upgrade.
  const damaged = new Device('damaged', admin);
  damaged.run(args['old-desktop'], 'list');
  damaged.write(
    `CREATE TRIGGER stop_sync BEFORE INSERT ON tasks WHEN NEW.title = 'task 100'
     BEGIN SELECT RAISE(ABORT, 'sync stopped'); END`,
  );
  const oldFailure = damaged.sync('old_binary', args['old-desktop']);
  damaged.write('DROP TRIGGER stop_sync');
  check('the old binary stops on the trigger', oldFailure.status !== 0, { exit: oldFailure.status });
  check('the old binary settles', damaged.settle('old_binary', args['old-desktop']));
  const lost = (title) => damaged.query('SELECT id FROM tasks WHERE title = ?', title).length === 0;
  const cursorBefore = damaged.cursor();
  check('the fixture is really damaged', lost('task 100') && cursorBefore >= reference.cursor() - 1, {
    task_100_missing: lost('task 100'),
    cursor: cursorBefore,
    reference_cursor: reference.cursor(),
  });
  const offline = damaged.run(args['old-desktop'], 'add', 'written offline before the upgrade');
  record({ step: 'offline', exit: offline.status, stdout: offline.stdout.slice(0, 160) });
  const pending = damaged.query('SELECT COUNT(*) AS n FROM desktop_outbox WHERE flushed_at IS NULL')[0].n;
  check('offline work is pending at the upgrade', pending === 1, { pending });

  // A replica from before bindings waits for this word; desktop-server-binding.mjs covers why.
  const adopted = damaged.run(args.desktop, 'replica', 'adopt');
  check('the replica is adopted by its server', adopted.status === 0, { exit: adopted.status });
  const repaired = damaged.sync('upgrade', args.desktop);
  check('the first sync of the new binary succeeds', repaired.status === 0, { exit: repaired.status });
  check('the lost task is back', !lost('task 100'));
  check('offline work survives', !lost('written offline before the upgrade'));
  check('cursor did not move back', damaged.cursor() >= cursorBefore, { cursor: damaged.cursor() });
  check('device settles', damaged.settle('upgrade', args.desktop));
  check('reference settles again', reference.settle('upgrade', args.desktop));
  compare('upgrade', damaged, reference);
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
  const admin = readFileSync(join(dir, 'server', 'data', 'bootstrap.token'), 'utf8').trim();
  secrets.set(admin, 'bootstrap');
  await scenario(admin);
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
