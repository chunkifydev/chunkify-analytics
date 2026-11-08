// Several analytics servers sharing one ClickHouse database.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { buildDemoEvents } from "../../examples/dashboard/seed-events.mjs";
import { clickhouseQuery, clickhouseUrl, collectorEnvironment, writeConfig } from "./configuration.mjs";

const binary = process.env.CHUNKIFY_BINARY ?? resolve("target/debug/chunkify-analytics");

async function run(args, directory, env) {
  const child = spawn(binary, args, { cwd: directory, env: collectorEnvironment(env), stdio: "pipe" });
  let stdout = "", stderr = "";
  child.stdout.on("data", chunk => { stdout += chunk; });
  child.stderr.on("data", chunk => { stderr += chunk; });
  const [code] = await once(child, "close");
  return { code, stdout, stderr };
}

// Start a server with its own data directory. `storage` selects the backend.
async function collector(t, storage, { init = true, settings = {} } = {}) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-clickhouse-test-"));
  const env = { ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("CHUNKIFY_"))),
    "rate_limits.read_per_minute": "1000000", "rate_limits.read_burst": "1000000",
    "rate_limits.read_concurrent_per_project": "32", "rate_limits.read_concurrent": "32",
    "rate_limits.export_downloads_per_minute": "1000", "server.ingest_per_second": "100000",
    "data_dir": directory, "server.bind": "127.0.0.1:0", "server.trusted_proxies": "127.0.0.1",
    "server.allowed_origins": "https://player.example", "retention.seconds": "0", ...storage, ...settings };
  await writeConfig(directory, env);
  let keys;
  if (init) {
    const result = await run(["init"], directory, env);
    assert.equal(result.code, 0, result.stderr);
    keys = JSON.parse(result.stdout);
  }
  const child = spawn(binary, ["serve"], { cwd: directory, env: collectorEnvironment(env), stdio: "pipe" });
  const closed = once(child, "close");
  let logs = "";
  child.stderr.on("data", chunk => { logs += chunk; });
  const url = await new Promise((ready, reject) => {
    const timeout = setTimeout(() => reject(new Error(`Collector startup timed out: ${logs}`)), 15000);
    child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Collector exited: ${logs}`)); });
    child.stdout.on("data", chunk => {
      logs += chunk;
      const match = logs.match(/Listening on (http:\/\/\S+)/);
      if (match) { clearTimeout(timeout); ready(match[1]); }
    });
  });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    await closed;
    await rm(directory, { recursive: true, force: true });
  });
  const server = {
    url, keys, directory, env,
    command: args => run(args, directory, env),
    post(event, headers = {}, key = server.keys.public_key) {
      return fetch(`${url}/v1/events`, { method: "POST", body: JSON.stringify(event),
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": key, ...headers } });
    },
    async get(path, token = server.keys.secret_key) {
      const response = await fetch(`${url}${path}`, { headers: { Authorization: `Bearer ${token}` } });
      const body = await response.text();
      assert.equal(response.status, 200, `${path}: ${body}`);
      return JSON.parse(body);
    },
  };
  return server;
}

function database(name) {
  return `chunkify_test_${name}_${Math.random().toString(36).slice(2, 10)}`;
}


// Demo playback, plus location, page, watched-range and error-code variety.
function dataset() {
  const addresses = ["81.2.69.160", "89.160.20.112", "216.160.83.56", "175.16.199.0", "127.0.0.2", null];
  const events = buildDemoEvents(new Date("2026-11-01T12:00:00Z"));
  const views = [...new Set(events.map(event => event.view_id))];
  return events.map((event, index) => {
    const view = views.indexOf(event.view_id);
    const next = { ...event, browser_context: { ...event.browser_context,
      page_origin: view % 4 === 3 ? null : `https://site-${view % 3}.example`, page_path: `/watch/${view % 5}` } };
    if (event.event_name !== "chunkify.video.view_started") {
      const end = Math.min(600, Math.max(1, (event.playing_time_ms ?? 0) / 1000));
      next.played_ranges = view % 6 === 5 ? [] : [{ start_seconds: 0, end_seconds: end / 2 },
        { start_seconds: Math.min(599, end / 2 + 10), end_seconds: Math.min(600, end + 10) }];
    }
    if (event.event_name === "chunkify.video.playback_error") {
      next.error_code = view % 2 ? 4012 : "DEMO_NETWORK_FAILURE";
      next.error_fatal = view % 3 !== 0;
    }
    return { event: next, address: addresses[(view + index) % addresses.length], duplicate: index % 17 === 0 };
  });
}

// Receipt times differ between servers.
function comparable(value) {
  if (Array.isArray(value)) return value.map(comparable);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).filter(([key]) => key !== "received_at" && key !== "next_cursor")
      .map(([key, item]) => [key, comparable(item)]));
  }
  return value;
}

test("servers sharing one ClickHouse database serve the same projects and events", async t => {
  const adminKey = `admin-${Math.random()}`;
  const storage = { "clickhouse.url": clickhouseUrl,
    "clickhouse.database": database("shared"), "clickhouse.registry_refresh_seconds": "1",
    CHUNKIFY_ADMIN_KEY: adminKey };
  const first = await collector(t, { ...storage, "clickhouse.node_id": "1" });
  const second = await collector(t, { ...storage, "clickhouse.node_id": "2" }, { init: false });
  second.keys = first.keys;
  const fresh = await mkdtemp(join(tmpdir(), "chunkify-clickhouse-init-"));
  t.after(() => rm(fresh, { recursive: true, force: true }));
  const freshEnv = { ...second.env, data_dir: fresh };
  await writeConfig(fresh, freshEnv);
  const again = await run(["init"], fresh, freshEnv);
  assert.notEqual(again.code, 0, "a shared database is initialized once");
  assert.match(again.stderr, /already initialized/);

  // Ingestion through either server is visible to both, in one event order.
  const records = dataset().slice(0, 60);
  for (const [index, { event }] of records.entries()) {
    const response = await (index % 2 ? second : first).post(event);
    assert.equal(response.status, 201, await response.text());
  }
  assert.equal((await first.post(records[1].event)).status, 200, "a retry through another server is a duplicate");
  const [a, b] = await Promise.all([first, second].map(server => server.get("/v1/events?limit=1000")));
  assert.equal(a.events.length, 60);
  assert.deepEqual(comparable(a), comparable(b));
  assert.deepEqual(await first.get("/v1/summary"), await second.get("/v1/summary"));

  // A project created through one server works immediately through the other.
  const created = await fetch(`${first.url}/v1/projects`, { method: "POST", body: JSON.stringify({ name: "Shared tenant", retention_seconds: 0 }),
    headers: { Authorization: `Bearer ${adminKey}`, "Content-Type": "application/json" } });
  assert.equal(created.status, 201, await created.clone().text());
  const project = await created.json();
  assert.equal((await second.post(records[0].event, {}, project.public_key)).status, 201);
  assert.equal((await second.get("/v1/events", project.secret_key)).events.length, 1);
  assert.equal((await first.get("/v1/events", project.secret_key)).events.length, 1);
  assert.equal((await first.get("/v1/events")).events.length, 60, "projects stay isolated");

  // Revocation through one server reaches the other at its next refresh.
  const keys = await (await fetch(`${second.url}/v1/project/keys`, { headers: { Authorization: `Bearer ${project.secret_key}` } })).json();
  const publicKey = keys.keys.find(key => key.permission === "public");
  const rotated = await fetch(`${second.url}/v1/project/keys/${publicKey.id}/rotate`, { method: "POST",
    headers: { Authorization: `Bearer ${project.secret_key}` } });
  assert.equal(rotated.status, 200);
  const deadline = Date.now() + 5000;
  let status;
  do {
    status = (await first.post({ ...records[1].event, event_id: crypto.randomUUID() }, {}, project.public_key)).status;
    if (status !== 401) await delay(200);
  } while (status !== 401 && Date.now() < deadline);
  assert.equal(status, 401, "the old public key stops working on every server");

  // The shared registry stores credential hashes, never the keys themselves.
  const registry = await clickhouseQuery(`SELECT document FROM ${storage["clickhouse.database"]}.registry FORMAT TSVRaw`);
  for (const secret of [first.keys.public_key, first.keys.secret_key, project.public_key, project.secret_key]) {
    assert.ok(!registry.includes(secret));
  }
  await clickhouseQuery(`DROP DATABASE IF EXISTS ${storage["clickhouse.database"]}`);
});
