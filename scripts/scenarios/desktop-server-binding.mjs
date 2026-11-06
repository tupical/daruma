#!/usr/bin/env node
// Real-binary scenario for what a replica belongs to: one daruma-desktop data
// directory moves between two live daruma-servers through real `pair` and `sync`,
// meets an address override, servers that cannot name themselves, a replica from
// before bindings and a switch stopped halfway.
//
//   GIT_SHA=$(git rev-parse HEAD) cargo build -p daruma-server -p daruma-desktop
//   node scripts/scenarios/desktop-server-binding.mjs \
//     --server "$CARGO_TARGET_DIR/debug/daruma-server" \
//     --desktop "$CARGO_TARGET_DIR/debug/daruma-desktop" \
//     --old-desktop /path/to/daruma-desktop-before-the-fix --out /tmp/trace.jsonl
//
// Writes one JSON object per line and exits 1 if any check failed. The old binary
// serves the replica from before bindings and the negative control at the end.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
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
    port: { type: 'string', default: '18993' },
    'expect-sha': { type: 'string' },
  },
});
if (!args.server || !args.desktop || !args['old-desktop'] || !args.out) {
  console.error(
    'usage: desktop-server-binding.mjs --server <path> --desktop <path> --old-desktop <path> --out <trace.jsonl>',
  );
  process.exit(2);
}

const EXPECTED = [
  'A to B: no payload of A reaches B, B arrives complete from its own start and equals a clean replica of B, A stays intact and in sight',
  'back to A: its own queue is delivered once, nothing of B goes to A, B never sees it; another token of A resets nothing',
  'override: another server behind the URL is refused before anything moves; the same server at a new address is not',
  'no identity: a server without one, with an error or a path for one gets nothing but the identity request',
  'older replica: refused on any server until the user adopts it, then delivers its queue once, also after a re-pair',
  'older queue no server has seen (cursor 0): a pair with B sends B nothing, the queue stays in sight, and it reaches A once after A is adopted; written offline on a new replica goes with the first server',
  'interrupted change: paired.json ahead of replicas.json is finished by the next process, a broken replicas.json stops it',
  'old binary: re-pairing sends the queue of A to B and misses part of the history of B (the defect)',
];

const dir = mkdtempSync(join(tmpdir(), 'daruma-binding-'));
const httpPort = Number(args.port);
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

const proxyFree = Object.fromEntries(
  Object.entries(process.env).filter(
    ([key]) => !/^(https?|no|all)_proxy$/i.test(key) && !/^(DARUMA_API_URL|DARUMA_TOKEN|SSL_CERT_(FILE|DIR))$/.test(key),
  ),
);

class Server {
  constructor(name, port) {
    Object.assign(this, { name, port, tls: port + 1, home: join(dir, name), log: '' });
  }

  get origin() {
    return `http://127.0.0.1:${this.port}`;
  }

  async start(port = this.port) {
    this.port = port;
    this.child = spawn(args.server, [], {
      env: {
        ...proxyFree,
        DARUMA_AGENT_DIR: this.home,
        DARUMA_HOSTNAME: '127.0.0.1',
        DARUMA_PORT: String(this.port),
        DARUMA_TLS_PORT: String(this.tls),
        DARUMA_MDNS_DISABLE: '1',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    this.child.stdout.on('data', (chunk) => { this.log += chunk; });
    this.child.stderr.on('data', (chunk) => { this.log += chunk; });
    this.closed = new Promise((resolve) => this.child.on('close', (code, signal) => resolve({ code, signal })));
    const deadline = Date.now() + 30000;
    for (;;) {
      if (Date.now() > deadline) throw new Error(`${this.name} never answered /healthz`);
      if (this.child.exitCode !== null) throw new Error(`${this.name} exited: ${this.child.exitCode}`);
      const health = await fetch(`${this.origin}/v1/healthz`).then((r) => (r.ok ? r.json() : null)).catch(() => null);
      if (health) {
        this.admin = readFileSync(join(this.home, 'data', 'bootstrap.token'), 'utf8').trim();
        secrets.set(this.admin, `${this.name} bootstrap`);
        return health;
      }
      await sleep(100);
    }
  }

  async stop() {
    if (!this.child) return null;
    this.child.kill('SIGTERM');
    const stopped = await Promise.race([this.closed, sleep(10000).then(() => null)]);
    if (!stopped) this.child.kill('SIGKILL');
    await this.closed;
    this.child = null;
    return stopped;
  }

  async api(step, method, path, body) {
    const response = await fetch(`${this.origin}/v1${path}`, {
      method,
      headers: {
        authorization: `Bearer ${this.admin}`,
        ...(body ? { 'content-type': 'application/json' } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    const text = await response.text();
    record({
      step,
      server: this.name,
      http: `${method} ${path}`,
      status: response.status,
      req_id: response.headers.get('x-request-id'),
    });
    return { status: response.status, json: text ? JSON.parse(text) : null };
  }

  async identity(step) {
    const { status, json } = await this.api(step, 'GET', '/events/origin');
    if (status !== 200) throw new Error(`origin: ${status}`);
    return json.server_id;
  }

  async ticket(step) {
    const { status, json } = await this.api(step, 'GET', '/devices/pair/ticket');
    if (status !== 200) throw new Error(`ticket: ${status}`);
    secrets.set(new URLSearchParams(json.pairing_url.split('?')[1]).get('token'), 'pairing ticket');
    return json;
  }

  async seed(step, title) {
    const project = await this.api(step, 'POST', '/commands', {
      command: { type: 'create_project', title },
    });
    if (project.status !== 200) throw new Error(`seed: ${project.status}`);
    const planned = await this.api(step, 'POST', '/commands', {
      command: {
        type: 'materialize_plan',
        plan: {
          title,
          project_id: project.json.data[0].payload.project.id,
          owner: { kind: 'user' },
          source_brief: 'scenario',
        },
        tasks: [{ title }],
      },
    });
    if (planned.status !== 200) throw new Error(`seed: ${planned.status}`);
  }

  /** The whole log: what reached this server, as text and by count. */
  async events(step) {
    const { json } = await this.api(step, 'GET', '/events?since=0&limit=1000');
    return { text: JSON.stringify(json), count: json.length };
  }
}

class Device {
  constructor(name) {
    this.name = name;
    this.dir = join(dir, name);
    this.paired = join(this.dir, 'paired.json');
    this.registry = join(this.dir, 'replicas.json');
    mkdirSync(this.dir);
  }

  run(step, binary, argv, env = {}) {
    return new Promise((resolve) => {
      const child = spawn(binary, argv, {
        env: { ...proxyFree, DARUMA_DATA_DIR: this.dir, ...env },
        stdio: ['ignore', 'pipe', 'pipe'],
      });
      let stdout = '';
      let stderr = '';
      child.stdout.on('data', (chunk) => { stdout += chunk; });
      child.stderr.on('data', (chunk) => { stderr += chunk; });
      child.on('close', (status) => {
        const result = { status, stdout: stdout.trim(), stderr: stderr.trim() };
        record({
          step,
          device: this.name,
          process: `daruma-desktop ${argv[0]}`,
          binary: binary === args.desktop ? 'under_test' : 'old',
          exit: status,
          stdout: result.stdout.slice(0, 400),
          stderr: result.stderr.slice(0, 700),
        });
        resolve(result);
      });
    });
  }

  /** The server allows five pairs a minute per address: wait out a 429. */
  async pair(step, server, binary = args.desktop) {
    const deadline = Date.now() + 90000;
    for (;;) {
      const ticket = await server.ticket(step);
      const paired = await this.run(step, binary, ['pair', ticket.pairing_url]);
      if (paired.status === 0) break;
      if (!paired.stderr.includes('HTTP 429') || Date.now() > deadline) {
        throw new Error(`pair ${server.name}: ${paired.stderr}`);
      }
      await sleep(5000);
    }
    secrets.set(JSON.parse(readFileSync(this.paired, 'utf8')).token, 'device token');
  }

  sync(step, env, binary = args.desktop) {
    return this.run(step, binary, ['sync', '--limit', '1000'], env);
  }

  add(step, title, binary = args.desktop) {
    return this.run(step, binary, ['add', title]);
  }

  async activePath(step) {
    return (await this.run(step, args.desktop, ['where'])).stdout;
  }
}

function inspect(path) {
  const db = new DatabaseSync(path, { readOnly: true });
  try {
    return {
      cursor: db.prepare('SELECT server_seq FROM desktop_replica_state WHERE id = 1').get().server_seq,
      pending: db.prepare('SELECT COUNT(*) AS n FROM desktop_outbox WHERE flushed_at IS NULL').get().n,
      titles: db.prepare('SELECT title FROM tasks ORDER BY title').all().map((row) => row.title),
      rows: db.prepare('SELECT id, title, status FROM tasks ORDER BY id').all(),
      projects: db.prepare('SELECT id FROM projects ORDER BY id').all(),
    };
  } finally {
    db.close();
  }
}

/** Plain HTTP stub that answers `origin` its way and records every request. */
async function stub(port, origin) {
  const seen = [];
  const server = createServer((request, response) => {
    seen.push(`${request.method} ${request.url}`);
    if (request.url === '/v1/events/origin') {
      response.writeHead(origin.status, { 'content-type': 'application/json' });
      response.end(origin.body);
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('[]');
  });
  await new Promise((resolve) => server.listen(port, '127.0.0.1', resolve));
  return {
    seen,
    close: () => new Promise((resolve) => {
      server.closeAllConnections();
      server.close(resolve);
    }),
  };
}

function digest(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

const a = new Server('server-a', httpPort);
const b = new Server('server-b', httpPort + 2);
const noEnv = {};

async function toAnotherServer(device) {
  const step = 'A to B';
  await a.seed(step, 'history of A');
  await device.pair(step, a);
  const first = await device.sync(step);
  check('first sync with A succeeds', first.status === 0, { stdout: first.stdout });
  await device.add(step, 'queued for A');
  const aPath = await device.activePath(step);
  const parked = inspect(aPath);
  check('A leaves a cursor and a queue', parked.cursor > 0 && parked.pending === 1, {
    cursor: parked.cursor,
    pending: parked.pending,
  });

  // B holds other events under the seqs the cursor of A has passed.
  await b.seed(step, 'early history of B');
  await b.seed(step, 'later history of B');
  await b.seed(step, 'latest history of B');
  const before = await b.events(step);
  check('B has events at and below the cursor of A', before.count > parked.cursor, {
    b_events: before.count,
    a_cursor: parked.cursor,
  });

  await device.pair(step, b);
  const bPath = await device.activePath(step);
  check('pairing with B selects another replica at once', bPath !== aPath, { path: bPath.replace(dir, '') });
  const synced = await device.sync(step);
  check('sync with B succeeds', synced.status === 0, { stdout: synced.stdout });

  const after = await b.events(step);
  check('no event of A reached B', after.count === before.count && !after.text.includes('queued for A'), {
    before: before.count,
    after: after.count,
  });

  const reference = new Device('clean-replica-of-b');
  await reference.pair(step, b);
  await reference.sync(step);
  const clean = inspect(join(reference.dir, 'replica.sqlite'));
  const replica = inspect(bPath);
  check('B arrived complete, from its own start', replica.cursor === clean.cursor && replica.titles.includes('early history of B'), {
    cursor: replica.cursor,
    clean_cursor: clean.cursor,
    titles: replica.titles,
  });
  check('B equals a clean replica of B', JSON.stringify(replica.rows) === JSON.stringify(clean.rows)
    && JSON.stringify(replica.projects) === JSON.stringify(clean.projects), {
    tasks: replica.rows.length,
    projects: replica.projects.length,
  });
  check('nothing of A is mixed into B', !replica.titles.includes('history of A') && !replica.titles.includes('queued for A'));
  const stayed = inspect(aPath);
  check('the replica of A did not move', JSON.stringify(stayed) === JSON.stringify(parked), {
    cursor: stayed.cursor,
    pending: stayed.pending,
  });
  const listed = await device.run(step, args.desktop, ['replica']);
  check('the queue of A stays in sight', listed.status === 0
    && listed.stdout.includes(`server ${a.id}  server_seq=${parked.cursor} pending=1`)
    && listed.stdout.includes(`* server ${b.id}`), { stdout: listed.stdout.replaceAll(dir, '') });

  await device.add(step, 'written for B');
  await device.sync(step);
  const withLocal = await b.events(step);
  check('work on B goes to B', withLocal.text.includes('written for B'));
  check('and not to A', !(await a.events(step)).text.includes('written for B'));
  return { aPath, bPath };
}

async function backToTheFirstServer(device, { aPath, bPath }) {
  const step = 'back to A';
  const beforeB = await b.events(step);
  await device.pair(step, a);
  check('pairing with A selects its own replica again', (await device.activePath(step)) === aPath);
  const synced = await device.sync(step);
  check('sync with A succeeds', synced.status === 0, { stdout: synced.stdout });
  const log = await a.events(step);
  check('the queue of A arrived once', log.text.split('queued for A').length === 2, {
    occurrences: log.text.split('queued for A').length - 1,
  });
  await device.sync(step);
  check('a retry adds nothing', (await a.events(step)).count === log.count);
  check('B never saw it', !(await b.events(step)).text.includes('queued for A'));
  check('B has no new events', (await b.events(step)).count === beforeB.count);
  check('A has nothing of B', !(await a.events(step)).text.includes('written for B'));

  const replica = inspect(aPath);
  await device.add(step, 'second for A');
  await device.pair(step, a);
  const same = await device.activePath(step);
  check('another token of A keeps the replica', same === aPath && !existsSync(join(device.dir, 'servers', a.id)), {
    servers_a_dir: existsSync(join(device.dir, 'servers', a.id)),
  });
  check('with its cursor and queue', inspect(aPath).cursor === replica.cursor && inspect(aPath).pending === 1);
  await device.sync(step);
  const again = await a.events(step);
  check('its queue arrives once', again.text.split('second for A').length === 2);
  check('the replica of B waits untouched', inspect(bPath).pending === 0);
}

async function overrides(device) {
  const step = 'override';
  await device.add(step, 'held under override');
  const held = inspect(await device.activePath(step));
  const before = await b.events(step);
  const refused = await device.sync(step, { DARUMA_API_URL: b.origin, DARUMA_TOKEN: b.admin });
  check('another server behind the URL is refused', refused.status !== 0 && refused.stderr.includes(`belongs to server ${a.id}`)
    && refused.stderr.includes('Nothing was sent or fetched'), { stderr: refused.stderr.slice(0, 300) });
  check('it received nothing', (await b.events(step)).count === before.count);
  const after = inspect(await device.activePath(step));
  check('the replica did not move', after.cursor === held.cursor && after.pending === held.pending);

  await a.stop();
  await a.start(a.port + 10);
  const sameServer = new Device('same-server-new-address');
  const first = await sameServer.sync(step, { DARUMA_API_URL: a.origin, DARUMA_TOKEN: a.admin });
  await a.stop();
  await a.start(a.port - 10);
  const moved = await sameServer.sync(step, { DARUMA_API_URL: `http://localhost:${a.port}`, DARUMA_TOKEN: a.admin });
  check('the same server at another address is no change', first.status === 0 && moved.status === 0
    && !existsSync(join(sameServer.dir, 'servers')), { stdout: moved.stdout });
}

async function serversWithoutIdentity(device) {
  const step = 'no identity';
  const cases = [
    ['older server, no endpoint', { status: 404, body: '{}' }, 'does not report which server'],
    ['failing server', { status: 500, body: '{}' }, 'server identity failed'],
    ['path for an identity', { status: 200, body: JSON.stringify({ server_id: '../../evil' }) }, 'unusable identity'],
    ['no identity in the answer', { status: 200, body: '{}' }, ''],
  ];
  let port = httpPort + 20;
  for (const [name, origin, why] of cases) {
    const server = await stub(port, origin);
    const result = await device.sync(step, { DARUMA_API_URL: `http://127.0.0.1:${port}`, DARUMA_TOKEN: 'stub' });
    port += 1;
    check(`${name}: refused`, result.status !== 0 && result.stderr.includes(why), { stderr: result.stderr.slice(0, 240) });
    check(`${name}: asked for the identity only`, server.seen.length === 1 && server.seen[0] === 'GET /v1/events/origin', {
      requests: server.seen,
    });
    await server.close();
  }
  check('no path from a server became a directory', !existsSync(join(device.dir, '..', '..', 'evil'))
    && !existsSync(join(device.dir, 'servers', '..', 'evil')));
}

async function replicaFromBeforeBindings() {
  const step = 'older replica';
  const device = new Device('older-replica');
  await device.run(step, args['old-desktop'], ['list']);
  const onA = { DARUMA_API_URL: a.origin, DARUMA_TOKEN: a.admin };
  const onB = { DARUMA_API_URL: b.origin, DARUMA_TOKEN: b.admin };
  const old = await device.sync(step, onA, args['old-desktop']);
  check('the old binary syncs', old.status === 0, { stdout: old.stdout });
  await device.add(step, 'queued before the upgrade', args['old-desktop']);
  const beforeB = await b.events(step);

  const refused = await device.sync(step, onB);
  check('refused on another server', refused.status !== 0 && refused.stderr.includes('does not record with which server'), {
    stderr: refused.stderr.slice(0, 300),
  });
  const own = await device.sync(step, onA);
  check('refused on its own too, nothing tells them apart', own.status !== 0 && own.stderr.includes('daruma replica adopt'));
  check('nothing reached B', (await b.events(step)).count === beforeB.count);
  check('nothing reached A', !(await a.events(step)).text.includes('queued before the upgrade'));

  await device.pair(step, b);
  const replica = inspect(join(device.dir, 'replica.sqlite'));
  check('pairing keeps the older replica in place', (await device.activePath(step)) === join(device.dir, 'replica.sqlite')
    && replica.pending === 1);
  const paired = await device.sync(step);
  check('and sync still waits for the user', paired.status !== 0 && paired.stderr.includes('does not record with which server'));
  check('even so nothing reached B', (await b.events(step)).count === beforeB.count);

  await device.pair(step, a);
  const adopted = await device.run(step, args.desktop, ['replica', 'adopt']);
  check('adopt records the server', adopted.status === 0 && adopted.stdout.includes(a.id), { stdout: adopted.stdout });
  const delivered = await device.sync(step);
  const log = await a.events(step);
  check('then the queue is delivered once', delivered.status === 0 && log.text.split('queued before the upgrade').length === 2, {
    stdout: delivered.stdout,
  });
  await device.pair(step, a);
  await device.sync(step);
  check('a later re-pair keeps it', (await device.activePath(step)) === join(device.dir, 'replica.sqlite')
    && (await a.events(step)).count === log.count);
}

async function queueNoServerHasSeen() {
  const step = 'older queue, cursor 0';
  const old = args['old-desktop'];
  const device = new Device('older-queue');
  await device.pair(step, a, old);
  await device.add(step, 'private queue for A', old);
  const legacyPath = join(device.dir, 'replica.sqlite');
  const before = inspect(legacyPath);
  check('an older replica with a queue and no cursor', before.cursor === 0 && before.pending === 1, before);
  check('it records no server', !existsSync(device.registry));
  const beforeA = await a.events(step);
  const beforeB = await b.events(step);

  await device.pair(step, b);
  check('pairing elsewhere keeps it in place', (await device.activePath(step)) === legacyPath);
  const paired = await device.sync(step);
  check('sync after the pair is refused', paired.status !== 0 && paired.stderr.includes('does not record with which server'), {
    stdout: paired.stdout,
    stderr: paired.stderr.slice(0, 300),
  });
  const onB = await b.events(step);
  check('B received nothing', onB.count === beforeB.count && !onB.text.includes('private queue for A'), {
    before: beforeB.count,
    after: onB.count,
  });
  const kept = inspect(legacyPath);
  check('the queue is kept', kept.pending === 1 && kept.cursor === 0, kept);
  check('nothing was recorded for the user', !existsSync(device.registry) || JSON.parse(readFileSync(device.registry, 'utf8')).active === null);
  const listed = await device.run(step, args.desktop, ['replica']);
  check('the queue stays in sight', listed.stdout.includes('server not recorded') && listed.stdout.includes('pending=1'), {
    stdout: listed.stdout,
  });

  const override = await device.sync(step, { DARUMA_API_URL: b.origin, DARUMA_TOKEN: b.admin });
  check('the same without a pair, by address', override.status !== 0 && override.stderr.includes('does not record with which server'));
  check('still nothing on B', (await b.events(step)).count === beforeB.count);
  check('and nothing on A before the user decides', (await a.events(step)).count === beforeA.count);

  await device.pair(step, a);
  const adopted = await device.run(step, args.desktop, ['replica', 'adopt']);
  check('adopted by its own server', adopted.status === 0 && adopted.stdout.includes(a.id), { stdout: adopted.stdout });
  const delivered = await device.sync(step);
  const onA = await a.events(step);
  check('the queue reaches A once', delivered.status === 0 && onA.text.split('private queue for A').length === 2, {
    stdout: delivered.stdout,
  });
  await device.sync(step);
  check('a retry adds nothing', (await a.events(step)).count === onA.count);
  check('B never saw it', !(await b.events(step)).text.includes('private queue for A'));

  const fresh = new Device('written-offline');
  await fresh.add(step, 'written offline on a new replica');
  const offline = inspect(join(fresh.dir, 'replica.sqlite'));
  check('a new replica with a queue and no cursor', offline.cursor === 0 && offline.pending === 1, offline);
  await fresh.pair(step, a);
  const first = await fresh.sync(step);
  check('goes with the first server, no adopt asked', first.status === 0
    && (await a.events(step)).text.includes('written offline on a new replica'), { stdout: first.stdout, stderr: first.stderr.slice(0, 300) });
}

async function interruptedChange() {
  const step = 'interrupted change';
  const device = new Device('interrupted');
  await device.pair(step, a);
  await device.sync(step);
  await device.add(step, 'queued before the stop');
  const aPath = await device.activePath(step);
  const onA = readFileSync(device.registry);
  const beforeB = await b.events(step);

  await device.pair(step, b);
  // A `pair` stopped between its two writes: paired.json names B, replicas.json still A.
  writeFileSync(device.registry, onA);
  const synced = await device.sync(step);
  const bPath = await device.activePath(step);
  check('the next process finishes the change', synced.status === 0 && bPath !== aPath, { stdout: synced.stdout });
  check('nothing of A went out', (await b.events(step)).count === beforeB.count);
  check('the queue of A is still there', inspect(aPath).pending === 1);

  copyFileSync(device.registry, `${device.registry}.kept`);
  writeFileSync(device.registry, '{"active":');
  const broken = await device.sync(step);
  check('a broken replicas.json stops the sync', broken.status !== 0 && broken.stderr.includes('replicas.json'), {
    stderr: broken.stderr.slice(0, 200).replaceAll(dir, ''),
  });
  check('and nothing went out', (await b.events(step)).count === beforeB.count);
  copyFileSync(`${device.registry}.kept`, device.registry);
  const restored = await device.sync(step);
  check('putting it back is enough', restored.status === 0);

  // The server of a pairing is replaced behind the same address.
  writeFileSync(device.paired, JSON.stringify({ ...JSON.parse(readFileSync(device.paired, 'utf8')), server_id: a.id }), { mode: 0o600 });
  const wrong = await device.sync(step);
  check('a pairing that names the wrong server is refused', wrong.status !== 0 && wrong.stderr.includes(`belongs to server ${a.id}`), {
    stderr: wrong.stderr.slice(0, 240),
  });
  check('and sends A its queue nowhere else', (await b.events(step)).count === beforeB.count);
}

async function negativeControl() {
  const step = 'negative control';
  const device = new Device('old-binary');
  const old = args['old-desktop'];
  await a.seed(step, 'more history of A');
  await device.pair(step, a, old);
  await device.sync(step, noEnv, old);
  await device.add(step, 'old queue for A', old);
  const cursor = inspect(join(device.dir, 'replica.sqlite')).cursor;
  await b.seed(step, 'history of B past the cursor');
  await device.pair(step, b, old);
  const synced = await device.sync(step, noEnv, old);
  const log = await b.events(step);
  const replica = inspect(join(device.dir, 'replica.sqlite'));
  check('negative control: the old binary sends the queue of A to B', log.text.includes('old queue for A'), {
    stdout: synced.stdout,
  });
  check('negative control: and misses history of B below the cursor of A', !replica.titles.includes('early history of B')
    && replica.titles.includes('history of A'), { cursor, titles: replica.titles });
}

let thrown = null;
try {
  const health = await a.start();
  await b.start();
  a.id = await a.identity('environment');
  b.id = await b.identity('environment');
  record({
    meta: 'environment',
    server_sha256: digest(args.server),
    desktop_sha256: digest(args.desktop),
    old_desktop_sha256: digest(args['old-desktop']),
    healthz_git_sha: health.git_sha,
    version: health.version,
    node: process.version,
  });
  record({ meta: 'expected_before_run', expected: EXPECTED });
  check('two servers, two identities', a.id !== b.id && a.id.length === 32 && b.id.length === 32, { a: a.id, b: b.id });
  if (args['expect-sha']) {
    check('server reports the expected SHA', health.git_sha === args['expect-sha'], { healthz_git_sha: health.git_sha });
  }
  const device = new Device('moving-device');
  const paths = await toAnotherServer(device);
  await backToTheFirstServer(device, paths);
  await overrides(device);
  await serversWithoutIdentity(device);
  await replicaFromBeforeBindings();
  await queueNoServerHasSeen();
  await interruptedChange();
  await negativeControl();
} catch (error) {
  thrown = error;
  failed += 1;
  record({ error: String(error?.message ?? error) });
} finally {
  const stops = { a: await a.stop(), b: await b.stop() };
  const logs = a.log + b.log;
  const serverErrors = logs.split('\n').filter((line) => /\bERROR\b|panicked/.test(line));
  check('server logs free of errors', serverErrors.length === 0, { error_lines: serverErrors.length, first: serverErrors[0]?.slice(0, 200) });
  record({ meta: 'cleanup', stops, server_log_lines: logs.split('\n').length });
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
