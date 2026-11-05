#!/usr/bin/env node
// Real-binary scenario for token revocation: DELETE /v1/tokens/{id} against a
// live daruma-server, driven over HTTP and WebSocket like a client would.
//
//   GIT_SHA=$(git rev-parse HEAD) cargo build -p daruma-server --bin daruma-server
//   node scripts/scenarios/token-revocation.mjs \
//     --binary "$CARGO_TARGET_DIR/debug/daruma-server" --out /tmp/trace.jsonl
//
// Writes one JSON object per line and exits 1 if any check failed. Bearers stay
// out of the trace: sockets send them as a subprotocol, and the run fails if a
// client token shows up in the server log or any token in the trace. The server
// directory is private to the run and removed at the end.
import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { setTimeout as sleep } from 'node:timers/promises';
import { parseArgs } from 'node:util';

const { values: args } = parseArgs({
  options: {
    binary: { type: 'string' },
    out: { type: 'string' },
    port: { type: 'string', default: '18931' },
    'expect-sha': { type: 'string' },
  },
});
if (!args.binary || !args.out) {
  console.error('usage: token-revocation.mjs --binary <path> --out <trace.jsonl> [--expect-sha <sha>]');
  process.exit(2);
}

const WAIT_MS = 3000;
const CAP = { projectRead: 1 << 5, projectWrite: 1 << 6, subscribeTasks: 1 << 7, taskRead: 1 << 0 };
const CAPS = CAP.projectRead | CAP.projectWrite | CAP.subscribeTasks | CAP.taskRead;

const EXPECTED = [
  'contractor: command before DELETE runs, WS and HTTP work',
  'DELETE returns 204; socket gets auth_revoked and close 1008 without any further client frame',
  'a command attempted after the 204 leaves no project; the socket is normally closed by then, so the trace states whether the frame left the client',
  'the cached HTTP bearer gets 401 auth_revoked at once',
  'reconnect with the same bearer gets close 1008 invalid token',
  'colleague token of the same agent stays connected and its command runs',
  'a revoke the hub never saw (row written by this process) ends the socket on its next frame, that frame leaves no project',
  'the event journal holds only the commands that were allowed',
];

const dir = mkdtempSync(join(tmpdir(), 'daruma-token-revocation-'));
const base = `http://127.0.0.1:${args.port}/v1`;
const wsUrl = `ws://127.0.0.1:${args.port}/v1/ws`;
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

class Socket {
  constructor(step, label, bearer) {
    this.label = label;
    this.frames = [];
    this.waiters = [];
    this.close = null;
    this.reqId = `ws_${randomUUID()}`;
    this.ws = new WebSocket(wsUrl, {
      protocols: ['daruma.v1', `bearer.${bearer}`],
      headers: { 'x-request-id': this.reqId },
    });
    record({ step, ws: 'connect', socket: label, req_id: this.reqId });
    this.ws.addEventListener('message', (event) => {
      const frame = JSON.parse(event.data);
      this.frames.push(frame);
      record({
        ws: 'recv',
        socket: label,
        type: frame.type,
        code: frame.code,
        event_kind: frame.event?.payload?.type ?? frame.event?.type,
        seq: frame.event?.seq ?? frame.seq,
      });
      this.wake();
    });
    this.ws.addEventListener('close', (event) => {
      this.close = { code: event.code, reason: event.reason };
      record({ ws: 'close', socket: label, code: event.code, reason: event.reason });
      this.wake();
    });
    this.ws.addEventListener('error', () => record({ ws: 'error', socket: label }));
  }

  wake() {
    for (const waiter of this.waiters.splice(0)) waiter();
  }

  async until(predicate) {
    const deadline = Date.now() + WAIT_MS;
    for (;;) {
      const found = predicate(this);
      if (found) return found;
      const left = deadline - Date.now();
      if (left <= 0) return null;
      await Promise.race([
        new Promise((resolve) => this.waiters.push(resolve)),
        sleep(left),
      ]);
    }
  }

  frame(type, from = 0) {
    return this.until((socket) => socket.frames.slice(from).find((frame) => frame.type === type));
  }

  closed() {
    return this.until((socket) => socket.close);
  }

  send(step, frame) {
    const open = this.ws.readyState === WebSocket.OPEN;
    record({ step, ws: 'send', socket: this.label, type: frame.type, title: frame.command?.title, socket_open: open });
    if (open) this.ws.send(JSON.stringify(frame));
    return open;
  }

  createProject(step, title) {
    return this.send(step, {
      type: 'dispatch',
      client_event_id: randomUUID(),
      command: { type: 'create_project', title },
    });
  }

  async subscribed(step) {
    const hello = await this.frame('hello');
    this.send(step, { type: 'subscribe', since_seq: 0 });
    const snapshot = await this.frame('snapshot');
    return Boolean(hello && snapshot);
  }
}

async function mint(label, admin, agentId) {
  const { status, json } = await http('setup', 'POST', '/tokens', admin, {
    kind: 'bot',
    agent_id: agentId,
    capabilities: CAPS,
    rate_limit_per_min: 600,
  });
  if (status !== 201) throw new Error(`mint: ${status}`);
  secrets.set(json.secret, label);
  return { secret: json.secret, id: json.token.id };
}

async function waitHealthy(child) {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(`server exited: ${child.exitCode}`);
    try {
      const response = await fetch(`${base}/healthz`);
      if (response.ok) return response.json();
    } catch {
      await sleep(100);
    }
  }
  throw new Error('server never answered /healthz');
}

async function projectTitles(admin) {
  const { json } = await http('journal', 'GET', '/events?since=0&limit=1000', admin);
  const events = json?.events ?? json?.items ?? json ?? [];
  return events
    .map((event) => event.payload ?? event)
    .filter((payload) => (payload.type ?? payload.kind) === 'project_created')
    .map((payload) => payload.title ?? payload.project?.title);
}

async function scenario(admin, dbPath) {
  const agent = randomUUID();
  const contractor = await mint('contractor', admin, agent);
  const colleague = await mint('colleague', admin, agent);
  const outOfBand = await mint('out_of_band', admin, agent);

  const a = new Socket('connect', 'contractor', contractor.secret);
  const b = new Socket('connect', 'colleague', colleague.secret);
  const c = new Socket('connect', 'out_of_band', outOfBand.secret);
  check('all three sockets reach snapshot', (await a.subscribed('connect')) && (await b.subscribed('connect')) && (await c.subscribed('connect')));

  a.createProject('before', 'contractor before delete');
  check('contractor command before delete is acked', Boolean(await a.frame('ack')));
  const cached = await http('before', 'GET', '/projects', contractor.secret);
  check('contractor HTTP works before delete', cached.status === 200, { status: cached.status });

  const framesBefore = a.frames.length;
  const deleted = await http('revoke', 'DELETE', `/tokens/${contractor.id}`, admin);
  check('DELETE returns 204', deleted.status === 204, { status: deleted.status });

  const sent = a.createProject('after', 'contractor after delete');
  record({ step: 'after', note: sent ? 'frame sent after 204' : 'socket already closed, nothing to send' });
  const closed = await a.closed();
  check('contractor socket closed 1008', closed?.code === 1008, { close: closed });
  const late = a.frames.slice(framesBefore);
  check('only auth_revoked follows the delete', late.length === 1 && late[0].code === 'auth_revoked', {
    frames: late.map((frame) => frame.code ?? frame.type),
  });

  const stale = await http('after', 'GET', '/projects', contractor.secret);
  check('cached HTTP bearer refused with auth_revoked', stale.status === 401 && JSON.stringify(stale.json).includes('auth_revoked'), {
    status: stale.status,
  });

  const again = new Socket('reconnect', 'contractor_again', contractor.secret);
  const refused = await again.closed();
  check('reconnect refused', refused?.code === 1008 && !again.frames.some((frame) => frame.type === 'hello'), { close: refused });
  check('reconnect names the reason', refused?.reason === 'invalid token', { reason: refused?.reason });

  const ackedBefore = b.frames.length;
  b.createProject('neighbour', 'colleague after delete');
  check('colleague token still works', Boolean(await b.frame('ack', ackedBefore)));
  check('colleague socket stayed open', b.ws.readyState === WebSocket.OPEN);

  const db = new DatabaseSync(dbPath);
  const row = db
    .prepare("UPDATE tokens SET revoked_at = ? WHERE id LIKE '%' || ? AND revoked_at IS NULL")
    .run(new Date().toISOString(), outOfBand.id);
  db.close();
  check('row revoke hit exactly one token', row.changes === 1, { changes: row.changes });
  c.createProject('out_of_band', 'written to the row only');
  const cClosed = await c.closed();
  check('out-of-band revoke ends the socket on its next frame', cClosed?.code === 1008, { close: cClosed });

  const titles = await projectTitles(admin);
  const allowed = ['contractor before delete', 'colleague after delete'];
  check(
    'journal holds exactly the allowed commands',
    JSON.stringify([...titles].sort()) === JSON.stringify([...allowed].sort()),
    { titles },
  );
  const listed = await http('journal', 'GET', '/projects', admin);
  const listedTitles = (listed.json?.projects ?? listed.json?.items ?? listed.json ?? []).map((project) => project.title);
  check('forbidden projects absent from the projection', !listedTitles.includes('contractor after delete') && !listedTitles.includes('written to the row only'), {
    projects: listedTitles,
  });

  for (const socket of [b, c, a, again]) socket.ws.close();
}

const env = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => !/^(https?|no|all)_proxy$/i.test(key)),
);
const server = spawn(args.binary, [], {
  env: {
    ...env,
    DARUMA_AGENT_DIR: dir,
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

let thrown = null;
try {
  const health = await waitHealthy(server);
  record({
    meta: 'build',
    binary_sha256: createHash('sha256').update(readFileSync(args.binary)).digest('hex'),
    healthz_git_sha: health.git_sha,
    version: health.version,
    pid: server.pid,
    node: process.version,
  });
  record({ meta: 'expected_before_run', expected: EXPECTED });
  if (args['expect-sha']) {
    check('server reports the expected SHA', health.git_sha === args['expect-sha'], { healthz_git_sha: health.git_sha });
  }
  const admin = readFileSync(join(dir, 'data', 'bootstrap.token'), 'utf8').trim();
  secrets.set(admin, 'bootstrap');
  await scenario(admin, join(dir, 'data', 'daruma.sqlite'));
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
  // The first boot prints the bootstrap token once on purpose, see main.rs.
  const mentions = [];
  for (const line of log.split('\n')) {
    for (const [secret, label] of secrets) {
      if (line.includes(secret)) mentions.push({ label, line: line.replaceAll(secret, '***').slice(0, 160) });
    }
  }
  record({ meta: 'bootstrap_banner', lines: mentions.filter((mention) => mention.label === 'bootstrap').length });
  const leaks = mentions.filter((mention) => mention.label !== 'bootstrap');
  check('no client bearer in server log', leaks.length === 0, { leaks });
  record({ meta: 'cleanup', server_stop: stop, server_log_lines: log.split('\n').length });
  rmSync(dir, { recursive: true, force: true });
  check('server directory removed', !existsSync(dir));
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
