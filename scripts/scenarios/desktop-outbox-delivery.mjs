#!/usr/bin/env node
// Real-binary scenario for local changes on their way to the server:
// daruma-desktop commands stopped around their commit, against a live
// daruma-server, and a device an old binary left with unqueued events.
//
//   GIT_SHA=$(git rev-parse HEAD) cargo build -p daruma-server -p daruma-desktop
//   node scripts/scenarios/desktop-outbox-delivery.mjs \
//     --server "$CARGO_TARGET_DIR/debug/daruma-server" \
//     --desktop "$CARGO_TARGET_DIR/debug/daruma-desktop" \
//     --old-desktop /path/to/daruma-desktop-before-the-fix --out /tmp/trace.jsonl
//
// Writes one JSON object per line and exits 1 if any check failed. Passing the
// old binary as --desktop is the negative control.
import { spawn, spawnSync } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
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
    port: { type: 'string', default: '18953' },
    rounds: { type: 'string', default: '24' },
    'expect-sha': { type: 'string' },
  },
});
if (!args.server || !args.desktop || !args['old-desktop'] || !args.out) {
  console.error(
    'usage: desktop-outbox-delivery.mjs --server <path> --desktop <path> --old-desktop <path> --out <trace.jsonl>',
  );
  process.exit(2);
}

const dir = mkdtempSync(join(tmpdir(), 'daruma-outbox-delivery-'));
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


const EXPECTED = [
  'a command stopped between its commit and its projectors exits non-zero; after a restart its task is on the device, on the server and on a reference device',
  'a command whose outbox write fails exits non-zero and leaves no event behind, also for the second event of a multi-event command; a retry delivers all of it',
  'a command killed with SIGKILL at any point leaves every logged event in the outbox',
  'an event the server accepted before the device could record it reaches the server once, under one identity',
  'events the old binary logged and never queued reach the server once and in log order at the first sync after the upgrade; incoming, delivered, pending and refused events keep their state',
  'later syncs and restarts add nothing to either log',
];

const bare = (id) => id.replace(/^evt_/, '');

async function serverLog(step) {
  const log = [];
  for (let since = 0; ; since = log.at(-1).seq) {
    const { status, json } = await http(step, 'GET', `/events?since=${since}&limit=1000`, adminToken);
    if (status !== 200) throw new Error(`events: ${status}`);
    if (json.length === 0) return log;
    log.push(...json);
  }
}

const taskEvents = (log, taskId) => log.filter((event) => event.payload.task_id === taskId
  || event.payload.task?.id === taskId?.replace(/^tsk_/, '')
  || event.payload.id === taskId);

let adminToken = '';

function add(device, binary, title) {
  const result = device.run(binary, 'add', title);
  record({ step: 'add', device: device.name, title, binary: binary === args.desktop ? 'under_test' : 'old', exit: result.status, stderr: result.stderr.slice(-160) });
  return result;
}

function done(device, binary, id) {
  const result = device.run(binary, 'done', id);
  record({ step: 'done', device: device.name, task: id, binary: binary === args.desktop ? 'under_test' : 'old', exit: result.status, stderr: result.stderr.slice(-160) });
  return result;
}

const taskId = (device, title) => device.query('SELECT id FROM tasks WHERE title = ?', title)[0]?.id;
const hasTask = (device, title) => taskId(device, title) !== undefined;
const logCount = (device) => device.query('SELECT COUNT(*) AS n FROM events')[0].n;
const pendingCount = (device) => device.query('SELECT COUNT(*) AS n FROM desktop_outbox WHERE flushed_at IS NULL')[0].n;
const unqueued = (device) => device.query(
  `SELECT event_id, kind FROM events WHERE origin_device_id IS NULL AND kind LIKE 'task%'
   AND event_id NOT IN (SELECT 'evt_' || json_extract(envelope_json, '$.id') FROM desktop_outbox)`,
);

function stopWrites(device, table, when = '1', op = 'INSERT') {
  device.write(
    `CREATE TRIGGER stop_write BEFORE ${op} ON ${table} WHEN ${when}
     BEGIN SELECT RAISE(ABORT, 'stopped'); END`,
  );
}
const letWrites = (device) => device.write('DROP TRIGGER stop_write');

async function seedTitles(admin) {
  const { status, json } = await http('seed', 'POST', '/commands', admin, {
    command: { type: 'create_project', title: 'outbox delivery' },
  });
  if (status !== 200) throw new Error(`seed: ${status}`);
  const projectId = json.data[0].payload.project.id;
  const planned = await http('seed', 'POST', '/commands', admin, {
    command: {
      type: 'materialize_plan',
      plan: { title: 'seed', project_id: projectId, owner: { kind: 'user' }, source_brief: 'scenario' },
      tasks: [{ title: 'seeded on the server' }],
    },
  });
  if (planned.status !== 200) throw new Error(`seed: ${planned.status}`);
  return planned.json.data.find((event) => event.payload.type === 'task_created');
}

/** An event that reads like a local command in any device's log. */
async function lookalike(admin, template) {
  const event = structuredClone(template);
  event.id = randomUUID();
  event.seq = 0;
  event.actor = { kind: 'user' };
  delete event.origin_device_id;
  event.origin_seq = 0;
  event.payload.task.id = randomUUID();
  event.payload.task.title = 'incoming, reads like a local command';
  delete event.payload.task.project_id;
  const { status } = await http('lookalike', 'POST', '/events/replica', admin, { events: [event] });
  if (status !== 200) throw new Error(`lookalike: ${status}`);
  return event;
}

async function stoppedBeforeProjectors(reference, table) {
  const title = `stopped at ${table}`;
  const device = new Device(`stopped-${table}`, adminToken);
  check(`${table}: device syncs`, device.sync('stopped_command', args.desktop).status === 0);
  const incoming = unqueued(device).length;
  stopWrites(device, table);
  const stopped = add(device, args.desktop, title);
  letWrites(device);
  check(`${table}: the command fails`, stopped.status !== 0, { exit: stopped.status });
  check(`${table}: its event is queued`, unqueued(device).length === incoming && pendingCount(device) === 1, {
    pending: pendingCount(device),
  });
  check(`${table}: next process settles`, device.settle('stopped_command', args.desktop));
  check(`${table}: the task is on the device`, hasTask(device, title));
  check(`${table}: the task reached a reference device`, reference.settle('stopped_command', args.desktop) && hasTask(reference, title));
  compare(`stopped_at_${table}`, device, reference);
  return device;
}

async function failedOutboxWrite(reference, failingEvent) {
  const device = new Device(`refused-write-${failingEvent}`, adminToken);
  const title = `finished after a failed write ${failingEvent}`;
  check('device adds its task', add(device, args.desktop, title).status === 0);
  check('device syncs', device.settle('failed_write', args.desktop));
  const id = taskId(device, title);
  const [queued, log, status] = [
    device.query('SELECT COUNT(*) AS n FROM desktop_outbox')[0].n,
    logCount(device),
    device.query('SELECT status FROM tasks WHERE id = ?', id)[0].status,
  ];
  stopWrites(device, 'desktop_outbox', `(SELECT COUNT(*) FROM desktop_outbox) >= ${queued + failingEvent}`);
  const failed = done(device, args.desktop, id);
  letWrites(device);
  check(`write ${failingEvent}: the command fails`, failed.status !== 0, { exit: failed.status });
  check(`write ${failingEvent}: nothing is left behind`, logCount(device) === log
    && pendingCount(device) === 0
    && device.query('SELECT status FROM tasks WHERE id = ?', id)[0].status === status, {
    log: [log, logCount(device)],
    pending: pendingCount(device),
  });
  check(`write ${failingEvent}: the retry succeeds`, done(device, args.desktop, id).status === 0);
  check(`write ${failingEvent}: device settles`, device.settle('failed_write', args.desktop));
  check(`write ${failingEvent}: reference settles`, reference.settle('failed_write', args.desktop));
  check(`write ${failingEvent}: the task is done on the reference`,
    reference.query('SELECT status FROM tasks WHERE id = ?', id)[0]?.status === 'done');
  compare(`failed_write_${failingEvent}`, device, reference);
}

async function killedCommands(reference) {
  const device = new Device('killed', adminToken);
  check('killed: device syncs', device.sync('sigkill', args.desktop).status === 0);
  const incoming = unqueued(device).length;
  const started = performance.now();
  add(device, args.desktop, 'measures a command');
  const span = performance.now() - started;
  const rounds = Number(args.rounds);
  let killed = 0;
  let killedAfterCommit = 0;
  let notQueued = 0;
  for (let round = 0; round < rounds; round += 1) {
    const before = logCount(device);
    const child = spawn(args.desktop, ['add', `killed ${round}`], { env: device.env(), stdio: 'ignore' });
    const closed = new Promise((resolve) => child.on('close', (code, signal) => resolve({ code, signal })));
    await sleep(span * (0.05 + (0.75 * round) / rounds));
    child.kill('SIGKILL');
    const exit = await closed;
    if (exit.signal === 'SIGKILL') killed += 1;
    if (exit.signal === 'SIGKILL' && logCount(device) > before) killedAfterCommit += 1;
    notQueued += unqueued(device).length - incoming;
    record({ step: 'sigkill', round, exit, log_grew: logCount(device) > before });
  }
  check('every logged event is queued whatever the moment of the kill', notQueued === 0, {
    rounds, killed, killed_after_commit: killedAfterCommit, not_queued: notQueued,
  });
  check('some kills landed', killed > 0, { killed });
  check('killed: device settles', device.settle('sigkill', args.desktop));
  check('killed: reference settles', reference.settle('sigkill', args.desktop));
  compare('sigkill', device, reference, { rounds, killed, killed_after_commit: killedAfterCommit });
}

async function unrecordedAccept(reference) {
  const device = new Device('unrecorded', adminToken);
  const title = 'accepted, not recorded';
  add(device, args.desktop, title);
  check('unrecorded: device syncs', device.settle('unrecorded_accept', args.desktop));
  add(device, args.desktop, `${title} again`);
  const id = bare(device.query(
    "SELECT event_id FROM events WHERE json_extract(payload_json, '$.task.title') = ?",
    `${title} again`,
  )[0].event_id);
  stopWrites(device, 'desktop_outbox', '1', 'UPDATE');
  const failed = device.sync('unrecorded_accept', args.desktop);
  letWrites(device);
  check('unrecorded: the sync fails on the mark', failed.status !== 0, { exit: failed.status });
  const onServer = (await serverLog('unrecorded_accept')).filter((event) => event.id === id);
  check('unrecorded: the server already holds it', onServer.length === 1);
  const before = onServer[0];
  for (let restart = 0; restart < 3; restart += 1) device.sync('unrecorded_accept', args.desktop);
  const after = (await serverLog('unrecorded_accept')).filter((event) => event.id === id);
  check('unrecorded: reaches the server once, under one identity', after.length === 1
    && after[0].seq === before.seq
    && after[0].origin_device_id === before.origin_device_id
    && after[0].origin_seq === before.origin_seq, { copies: after.length });
  check('unrecorded: nothing stays pending', pendingCount(device) === 0);
  check('unrecorded: reference settles', reference.settle('unrecorded_accept', args.desktop));
  compare('unrecorded_accept', device, reference);
}

async function upgradedDevice(reference, template) {
  const old = args['old-desktop'];
  const other = new Device('other-device', adminToken);
  add(other, args.desktop, 'made on another device');
  check('other device syncs', other.settle('upgrade', args.desktop));
  const incoming = await lookalike(adminToken, template);

  const device = new Device('upgraded', adminToken);
  check('old binary syncs', device.settle('upgrade', old));
  check('incoming event in the old log', hasTask(device, 'incoming, reads like a local command'));

  add(device, old, 'delivered by the old binary');
  check('old binary delivers', device.settle('upgrade', old));

  stopWrites(device, 'desktop_outbox');
  const lost = add(device, old, 'lost by the old binary');
  letWrites(device);
  check('the old binary loses a create', lost.status !== 0, { exit: lost.status });

  add(device, old, 'partly lost by the old binary');
  const partly = taskId(device, 'partly lost by the old binary');
  const queued = device.query('SELECT COUNT(*) AS n FROM desktop_outbox')[0].n;
  stopWrites(device, 'desktop_outbox', `(SELECT COUNT(*) FROM desktop_outbox) >= ${queued + 1}`);
  const partial = done(device, old, partly);
  letWrites(device);
  check('the old binary queues a prefix of a command', partial.status !== 0, { exit: partial.status });

  add(device, old, 'refused by the server');
  const refused = device.query(
    "SELECT id, json_extract(envelope_json, '$.id') AS event FROM desktop_outbox ORDER BY id DESC LIMIT 1",
  )[0];
  device.write(
    `INSERT INTO desktop_outbox_rejected (outbox_id, status, code)
     VALUES (${refused.id}, 403, 'intake_source_admin_only')`,
  );
  add(device, old, 'pending at the upgrade');

  const missing = unqueued(device);
  const lookalikeId = `evt_${incoming.id}`;
  check('the fixture is really damaged', missing.length > 3, {
    not_queued: missing.map((row) => row.kind),
  });
  const before = await serverLog('upgrade');
  const logBefore = logCount(device);

  const upgrade = device.sync('upgrade', args.desktop);
  check('the first sync of the new binary succeeds', upgrade.status === 0, { exit: upgrade.status });
  check('it reports what it restored', /restored 3 event/.test(upgrade.stdout), { stdout: upgrade.stdout.slice(0, 160) });

  const after = await serverLog('upgrade');
  const added = after.slice(before.length);
  const titles = added.filter((e) => e.payload.type === 'task_created').map((e) => e.payload.task.title);
  check('lost, pending and partly lost work reached the server', [
    'lost by the old binary', 'partly lost by the old binary', 'pending at the upgrade',
  ].every((title) => titles.includes(title)), { titles });
  check('nothing else went out', added.length === 6, {
    added: added.map((e) => e.payload.type),
  });
  const seen = new Set(after.map((event) => event.id));
  check('every event is on the server once', seen.size === after.length);
  check('no local event went out twice or came back new', device.duplicates() === 0 && logCount(device) === logBefore, {
    log: [logBefore, logCount(device)],
  });
  check('the incoming event was not sent back', after.filter((event) => `evt_${event.id}` === lookalikeId).length === 1);

  const order = device.query(
    `SELECT event_id FROM events WHERE event_id IN (${added.map((e) => `'evt_${e.id}'`).join(',')}) ORDER BY seq`,
  ).map((row) => row.event_id);
  check('events went out in the order of the local log', JSON.stringify(order) === JSON.stringify(added.map((e) => `evt_${e.id}`)));

  check('the refused event stays refused', !after.some((event) => event.id === refused.event)
    && device.query('SELECT COUNT(*) AS n FROM desktop_outbox_rejected')[0].n === 1);
  check('nothing stays pending', pendingCount(device) === 1, { pending: pendingCount(device), left: 'the refused one' });

  const settled = after.length;
  check('upgraded: device settles', device.settle('upgrade', args.desktop));
  check('later syncs add nothing', (await serverLog('upgrade')).length === settled);
  check('upgraded: reference settles', reference.settle('upgrade', args.desktop));
  const left = (await serverLog('upgrade')).find((event) => event.payload.task?.title === 'partly lost by the old binary');
  check('the partly lost task is done on the reference', reference.query(
    "SELECT status FROM tasks WHERE title = 'partly lost by the old binary'",
  )[0]?.status === 'done', { event: left?.id });
}

async function scenario(admin) {
  adminToken = admin;
  const template = await seedTitles(admin);
  const reference = new Device('reference', admin);
  check('reference device settles', reference.settle('reference', args.desktop));

  await stoppedBeforeProjectors(reference, 'tasks');
  await stoppedBeforeProjectors(reference, 'activity');
  for (const failingEvent of [0, 1, 2]) await failedOutboxWrite(reference, failingEvent);
  await killedCommands(reference);
  await unrecordedAccept(reference);
  await upgradedDevice(reference, template);
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
