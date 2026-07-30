#!/usr/bin/env node

const args = process.argv.slice(2);
const positionalArgs = args.filter((arg) => !arg.startsWith('-'));

function readOption(name, fallback) {
  const index = args.indexOf(name);
  if (index >= 0 && args[index + 1]) {
    return args[index + 1];
  }
  return fallback;
}

const baseUrl = readOption(
  '--base-url',
  process.env.AETHERSENSE_DASHBOARD_URL ?? positionalArgs[0] ?? 'http://127.0.0.1:3000',
).replace(/\/$/, '');
const timeoutMs = Number.parseInt(
  readOption('--timeout-ms', process.env.AETHERSENSE_DASHBOARD_TIMEOUT_MS ?? '5000'),
  10,
);
const requireLive = args.includes('--require-live') || process.env.AETHERSENSE_REQUIRE_LIVE === '1';
const apiOnly = args.includes('--api-only');

function withAbort(timeoutLabel) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(new Error(`${timeoutLabel} timed out after ${timeoutMs}ms`)), timeoutMs);
  return { controller, timer };
}

async function fetchJson(pathname) {
  const { controller, timer } = withAbort(pathname);
  try {
    const response = await fetch(`${baseUrl}${pathname}`, { signal: controller.signal });
    if (!response.ok) {
      throw new Error(`${pathname} returned HTTP ${response.status}`);
    }
    return await response.json();
  } finally {
    clearTimeout(timer);
  }
}

function dashboardWebSocketUrl() {
  const url = new URL(baseUrl);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.pathname = '/ws';
  url.search = '?once=1';
  return url.toString();
}

function readWebSocketSnapshot() {
  if (typeof WebSocket === 'undefined') {
    throw new Error('Node.js runtime does not expose WebSocket; use Node 24+ for this healthcheck');
  }

  const wsUrl = dashboardWebSocketUrl();

  return new Promise((resolve, reject) => {
    let settled = false;
    let snapshot;
    let closeGraceTimer;
    const socket = new WebSocket(wsUrl);
    const finish = (callback, value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (closeGraceTimer) clearTimeout(closeGraceTimer);
      callback(value);
    };
    const timer = setTimeout(() => {
      socket.close();
      finish(reject, new Error(`${wsUrl} did not send a snapshot within ${timeoutMs}ms`));
    }, timeoutMs);

    socket.addEventListener('message', (event) => {
      if (settled) return;

      try {
        snapshot = JSON.parse(event.data);
        closeGraceTimer = setTimeout(() => {
          socket.close(1000, 'healthcheck complete');
          finish(resolve, snapshot);
        }, 750);
      } catch (error) {
        finish(reject, new Error(`Invalid JSON from ${wsUrl}: ${error instanceof Error ? error.message : String(error)}`));
      }
    });

    socket.addEventListener('error', () => {
      finish(reject, new Error(`Could not open ${wsUrl}`));
    });

    socket.addEventListener('close', (event) => {
      if (snapshot) {
        finish(resolve, snapshot);
        return;
      }
      finish(reject, new Error(`${wsUrl} closed before first snapshot (code ${event.code})`));
    });
  });
}

function validateSnapshot(snapshot) {
  if (!snapshot || typeof snapshot !== 'object') {
    throw new Error('WebSocket snapshot is not an object');
  }

  if (!Number.isFinite(snapshot.count)) {
    throw new Error('WebSocket snapshot is missing numeric count');
  }

  if (!snapshot.stream || typeof snapshot.stream.source !== 'string') {
    throw new Error('WebSocket snapshot is missing stream.source');
  }

  if (requireLive && (snapshot.count <= 0 || snapshot.stream.source !== 'udp_live')) {
    throw new Error(`Live UDP data required, got count=${snapshot.count} source=${snapshot.stream.source}`);
  }
}

try {
  const health = await fetchJson('/api/health');
  if (health.ok !== true) {
    throw new Error('/api/health did not return ok=true');
  }

  if (apiOnly) {
    console.log(JSON.stringify({
      ok: true,
      baseUrl,
      apiOnly,
    }));
    process.exit(0);
  }

  const snapshot = await readWebSocketSnapshot();
  validateSnapshot(snapshot);

  console.log(JSON.stringify({
    ok: true,
    baseUrl,
    count: snapshot.count,
    stream: snapshot.stream.source,
    devices: Array.isArray(snapshot.devices) ? snapshot.devices.length : 0,
    requireLive,
  }));
  process.exit(0);
} catch (error) {
  console.error(`[dashboard-healthcheck] ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
