// ClickHouse storage checks. Set CHUNKIFY_TEST_CLICKHOUSE_URL to a disposable
// ClickHouse HTTP endpoint, for example http://127.0.0.1:8123, to run them.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { buildDemoEvents } from "../../examples/dashboard/seed-events.mjs";
import { collectorEnvironment, writeConfig } from "./configuration.mjs";

const binary = process.env.CHUNKIFY_BINARY ?? resolve("target/debug/chunkify-analytics");
const clickhouse = process.env.CHUNKIFY_TEST_CLICKHOUSE_URL;
const skip = clickhouse ? false : "set CHUNKIFY_TEST_CLICKHOUSE_URL to run ClickHouse storage tests";

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

async function sql(query) {
  const response = await fetch(clickhouse, { method: "POST", body: query });
  assert.equal(response.status, 200, await response.clone().text());
  return response.text();
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

async function ingest(server, records) {
  for (const { event, address, duplicate } of records) {
    const headers = address ? { "X-Forwarded-For": address } : {};
    for (const attempt of duplicate ? [1, 2] : [1]) {
      const response = await server.post(event, headers);
      assert.equal(response.status, attempt === 1 ? 201 : 200, await response.text());
    }
  }
}

// Receipt times differ between installations; floating sums may differ in the last bits.
function comparable(value) {
  if (Array.isArray(value)) return value.map(comparable);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).filter(([key]) => key !== "received_at" && key !== "next_cursor")
      .map(([key, item]) => [key, comparable(item)]));
  }
  return value;
}

function assertClose(actual, expected, path = "$") {
  if (typeof expected === "number" && typeof actual === "number") {
    const tolerance = 1e-9 * Math.max(1, Math.abs(expected));
    assert.ok(Math.abs(actual - expected) <= tolerance, `${path}: ${actual} != ${expected}`);
    return;
  }
  if (Array.isArray(expected)) {
    assert.ok(Array.isArray(actual), `${path}: expected an array`);
    assert.equal(actual.length, expected.length, `${path}: length`);
    expected.forEach((item, index) => assertClose(actual[index], item, `${path}[${index}]`));
    return;
  }
  if (expected && typeof expected === "object") {
    assert.deepEqual(Object.keys(actual).sort(), Object.keys(expected).sort(), `${path}: keys`);
    for (const key of Object.keys(expected)) assertClose(actual[key], expected[key], `${path}.${key}`);
    return;
  }
  assert.deepEqual(actual, expected, path);
}

// Follow cursors independently on each server and compare every page.
async function pages(server, path) {
  const result = [];
  let cursor = null;
  for (let page = 0; page < 50; page++) {
    const separator = path.includes("?") ? "&" : "?";
    const body = await server.get(cursor ? `${path}${separator}cursor=${encodeURIComponent(cursor)}` : path);
    result.push(body);
    cursor = body.next_cursor;
    if (!cursor) break;
  }
  return result;
}

const REPORTS = [
  "/v1/summary", "/v1/quality", "/v1/engagement", "/v1/renditions", "/v1/renditions?limit=2",
  "/v1/summary?browser_name=Chrome", "/v1/quality?device_type=mobile", "/v1/engagement?country=GB",
  "/v1/summary?from=2026-10-28T00:00:00Z&to=2026-10-30T00:00:00Z", "/v1/quality?page_origin=https%3A%2F%2Fsite-1.example",
  "/v1/retention?video_id=product-tour", "/v1/retention?video_id=launch-film&browser_name=Safari",
  "/v1/timeseries?from=2026-10-25T00:00:00Z&to=2026-11-02T00:00:00Z&interval=day",
  "/v1/timeseries?kind=quality&from=2026-10-30T00:00:00Z&to=2026-11-01T00:00:00Z&interval=hour",
  "/v1/timeseries?kind=engagement&from=2026-10-25T00:00:00Z&to=2026-11-02T00:00:00Z&interval=day&os_name=iOS",
];
const PAGED = [
  "/v1/events?limit=50", "/v1/events?limit=40&order=desc&event_name=chunkify.video.view_ended",
  "/v1/events?limit=25&city=London", "/v1/videos?limit=3", "/v1/videos?limit=2&sort_by=views&order=desc",
  "/v1/videos?limit=4&sort_by=total_playing_time_ms&order=desc", "/v1/videos?limit=3&sort_by=playback_score&order=asc",
  "/v1/videos?limit=3&sort_by=unique_viewers&order=desc", "/v1/views?limit=40",
  "/v1/views?limit=30&sort_by=playback_score&order=asc", "/v1/views?limit=30&sort_by=startup_time_ms",
  "/v1/views?limit=25&sort_by=device_type&has_fatal_error=false", "/v1/views?limit=10&has_fatal_error=true",
  "/v1/errors?limit=1", "/v1/errors?browser_name=Chrome",
  ...["browser_name", "os_name", "device_type", "continent", "country", "city", "page_origin", "page_path", "video_id", "viewer_id"]
    .map(dimension => `/v1/dimensions/${dimension}?limit=2`),
  ...["browser_name", "country", "city", "page_path"].flatMap(group =>
    ["summary", "quality", "engagement"].map(kind => `/v1/${kind}?group_by=${group}&limit=2`)),
];

test("ClickHouse reports match local storage for the same events", { skip }, async t => {
  const local = await collector(t, { "storage.backend": "local" });
  const remote = await collector(t, { "storage.backend": "clickhouse", "clickhouse.url": clickhouse,
    "clickhouse.database": database("parity") });
  const records = dataset();
  await Promise.all([ingest(local, records), ingest(remote, records)]);
  for (const path of REPORTS) {
    assertClose(comparable(await remote.get(path)), comparable(await local.get(path)), path);
  }
  for (const path of PAGED) {
    const [expected, actual] = await Promise.all([pages(local, path), pages(remote, path)]);
    assert.ok(expected.length > 0);
    assertClose(comparable(actual), comparable(expected), path);
  }
  const view = (await local.get("/v1/views?limit=1")).views[0].view_id;
  assertClose(comparable(await pages(remote, `/v1/views/${view}?limit=1`)),
    comparable(await pages(local, `/v1/views/${view}?limit=1`)), "view detail");
  for (const resource of ["videos", "views", "errors", "renditions", "timeseries"]) {
    const query = resource === "timeseries" ? { from: "2026-10-25T00:00:00Z", to: "2026-11-02T00:00:00Z", interval: "day" } : {};
    const csv = await Promise.all([local, remote].map(async server => {
      const response = await fetch(`${server.url}/v1/exports`, { method: "POST", body: JSON.stringify({ resource, query }),
        headers: { Authorization: `Bearer ${server.keys.secret_key}`, "Content-Type": "application/json" } });
      assert.equal(response.status, 201, await response.clone().text());
      return (await fetch((await response.json()).download_url)).text();
    }));
    assert.equal(csv[1].split("\n").length, csv[0].split("\n").length, `${resource} CSV rows`);
    assert.equal(csv[1].split("\n")[0], csv[0].split("\n")[0], `${resource} CSV header`);
  }
});

test("servers sharing one ClickHouse database serve the same projects and events", { skip }, async t => {
  const adminKey = `admin-${Math.random()}`;
  const storage = { "storage.backend": "clickhouse", "clickhouse.url": clickhouse,
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
  const registry = await sql(`SELECT document FROM ${storage["clickhouse.database"]}.registry FORMAT TSVRaw`);
  for (const secret of [first.keys.public_key, first.keys.secret_key, project.public_key, project.secret_key]) {
    assert.ok(!registry.includes(secret));
  }
  await sql(`DROP DATABASE IF EXISTS ${storage["clickhouse.database"]}`);
});
