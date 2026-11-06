#!/usr/bin/env node
// Real-binary scenario for the trust a desktop keeps after pairing: daruma-desktop
// pairs with a live daruma-server on its self-signed certificate, then syncs from
// new processes, meets a swapped and a rotated certificate, an address override,
// a paired.json from before fingerprints and a server the system already trusts.
//
//   GIT_SHA=$(git rev-parse HEAD) cargo build -p daruma-server -p daruma-desktop
//   node scripts/scenarios/desktop-pairing-tls-trust.mjs \
//     --server "$CARGO_TARGET_DIR/debug/daruma-server" \
//     --desktop "$CARGO_TARGET_DIR/debug/daruma-desktop" \
//     --old-desktop /path/to/daruma-desktop-before-the-fix --out /tmp/trace.jsonl
//
// Writes one JSON object per line and exits 1 if any check failed. Passing the
// old binary as --desktop is the negative control.
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:https';
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
    port: { type: 'string', default: '18973' },
    'expect-sha': { type: 'string' },
  },
});
if (!args.server || !args.desktop || !args['old-desktop'] || !args.out) {
  console.error(
    'usage: desktop-pairing-tls-trust.mjs --server <path> --desktop <path> --old-desktop <path> --out <trace.jsonl>',
  );
  process.exit(2);
}

const EXPECTED = [
  'clean trust store: pair, then sync pull and push and devices from new processes, also after a server restart',
  'swapped certificate: sync and devices refused with both fingerprints, the impostor decrypts no request, work stays queued',
  'rotated certificate: refused until a fresh ticket re-pairs; a failed pair leaves the pin; queued work then arrives',
  'override: plain URL keeps working, the same server under another host name gets no pin',
  'paired.json without fingerprint: refused with the way out, the impostor gets nothing, a fresh pair adds the pin',
  'system-trusted server: old paired.json and explicit override keep working',
  'old binary: pair succeeds, its next sync fails on the same server (the defect)',
];

const dir = mkdtempSync(join(tmpdir(), 'daruma-pairing-tls-'));
const httpPort = Number(args.port);
const tlsPort = httpPort + 1;
const trustedHttpPort = httpPort + 2;
const trustedTlsPort = httpPort + 3;
const impostorPort = httpPort + 4;
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
  constructor(name, hostname, port, tls) {
    Object.assign(this, { name, hostname, port, tls, home: join(dir, name), log: '' });
    this.base = `http://127.0.0.1:${port}/v1`;
  }

  async start() {
    this.child = spawn(args.server, [], {
      env: {
        ...proxyFree,
        DARUMA_AGENT_DIR: this.home,
        DARUMA_HOSTNAME: this.hostname,
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
      const health = await fetch(`${this.base}/healthz`).then((r) => (r.ok ? r.json() : null)).catch(() => null);
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

  file(name) {
    return join(this.home, 'data', 'tls', name);
  }

  async api(step, method, path, body) {
    const response = await fetch(`${this.base}${path}`, {
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

  async ticket(step) {
    const { status, json } = await this.api(step, 'GET', '/devices/pair/ticket');
    if (status !== 200) throw new Error(`ticket: ${status}`);
    const token = new URLSearchParams(json.pairing_url.split('?')[1]).get('token');
    secrets.set(token, 'pairing ticket');
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

  async hasTask(step, title) {
    const { json } = await this.api(step, 'GET', '/events?since=0&limit=1000');
    return JSON.stringify(json).includes(title);
  }
}

class Device {
  constructor(name) {
    this.name = name;
    this.dir = join(dir, name);
    this.paired = join(this.dir, 'paired.json');
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
          stdout: result.stdout.slice(0, 300),
          stderr: result.stderr.slice(0, 700),
        });
        resolve(result);
      });
    });
  }

  pairing() {
    return JSON.parse(readFileSync(this.paired, 'utf8'));
  }

  writePairing(serverUrl, token) {
    writeFileSync(this.paired, JSON.stringify({ server_url: serverUrl, token }), { mode: 0o600 });
    secrets.set(token, 'legacy pairing');
  }

  pending() {
    const db = new DatabaseSync(join(this.dir, 'replica.sqlite'), { readOnly: true });
    try {
      return db.prepare('SELECT COUNT(*) AS n FROM desktop_outbox').get().n;
    } finally {
      db.close();
    }
  }

  titles() {
    const db = new DatabaseSync(join(this.dir, 'replica.sqlite'), { readOnly: true });
    try {
      return db.prepare('SELECT title FROM tasks').all().map((row) => row.title);
    } finally {
      db.close();
    }
  }
}

/** A TLS listener under another certificate, recording what it decrypts. */
async function impostor(cert, key, port) {
  const seen = { requests: [], refusedHandshakes: 0 };
  const server = createServer({ cert, key }, (request, response) => {
    seen.requests.push({ method: request.method, url: request.url, bearer: Boolean(request.headers.authorization) });
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('[]');
  });
  server.on('tlsClientError', () => { seen.refusedHandshakes += 1; });
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

const main = new Server('server', '127.0.0.1', httpPort, tlsPort);
const trusted = new Server('trusted-server', 'localhost', trustedHttpPort, trustedTlsPort);

async function cleanTrustStore() {
  const device = new Device('paired');
  const ticket = await main.ticket('pair');
  const paired = await device.run('pair', args.desktop, ['pair', ticket.pairing_url]);
  check('pair succeeds on the self-signed certificate', paired.status === 0);
  const stored = device.pairing();
  secrets.set(stored.token, 'device token');
  check('paired.json keeps the fingerprint the ticket advertised', stored.tls_fingerprint === ticket.tls_fingerprint, {
    tls_fingerprint: stored.tls_fingerprint,
  });
  check('paired.json names the port pairing used', stored.server_url === `https://127.0.0.1:${tlsPort}`, {
    server_url: stored.server_url,
  });
  check('paired.json stays owner-only', (statSync(device.paired).mode & 0o777) === 0o600);

  await main.seed('seed', 'seeded on the server');
  const pull = await device.run('first sync', args.desktop, ['sync', '--limit', '1000']);
  check('sync pulls from a new process without any CA setup', pull.status === 0 && device.titles().includes('seeded on the server'));

  await device.run('local change', args.desktop, ['add', 'written on the device']);
  const push = await device.run('push', args.desktop, ['sync', '--limit', '1000']);
  check('sync pushes over the pinned connection', push.status === 0 && (await main.hasTask('push', 'written on the device')));

  const devices = await device.run('devices', args.desktop, ['devices']);
  check('devices works over the pinned connection', devices.status === 0);

  await main.stop();
  await main.start();
  const restarted = await device.run('after server restart', args.desktop, ['sync', '--limit', '1000']);
  check('a server restart keeps its certificate and the pin holds', restarted.status === 0);
  return device;
}

async function swappedCertificate(device) {
  const stored = device.pairing();
  await device.run('local change behind the swap', args.desktop, ['add', 'waiting out the swap']);
  const queued = device.pending();
  await main.stop();
  const fake = await impostor(
    readFileSync(trusted.file('cert.pem')),
    readFileSync(trusted.file('key.pem')),
    tlsPort,
  );
  const sync = await device.run('swapped sync', args.desktop, ['sync', '--limit', '1000']);
  const devices = await device.run('swapped devices', args.desktop, ['devices']);
  await fake.close();
  await main.start();
  const expected = `expected ${stored.tls_fingerprint}`;
  const got = `got ${trusted.fingerprint}`;
  for (const [name, result] of [['sync', sync], ['devices', devices]]) {
    check(`${name} is refused with both fingerprints`, result.status !== 0
      && result.stderr.includes('TLS fingerprint mismatch')
      && result.stderr.includes(expected)
      && result.stderr.includes(got));
    check(`${name} says what to do about it`, result.stderr.includes('`daruma pair`'));
  }
  check('the impostor decrypted no request and saw no token', fake.seen.requests.length === 0, {
    requests: fake.seen.requests.length,
    refused_handshakes: fake.seen.refusedHandshakes,
  });
  check('the client reached the impostor and the handshake died there', fake.seen.refusedHandshakes >= 2, {
    refused_handshakes: fake.seen.refusedHandshakes,
  });
  check('queued work survives the refusal', device.pending() === queued, { pending: device.pending() });
}

async function rotatedCertificate(device) {
  const oldTicketFingerprint = device.pairing().tls_fingerprint;
  await main.stop();
  rmSync(join(main.home, 'data', 'tls'), { recursive: true });
  await main.start();
  const refused = await device.run('rotated sync', args.desktop, ['sync', '--limit', '1000']);
  check('a rotated certificate is not trusted silently', refused.status !== 0 && refused.stderr.includes('TLS fingerprint mismatch'));

  const before = readFileSync(device.paired);
  const ticket = await main.ticket('rotate');
  const stale = ticket.pairing_url.replace(/fpr=[^&]+/, `fpr=${encodeURIComponent(oldTicketFingerprint)}`);
  const failedPair = await device.run('re-pair under the old fingerprint', args.desktop, ['pair', stale]);
  check('re-pairing under the old fingerprint fails', failedPair.status !== 0);
  check('a failed pair leaves paired.json alone', before.equals(readFileSync(device.paired)));

  const repaired = await device.run('re-pair', args.desktop, ['pair', ticket.pairing_url]);
  const stored = device.pairing();
  secrets.set(stored.token, 'device token');
  check('a fresh ticket moves the pin', repaired.status === 0
    && stored.tls_fingerprint === ticket.tls_fingerprint
    && stored.tls_fingerprint !== oldTicketFingerprint);
  const sync = await device.run('sync after re-pair', args.desktop, ['sync', '--limit', '1000']);
  check('queued work arrives after re-pairing', sync.status === 0 && (await main.hasTask('re-pair', 'waiting out the swap')));
}

async function overrides(device) {
  const plain = await device.run('plain override', args.desktop, ['sync', '--limit', '1000'], {
    DARUMA_API_URL: `http://127.0.0.1:${httpPort}`,
  });
  check('an explicit plain URL keeps working next to a pin', plain.status === 0);

  const moved = await device.run('another host name', args.desktop, ['sync', '--limit', '1000'], {
    DARUMA_API_URL: `https://localhost:${tlsPort}`,
  });
  check('the pin does not follow another host name to the same server', moved.status !== 0
    && moved.stderr.includes('TLS refused')
    && !moved.stderr.includes('TLS fingerprint mismatch'));
}

async function pairedBeforeFingerprints() {
  const device = new Device('legacy');
  device.writePairing(`https://127.0.0.1:${tlsPort}`, main.admin);
  const refused = await device.run('legacy sync', args.desktop, ['sync', '--limit', '1000']);
  check('an unpinned pairing does not accept the self-signed server', refused.status !== 0 && refused.stderr.includes('TLS refused'));
  check('the refusal names the way out', refused.stderr.includes('`daruma pair`') && refused.stderr.includes('no stored certificate fingerprint'));

  const fake = await impostor(readFileSync(trusted.file('cert.pem')), readFileSync(trusted.file('key.pem')), impostorPort);
  device.writePairing(`https://127.0.0.1:${impostorPort}`, 'legacy-token-for-the-impostor');
  await device.run('legacy sync at an impostor', args.desktop, ['sync', '--limit', '1000']);
  await fake.close();
  check('an unpinned pairing sends no token to an untrusted server', fake.seen.requests.length === 0, {
    requests: fake.seen.requests.length,
    refused_handshakes: fake.seen.refusedHandshakes,
  });

  const ticket = await main.ticket('legacy re-pair');
  const repaired = await device.run('legacy re-pair', args.desktop, ['pair', ticket.pairing_url]);
  secrets.set(device.pairing().token, 'device token');
  const sync = await device.run('legacy sync after re-pair', args.desktop, ['sync', '--limit', '1000']);
  check('re-pairing migrates it to a pin', repaired.status === 0 && sync.status === 0 && device.pairing().tls_fingerprint === ticket.tls_fingerprint);
}

async function systemTrustedServer() {
  const trust = { SSL_CERT_FILE: trusted.file('cert.pem') };
  const url = `https://localhost:${trustedTlsPort}`;

  const legacy = new Device('legacy-trusted');
  legacy.writePairing(url, trusted.admin);
  const sync = await legacy.run('legacy sync, system trust', args.desktop, ['sync', '--limit', '1000'], trust);
  check('an unpinned pairing keeps working where the system trusts the server', sync.status === 0);

  const explicit = new Device('override-trusted');
  const overridden = await explicit.run('override, system trust', args.desktop, ['sync', '--limit', '1000'], {
    ...trust,
    DARUMA_API_URL: url,
    DARUMA_TOKEN: trusted.admin,
  });
  check('an explicit override keeps working where the system trusts the server', overridden.status === 0);

  const untrusted = await explicit.run('override, no system trust', args.desktop, ['sync', '--limit', '1000'], {
    DARUMA_API_URL: url,
    DARUMA_TOKEN: trusted.admin,
  });
  check('the same override without system trust is refused', untrusted.status !== 0);
}

async function negativeControl() {
  const device = new Device('old-binary');
  const ticket = await main.ticket('old pair');
  const paired = await device.run('old pair', args['old-desktop'], ['pair', ticket.pairing_url]);
  secrets.set(device.pairing().token, 'device token');
  const sync = await device.run('old sync', args['old-desktop'], ['sync', '--limit', '1000']);
  check('negative control: the old binary pairs, then cannot sync', paired.status === 0
    && sync.status !== 0
    && device.pairing().tls_fingerprint === undefined, { stderr: sync.stderr.slice(0, 200) });
}

let thrown = null;
try {
  const health = await main.start();
  await trusted.start();
  trusted.fingerprint = (await trusted.ticket('fingerprint of the other certificate')).tls_fingerprint;
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
  if (args['expect-sha']) {
    check('server reports the expected SHA', health.git_sha === args['expect-sha'], { healthz_git_sha: health.git_sha });
  }
  const device = await cleanTrustStore();
  await swappedCertificate(device);
  await rotatedCertificate(device);
  await overrides(device);
  await pairedBeforeFingerprints();
  await systemTrustedServer();
  await negativeControl();
} catch (error) {
  thrown = error;
  failed += 1;
  record({ error: String(error?.message ?? error) });
} finally {
  const stops = { server: await main.stop(), trusted: await trusted.stop() };
  const logs = main.log + trusted.log;
  const serverErrors = logs.split('\n').filter((line) => /\bERROR\b|panicked/.test(line));
  check('server logs free of errors', serverErrors.length === 0, { error_lines: serverErrors.length });
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
