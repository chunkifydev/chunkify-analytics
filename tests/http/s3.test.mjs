import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createServer, request as httpRequest } from "node:http";
import { request as httpsRequest } from "node:https";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { attachAnalytics } from "@chunkify/player-analytics";
import { createHttpAdapter } from "@chunkify/player-analytics/http";
import { assertResponse } from "./openapi-support.mjs";

const binary = process.env.CHUNKIFY_BINARY ?? resolve("target/debug/chunkify-analytics");

function events() {
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const result = [];
  const session = attachAnalytics(media, { videoId: "s3-video", viewerId: "s3-viewer",
    destinations: [{ send(event) { result.push(event); } }] });
  session.startAttempt();
  session.destroy();
  result.at(-1).playing_time_ms = 1200;
  return result;
}

async function installation(t, storage, retention = 0, environment = {}, server = {}, batch = {}, reports = {}) {
  let directory = await mkdtemp(join(tmpdir(), "chunkify-storage-test-"));
  const directories = [directory];
  let child, closed;
  const inherited = { ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("CHUNKIFY_"))),
    CHUNKIFY_GEOIP_DATABASE: resolve("tests/fixtures/GeoIP2-City-Test.mmdb"), ...environment };
  let config;
  async function configure() {
    config = join(directory, "config.toml");
    const serverFields = Object.entries(server).map(([key, value]) => `${key} = ${JSON.stringify(value)}`).join("\n");
    const fields = Object.entries(storage).map(([key, value]) => `${key} = ${JSON.stringify(value)}`).join("\n");
    const batchFields = Object.entries(batch).map(([key, value]) => `${key} = ${JSON.stringify(value)}`).join("\n");
    await writeFile(config, `data_dir = "state"\nprofile = ${reports.profile ?? false}\n[server]\nbind = "127.0.0.1:0"\n${serverFields}\n[storage]\n${fields}\n[retention]\nseconds = ${retention}\ninterval_seconds = ${reports.retention_interval_seconds ?? 86400}\n[batch]\n${batchFields}\n[cache]\nreport_ttl_seconds = ${reports.ttl_seconds ?? 0}\n`, { mode: 0o600 });
  }
  async function stop(signal = "SIGTERM", expectedCode = 0) {
    if (child && child.exitCode === null && child.signalCode === null) child.kill(signal);
    if (closed) {
      const [code, receivedSignal] = await closed;
      if (signal === "SIGTERM") assert.equal(code, expectedCode, app.logs());
      else assert.equal(receivedSignal, signal);
    }
    child = closed = undefined;
  }
  t.after(async () => { await stop(); for (const path of directories) await rm(path, { recursive: true, force: true }); });
  await configure();
  const app = {
    async command(args) {
      const process = spawn(binary, ["--config", config, ...args], { cwd: directory, env: inherited, stdio: "pipe" });
      let stdout = "", stderr = "";
      process.stdout.on("data", chunk => { stdout += chunk; });
      process.stderr.on("data", chunk => { stderr += chunk; });
      const [code] = await once(process, "close");
      assert.equal(code, 0, stderr);
      if (args[0] === "init") app.keys = JSON.parse(stdout);
      return stdout;
    },
    async start() {
      child = spawn(binary, ["--config", config, "serve"], { cwd: directory, env: inherited, stdio: "pipe" });
      closed = once(child, "close");
      let logs = "";
      app.logs = () => logs;
      child.stderr.on("data", chunk => { logs += chunk; });
      app.url = await new Promise((ready, reject) => {
        const timeout = setTimeout(() => { child.kill("SIGKILL"); reject(new Error(`Storage startup timed out: ${logs}`)); }, 120000);
        child.once("error", error => { clearTimeout(timeout); reject(error); });
        child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Storage startup failed: ${logs}`)); });
        child.stdout.on("data", chunk => {
          logs += chunk;
          const match = logs.match(/Listening on (http:\/\/\S+)/);
          if (match) { clearTimeout(timeout); ready(match[1]); }
        });
      });
      app.projects = JSON.parse(await app.command(["project", "list"]));
      if (storage.backend === "s3") {
        await assert.rejects(stat(join(directory, "state", "s3-keys.json")), { code: "ENOENT" });
      }
    },
    stop,
    async replaceTask() {
      await stop();
      directory = await mkdtemp(join(tmpdir(), "chunkify-storage-replacement-"));
      directories.push(directory);
      await configure();
      await app.start();
    },
    async get(path, token = app.keys.secret_key) {
      const response = await fetch(`${app.url}${path}`, { headers: { authorization: `Bearer ${token}` } });
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    },
    async manage(path, method, body, token = app.keys.secret_key) {
      const response = await fetch(`${app.url}/v1/project${path}`, { method,
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        ...(body !== undefined && { body: JSON.stringify(body) }) });
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    },
    async send(event, key = app.keys.public_key, expectedStatus = 201) {
      const response = await fetch(`${app.url}/v1/events`, { method: "POST",
        headers: { "content-type": "application/json", "x-chunkify-ingest-key": key }, body: JSON.stringify(event) });
      assert.equal(response.status, expectedStatus, await response.clone().text());
      return response.json();
    },
    directory: () => directory,
  };
  return app;
}

test("local storage honors a database path relative to the TOML file", async t => {
  const app = await installation(t, { backend: "local", path: "database/events.duckdb" });
  await app.command(["init"]);
  await app.start();
  const records = events();
  for (const event of records) await app.send(event);
  assert.equal((await app.get("/v1/events")).events.length, records.length);
  await app.stop();
  assert.ok((await stat(join(app.directory(), "database/events.duckdb"))).isFile());
  await assert.rejects(stat(join(app.directory(), "state/analytics.duckdb")), { code: "ENOENT" });
});

const endpoint = process.env.CHUNKIFY_S3_TEST_ENDPOINT;
function s3Settings() {
  return { backend: "s3", url: `s3://${process.env.CHUNKIFY_S3_TEST_BUCKET ?? "chunkify-analytics-test"}/test-${randomUUID()}/`,
    endpoint, region: process.env.CHUNKIFY_S3_TEST_REGION ?? "us-east-1",
    access_key_id: process.env.CHUNKIFY_S3_TEST_ACCESS_KEY_ID ?? "test",
    secret_access_key: process.env.CHUNKIFY_S3_TEST_SECRET_ACCESS_KEY ?? "test",
    url_style: "path", use_ssl: process.env.CHUNKIFY_S3_TEST_USE_SSL === "true" };
}

async function waitFor(check, description, timeout = 15000) {
  const deadline = Date.now() + timeout;
  while (!(await check())) {
    assert.ok(Date.now() < deadline, `Timed out waiting for ${description}`);
    await delay(50);
  }
}

test("buffered S3 ingestion responds before upload and reads do not flush or postpone the batch", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, { ...s3Settings(), flush_interval_seconds: 2 });
  await app.command(["init"]);
  await app.start();
  const first = events()[0], second = events()[0];
  const before = Date.now();
  const response = await fetch(`${app.url}/v1/events`, { method: "POST",
    headers: { "content-type": "application/json", "x-chunkify-ingest-key": app.keys.public_key },
    body: JSON.stringify(first) });
  assert.deepEqual(await assertResponse("/v1/events", "post", response, 201), { accepted: true, duplicate: false });
  assert.ok(Date.now() - before < 1000, "acceptance must not wait for the upload timer");
  assert.deepEqual((await app.get("/v1/events")).events, []);
  const exportResponse = await fetch(`${app.url}/v1/exports`, { method: "POST",
    headers: { authorization: `Bearer ${app.keys.secret_key}`, "content-type": "application/json" },
    body: JSON.stringify({ resource: "events" }) });
  assert.equal(exportResponse.status, 201);
  const link = await exportResponse.json();
  const download = await fetch(link.download_url);
  assert.equal(download.status, 200);
  assert.equal((await download.text()).trimEnd().split("\r\n").length, 1, "exports must show only uploaded events");
  await delay(1200);
  await app.send(second);
  await app.send(first);
  assert.deepEqual((await app.get("/v1/events")).events, []);
  assert.doesNotMatch(app.logs(), /batch_saved/);
  await waitFor(() => /batch_saved/.test(app.logs()), "the first receipt's upload deadline", 1800);
  assert.match(app.logs(), /requests=3 received=3 inserted=3/);
  const saved = (await app.get("/v1/events")).events;
  assert.deepEqual(saved.map(row => row.event), [first, second, first]);
  assert.ok(Date.parse(saved[0].received_at) >= before);
  assert.ok(Date.parse(saved[0].received_at) < before + 1000, "receipt time precedes upload");
  await app.replaceTask();
  assert.deepEqual((await app.get("/v1/events")).events, saved);
});

for (const [limit, batch, extra, expectedBatch] of [
  ["event", { max_events: 2 }, "", /requests=2 received=2 inserted=2/],
  ["byte", { max_bytes: 16384 }, "x".repeat(4000), /requests=1 received=1 inserted=1/],
]) test(`buffered S3 flushes early at the ${limit} limit`, {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, { ...s3Settings(), flush_interval_seconds: 60 }, 0, {}, {}, batch);
  await app.command(["init"]);
  await app.start();
  const records = [events()[0], events()[0]].map(event => ({ ...event, extra }));
  for (const event of records) await app.send(event);
  await waitFor(() => /batch_saved/.test(app.logs()), "a full batch upload");
  assert.match(app.logs(), expectedBatch);
  await app.replaceTask();
  assert.deepEqual((await app.get("/v1/events")).events.map(row => row.event), records);
});

test("buffered S3 holds admission budgets until upload and releases them afterward", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, { ...s3Settings(), flush_interval_seconds: 1 }, 0, {}, {}, { queue_max_requests: 1 });
  await app.command(["init"]);
  await app.start();
  await app.send(events()[0]);
  await app.send(events()[0], undefined, 503);
  await waitFor(() => /batch_saved/.test(app.logs()), "upload to release the request budget");
  await app.send(events()[0]);
  await app.replaceTask();
  assert.equal((await app.get("/v1/events")).events.length, 2);
});

test("buffered S3 defaults can collect over 100 requests and flush at 10 MiB", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, { ...s3Settings(), flush_interval_seconds: 60 }, 0, {}, { ingest_per_second: 10000 });
  await app.command(["init"]);
  await app.start();
  const payload = "x".repeat(4000);
  let sent = 0;
  for (; sent < 150; sent++) await app.send({ ...events()[0], extra: payload });
  assert.doesNotMatch(app.logs(), /batch_saved/, "old event and byte limits must not flush the new buffer");
  while (!/batch_saved/.test(app.logs()) && sent < 2000) {
    await app.send({ ...events()[0], extra: payload });
    sent++;
  }
  await waitFor(() => /batch_saved/.test(app.logs()), "the default 10 MiB buffer upload");
  const size = Number(app.logs().match(/batch_saved[^\n]* bytes=(\d+)/)[1]);
  assert.ok(size > 10485760 - 65536 && size <= 10485760, `unexpected batch size: ${size}`);
  await app.replaceTask();
  assert.equal((await app.get("/v1/summary")).views, sent);
});

test("buffered S3 drains on graceful shutdown and can lose the last minute on a crash", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, { ...s3Settings(), flush_interval_seconds: 60 });
  await app.command(["init"]);
  await app.start();
  const customer = JSON.parse(await app.command(["project", "create", "Buffered customer"]));
  const first = events()[0], second = events()[0];
  await app.send(first);
  await app.send(second, customer.public_key);
  assert.deepEqual((await app.get("/v1/events")).events, []);
  assert.deepEqual((await app.get("/v1/events", customer.secret_key)).events, []);
  assert.doesNotMatch(app.logs(), /batch_saved/);
  const before = Date.now();
  await app.replaceTask();
  assert.ok(Date.now() - before < 15000, "shutdown should flush without waiting a minute");
  assert.deepEqual((await app.get("/v1/events")).events.map(row => row.event), [first]);
  assert.deepEqual((await app.get("/v1/events", customer.secret_key)).events.map(row => row.event), [second]);
  await app.send(events()[0]);
  assert.doesNotMatch(app.logs(), /batch_saved/);
  await app.stop("SIGKILL");
  await app.start();
  assert.deepEqual((await app.get("/v1/events")).events.map(row => row.event), [first]);
});

test("buffered S3 retries partial uploads without duplicating files or dropping later requests", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const storage = s3Settings();
  const upstream = `${storage.use_ssl ? "https" : "http"}://${storage.endpoint}`;
  const successful = new Set();
  let fail = true;
  const proxy = createServer((request, response) => {
    const eventWrite = request.method === "PUT" && request.url.includes(".parquet");
    if (eventWrite && fail && successful.size > 0) {
      request.resume();
      response.writeHead(403, { "content-type": "application/xml" });
      response.end("<Error><Code>AccessDenied</Code><Message>Injected test failure</Message></Error>");
      return;
    }
    const forward = (storage.use_ssl ? httpsRequest : httpRequest)(new URL(request.url, upstream), {
      method: request.method, headers: request.headers,
    }, result => {
      if (eventWrite && result.statusCode >= 200 && result.statusCode < 300) successful.add(request.url);
      response.writeHead(result.statusCode, result.headers);
      result.pipe(response);
    });
    forward.on("error", () => { response.writeHead(502); response.end(); });
    request.pipe(forward);
  });
  proxy.listen(0, "127.0.0.1");
  await once(proxy, "listening");
  t.after(() => new Promise(resolve => { proxy.close(resolve); proxy.closeAllConnections(); }));
  const app = await installation(t, { ...storage, endpoint: `127.0.0.1:${proxy.address().port}`,
    use_ssl: false, flush_interval_seconds: 1 });
  await app.command(["init"]);
  await app.start();
  const other = JSON.parse(await app.command(["project", "create", "Retry customer"]));
  const records = [events()[0], events()[0], events()[0]];
  await app.send(records[0]);
  await app.send(records[1], other.public_key);
  await waitFor(() => /batch_failed/.test(app.logs()), "an upload failure", 30000);
  assert.equal(successful.size, 1, "one project must upload before the injected failure");
  await app.send(records[2]);
  fail = false;
  await waitFor(() => (app.logs().match(/batch_saved/g) ?? []).length === 2, "both the retried and later batches", 30000);
  assert.equal(successful.size, 3, "the successful part of the retried batch must reuse its object name");
  assert.deepEqual((await app.get("/v1/events")).events.map(row => row.event), [records[0], records[2]]);
  assert.deepEqual((await app.get("/v1/events", other.secret_key)).events.map(row => row.event), [records[1]]);
  await app.send(events()[0]);
  fail = true;
  await app.stop("SIGTERM", 1);
  assert.match(app.logs(), /Cannot upload buffered S3 events during shutdown/);
});

test("S3 query index keeps reports available during publication and exposes events only after storage", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const storage = s3Settings();
  const upstream = `${storage.use_ssl ? "https" : "http"}://${storage.endpoint}`;
  let hold = false, eventReads = 0, lists = 0;
  const waiting = [];
  const proxy = createServer((request, response) => {
    if (request.method === "GET" && request.url.includes(".parquet")) eventReads++;
    if (new URL(request.url, "http://proxy").searchParams.get("prefix")?.includes("events/")) lists++;
    const forward = () => {
      const transfer = (storage.use_ssl ? httpsRequest : httpRequest)(new URL(request.url, upstream), {
        method: request.method, headers: request.headers,
      }, result => { response.writeHead(result.statusCode, result.headers); result.pipe(response); });
      transfer.on("error", () => { response.writeHead(502); response.end(); });
      request.pipe(transfer);
    };
    if (hold && request.method === "PUT" && request.url.includes(".parquet")) waiting.push(forward);
    else forward();
  });
  proxy.listen(0, "127.0.0.1");
  await once(proxy, "listening");
  t.after(() => new Promise(resolve => { proxy.close(resolve); proxy.closeAllConnections(); }));
  const app = await installation(t, { ...storage, endpoint: `127.0.0.1:${proxy.address().port}`, use_ssl: false });
  await app.command(["init"]);
  await app.start();
  const other = JSON.parse(await app.command(["project", "create", "Independent tenant"]));
  await app.send({ ...events()[0], video_id: "first-project" });
  await app.send({ ...events()[0], video_id: "second-project" }, other.public_key);
  const before = { eventReads, lists };
  hold = true;
  let saved = false;
  const write = app.send({ ...events()[0], video_id: "during-write" }, other.public_key).then(() => { saved = true; });
  try {
    await waitFor(() => waiting.length > 0, "the durable upload to start");
    const [first, second] = await Promise.all([app.get("/v1/videos"), app.get("/v1/videos", other.secret_key)]);
    assert.deepEqual(first.videos.map(video => video.video_id), ["first-project"]);
    assert.deepEqual(second.videos.map(video => video.video_id), ["second-project"]);
    assert.equal(saved, false, "synchronous ingestion must wait for S3 publication");
    assert.deepEqual({ eventReads, lists }, before, "normal reports must query the index without listing or reading S3 batches");
  } finally {
    hold = false;
    for (const release of waiting.splice(0)) release();
    await write;
  }
  assert.deepEqual((await app.get("/v1/videos", other.secret_key)).videos.map(v => v.video_id).sort(), ["during-write", "second-project"]);
  await app.replaceTask();
  assert.deepEqual((await app.get("/v1/videos", other.secret_key)).videos.map(v => v.video_id).sort(), ["during-write", "second-project"], "a replacement rebuilds the disposable index from S3");
});

test("S3 cached reports expire, isolate projects and still authenticate every read", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings(), 0, {}, {}, {}, { ttl_seconds: 2 });
  await app.command(["init"]);
  await app.start();
  const other = JSON.parse(await app.command(["project", "create", "Cache isolation"]));
  await app.send(events()[0]);
  assert.equal((await app.get("/v1/summary")).views, 1);
  assert.equal((await app.get("/v1/summary", other.secret_key)).views, 0);
  await app.send(events()[0]);
  assert.equal((await app.get("/v1/summary")).views, 1, "reads within the freshness window reuse the report");
  assert.equal((await fetch(`${app.url}/v1/summary`, { headers: { authorization: "Bearer invalid" } })).status, 401);
  assert.equal((await app.get("/v1/summary?video_id=absent")).views, 0, "filters have separate entries");
  await delay(2100);
  assert.equal((await app.get("/v1/summary")).views, 2, "published events appear after cache expiry");
});

test("S3 stores repeated events, isolates projects and survives task replacement", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const adminKey = `s3-admin-${randomUUID()}`;
  const app = await installation(t, s3Settings(), 0, { CHUNKIFY_ADMIN_KEY: adminKey });
  await app.command(["init"]);
  const cache = join(app.directory(), "state", "s3-keys.json");
  await assert.rejects(stat(cache), { code: "ENOENT" });
  await app.start();
  const created = await fetch(`${app.url}/v1/projects`, { method: "POST", headers: {
    authorization: `Bearer ${adminKey}`, "content-type": "application/json" },
    body: JSON.stringify({ name: "Live website" }) });
  assert.equal(created.status, 201, await created.clone().text());
  const live = await created.json();
  assert.match(live.project_id, /^[0-9a-f-]{36}$/);
  assert.equal(live.restart_required, false);
  assert.deepEqual((await app.get("/v1/events")).events, []);
  assert.equal((await app.get("/v1/summary")).views, 0);
  const dashboard = JSON.parse(await app.command(["key", "create", "default", "secret", "S3 dashboard", "--read-only"]));
  assert.equal(dashboard.read_only, true);
  assert.equal(dashboard.restart_required, false);
  const records = events();
  for (const event of records) await app.send(event);
  await app.send(records[0]);
  assert.equal((await app.get("/v1/events")).events.length, records.length + 1, "S3 does not deduplicate");
  const adapter = createHttpAdapter(`${app.url}/v1/events`, { headers: { "X-Chunkify-Ingest-Key": live.public_key } });
  for (const event of records) await adapter.send(event);
  let liveRows = (await app.get("/v1/events", live.secret_key)).events;
  assert.equal(liveRows.length, records.length);
  assert.ok(liveRows.every(row => row.project_id === live.project_id && !row.otlp));
  for (const path of ["/v1/summary", "/v1/quality", "/v1/engagement", "/v1/videos", "/v1/renditions"]) {
    assert.deepEqual(await app.get(path), await app.get(path, live.secret_key));
  }
  await app.manage("", "PATCH", { name: "Renamed S3 customer", allowed_origins: ["https://s3-customer.example"] }, live.secret_key);
  const projectKeys = await app.manage("/keys", "GET", undefined, live.secret_key);
  const secretId = projectKeys.keys.find(key => key.permission === "secret").id;
  const oldSecret = live.secret_key;
  live.secret_key = (await app.manage(`/keys/${secretId}/rotate`, "POST", undefined, oldSecret)).key;
  const first = await app.get("/v1/events?limit=1");
  const originalKeys = app.keys;
  await app.replaceTask();
  assert.equal(app.keys.public_key, originalKeys.public_key);
  assert.equal(app.keys.secret_key, originalKeys.secret_key);
  assert.equal(JSON.stringify(app.keys).includes(adminKey), false);
  assert.equal(app.projects.find(project => project.id === live.project_id).name, "Renamed S3 customer");
  assert.deepEqual(app.projects.find(project => project.id === live.project_id).settings.allowed_origins, ["https://s3-customer.example"]);
  assert.equal((await fetch(`${app.url}/v1/project/keys`, { headers: { authorization: `Bearer ${oldSecret}` } })).status, 401);
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, records.length + 1);
  assert.equal(app.projects.find(project => project.id === "default").keys.find(key => key.id === dashboard.key_id).read_only, true);
  const denied = await fetch(`${app.url}/v1/project`, { method: "DELETE", headers: { authorization: `Bearer ${dashboard.key}` } });
  assert.equal(denied.status, 403);
  const rotatedDashboard = await app.manage(`/keys/${dashboard.key_id}/rotate`, "POST");
  assert.equal(rotatedDashboard.read_only, true);
  assert.equal(rotatedDashboard.restart_required, false);
  assert.equal((await fetch(`${app.url}/v1/events`, { headers: { authorization: `Bearer ${dashboard.key}` } })).status, 401);
  assert.equal((await app.get("/v1/events", rotatedDashboard.key)).events.length, records.length + 1);
  const next = await app.get(`/v1/events?limit=100&cursor=${encodeURIComponent(first.next_cursor)}`);
  assert.equal(next.events.length, records.length);
  await app.send(records[0]);
  assert.equal((await app.get("/v1/events")).events.length, records.length + 2);
  liveRows = (await app.get("/v1/events", live.secret_key)).events;
  assert.equal(liveRows.length, records.length);
  await assert.rejects(stat(join(app.directory(), "state/analytics.duckdb")), { code: "ENOENT" });
});

test("S3 HTTP retention and newly created keys remain active across task replacement", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const adminKey = `s3-admin-${randomUUID()}`;
  const app = await installation(t, s3Settings(), 0, { CHUNKIFY_ADMIN_KEY: adminKey });
  await app.command(["init"]);
  await app.start();
  const response = await fetch(`${app.url}/v1/projects`, { method: "POST", headers: {
    authorization: `Bearer ${adminKey}`, "content-type": "application/json" },
    body: JSON.stringify({ name: "Retained customer", retention_seconds: 1 }) });
  assert.equal(response.status, 201, await response.clone().text());
  const project = await response.json();
  assert.equal(project.effective_retention_seconds, 1);
  const created = await fetch(`${app.url}/v1/project/keys`, { method: "POST", headers: {
    authorization: `Bearer ${project.secret_key}`, "content-type": "application/json" },
    body: JSON.stringify({ name: "Dashboard", permission: "secret", read_only: true }) });
  assert.equal(created.status, 201, await created.clone().text());
  const dashboard = await created.json();
  const event = events()[0];
  await app.send(event, project.public_key);
  await app.send(event);
  await delay(1200);
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, 0, "creation policy filters reads before a sweep");
  assert.equal((await app.get("/v1/events")).events.length, 1);
  const unlimited = await app.manage("", "PATCH", { retention_seconds: 0 }, project.secret_key);
  assert.equal(unlimited.effective_retention_seconds, 0);
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, 1, "the unswept file is still retained");
  await app.replaceTask();
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, 1);
  const keys = await app.get("/v1/project/keys", project.secret_key);
  assert.equal(keys.keys.find(key => key.id === dashboard.key_id).read_only, true);
  assert.ok(!JSON.stringify(keys).includes(dashboard.key));
  await app.manage("", "PATCH", { retention_seconds: 1 }, project.secret_key);
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, 0);
  await app.replaceTask();
  const inherited = await app.manage("", "PATCH", { retention_seconds: null }, project.secret_key);
  assert.equal(inherited.retention_seconds, null);
  assert.equal(inherited.effective_retention_seconds, 0);
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, 0, "startup cleanup removed the expired file");
  assert.equal((await app.get("/v1/events")).events.length, 1);
});

test("S3 uses the AWS credential chain when explicit storage keys are omitted", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const storage = s3Settings();
  const environment = { AWS_ACCESS_KEY_ID: storage.access_key_id, AWS_SECRET_ACCESS_KEY: storage.secret_access_key,
    AWS_EC2_METADATA_DISABLED: "true" };
  delete storage.access_key_id;
  delete storage.secret_access_key;
  const app = await installation(t, storage, 0, environment);
  await app.command(["init"]);
  await app.start();
  const event = events()[0];
  await app.send(event);
  await app.replaceTask();
  assert.deepEqual((await app.get("/v1/events")).events.map(row => row.event), [event]);
});

test("S3 hides expired events before cleanup and deletes expired batches on startup", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings(), 2);
  await app.command(["init"]);
  await app.start();
  await app.send(events()[0]);
  assert.equal((await app.get("/v1/events")).events.length, 1);
  await delay(2100);
  assert.deepEqual((await app.get("/v1/events")).events, []);
  await app.stop();
  await app.start();
  assert.deepEqual((await app.get("/v1/events")).events, []);
});

test("S3 project deletion survives replacement and expires retained events with its own policy", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings());
  await app.command(["init"]);
  await app.start();
  const customer = JSON.parse(await app.command(["project", "create", "Deleted customer"]));
  await app.command(["project", "retention", customer.project_id, "1"]);
  const event = events()[0];
  await app.send(event);
  await app.send(event, customer.public_key);
  const result = await app.manage("", "DELETE", undefined, customer.secret_key);
  assert.equal(result.deleted, true);
  assert.equal((await fetch(`${app.url}/v1/events`, { headers: { authorization: `Bearer ${customer.secret_key}` } })).status, 401);
  await delay(1200);
  await app.replaceTask();
  assert.match(app.logs(), /Retention removed 1 events/);
  assert.equal((await app.get("/v1/events")).events.length, 1);
  const saved = app.projects.find(project => project.id === customer.project_id);
  assert.equal(saved.deleted, true);
  assert.deepEqual(saved.keys, []);
  assert.equal(saved.settings.retention_seconds, 1);
  assert.equal((await fetch(`${app.url}/v1/events`, { headers: { authorization: `Bearer ${customer.secret_key}` } })).status, 401);
});

test("S3 video rankings preserve metric ties and cursors across task replacement", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings());
  await app.command(["init"]);
  await app.start();
  const other = JSON.parse(await app.command(["project", "create", "Other rankings"]));
  async function playback(videoId, playingTime, key) {
    const records = events().map(event => ({ ...event, video_id: videoId }));
    records.at(-1).playing_time_ms = playingTime;
    for (const event of records) await app.send(event, key);
  }
  for (const [video, count, playing] of [["a", 2, 1000], ["b", 3, 100], ["c", 1, 5000.25], ["d", 1, 5000.25]]) {
    for (let view = 0; view < count; view++) await playback(video, playing);
  }
  await playback("a", 9999999, other.public_key);
  const query = params => `/v1/videos?${new URLSearchParams(params)}`;
  const views = { sort_by: "views", order: "desc", limit: "1" };
  const watch = { sort_by: "total_playing_time_ms", order: "desc", limit: "1" };
  const firstViews = await app.get(query(views)), firstWatch = await app.get(query(watch));
  await app.replaceTask();
  for (const [params, first, expected] of [[views, firstViews, ["b", "a", "c", "d"]], [watch, firstWatch, ["c", "d", "a", "b"]]]) {
    let page = first;
    const rows = [...page.videos];
    while (page.next_cursor) {
      page = await app.get(query({ ...params, cursor: page.next_cursor }));
      rows.push(...page.videos);
    }
    assert.deepEqual(rows.map(video => video.video_id), expected);
    assert.equal(rows.find(video => video.video_id === "a").total_playing_time_ms, 2000);
    assert.equal(rows.find(video => video.video_id === "c").total_playing_time_ms, 5000.25);
  }
});

test("S3 grouped metrics preserve context, project isolation and page watermarks across replacement", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings());
  await app.command(["init"]);
  await app.start();
  const other = JSON.parse(await app.command(["project", "create", "Other groups"]));
  async function playback(browser, os, device, key, onlyStart = false) {
    const records = events().map(event => ({ ...event,
      ...(browser !== null && { browser_context: { schema_version: 1, browser_name: browser, os_name: os, device_type: device } }) }));
    for (const event of onlyStart ? records.slice(0, 1) : records) await app.send(event, key);
  }
  await playback(null, null, null, undefined, true);
  await playback("Chrome", "Linux", "desktop");
  await playback("Firefox", "Windows", "mobile");
  await playback("unknown", "unknown", "unknown");
  await playback("Safari", "Other", "tablet", other.public_key);
  const snapshots = [];
  for (const route of ["summary", "quality", "engagement"]) for (const group_by of ["browser_name", "os_name", "device_type"]) {
    const path = `/v1/${route}?group_by=${group_by}`;
    snapshots.push({ path, first: await app.get(`${path}&limit=1`), full: await app.get(`${path}&limit=1000`) });
  }
  await app.replaceTask();
  await playback("Zulu", "Later", "tablet");
  for (const { path, first, full } of snapshots) {
    let page = first;
    const groups = [...page.groups];
    while (page.next_cursor) {
      page = await app.get(`${path}&limit=2&cursor=${encodeURIComponent(page.next_cursor)}`);
      groups.push(...page.groups);
    }
    assert.deepEqual(groups, full.groups);
    assert.equal(groups[0].value, null);
    assert.equal(groups.reduce((sum, group) => sum + group.data.views, 0), 4);
  }
  const refreshed = await app.get("/v1/summary?group_by=browser_name");
  assert.deepEqual(refreshed.groups.map(group => group.value), [null, "Chrome", "Firefox", "Zulu", "unknown"]);
});

test(
  "S3 session queries and CSV links use isolated read connections and survive replacement",
  {
    skip:
      !endpoint &&
      "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
  },
  async (t) => {
    const app = await installation(t, s3Settings());
    await app.command(["init"]);
    await app.start();
    const create = async (resource) => {
      const response = await fetch(`${app.url}/v1/exports`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${app.keys.secret_key}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ resource }),
      });
      assert.equal(response.status, 201, await response.clone().text());
      return response.json();
    };
    const empty = await create("events");
    const emptyResponse = await fetch(empty.download_url);
    assert.equal(emptyResponse.status, 200);
    assert.equal(
      (await emptyResponse.text()).trimEnd().split("\r\n").length,
      1,
    );
    const records = events().map((event) => ({
      ...event,
      browser_context: {
        schema_version: 1,
        browser_name: "Chrome",
        os_name: "Linux",
        device_type: "desktop",
      },
    }));
    records.at(-1).startup_time_ms = 250;
    for (const event of records) await app.send(event);
    const error = {
      ...records[0],
      event_id: randomUUID(),
      event_name: "chunkify.video.playback_error",
      sequence: 3,
      error_code: 2,
      error_category: "network",
      error_fatal: true,
    };
    await app.send(error);
    await app.send(records[0]);
    const other = JSON.parse(
      await app.command(["project", "create", "Other export"]),
    );
    for (const event of records)
      await app.send(
        {
          ...event,
          browser_context: {
            schema_version: 1,
            browser_name: "Firefox",
            os_name: "Linux",
            device_type: "desktop",
          },
        },
        other.public_key,
      );
    const views = await app.get("/v1/views?has_fatal_error=true");
    assert.equal(views.views.length, 1);
    assert.equal(views.views[0].playing_time_ms, 1200);
    const detail = await app.get(`/v1/views/${records[0].view_id}`);
    assert.equal(detail.events.length, records.length + 2);
    assert.equal(detail.view.has_fatal_error, true);
    assert.deepEqual((await app.get("/v1/errors")).errors, [
      { error_code: 2, fatal: true, error_events: 1, affected_views: 1 },
    ]);
    assert.deepEqual((await app.get("/v1/dimensions/browser_name")).values, [
      { value: "Chrome", views: 1 },
    ]);
    const from = records[0].occurred_at,
      to = new Date(Date.parse(from) + 86400000).toISOString();
    for (const kind of ["quality", "engagement"]) {
      const series = await app.get(
        `/v1/timeseries?${new URLSearchParams({ kind, from, to, interval: "day" })}`,
      );
      assert.equal(series.buckets[0].views, 1);
      if (kind === "quality")
        assert.equal(series.buckets[0].startup_time_ms.median, 250);
    }
    const link = await create("events");
    const download = await fetch(link.download_url);
    assert.equal(download.status, 200);
    const csv = await download.text();
    assert.equal(csv.trimEnd().split("\r\n").length, records.length + 3);
    assert.match(csv, /Chrome/);
    assert.doesNotMatch(csv, /Firefox/);
    await app.replaceTask();
    assert.equal(
      (await fetch(`${app.url}${new URL(link.download_url).pathname}`)).status,
      404,
    );
    const fresh = await create("views");
    const refreshed = await fetch(fresh.download_url);
    assert.equal(refreshed.status, 200);
    assert.match(await refreshed.text(), /Chrome/);
  },
);

test("S3 video reports match local storage after repeated delivery and replacement", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const local = await installation(t, { backend: "local" });
  const remote = await installation(t, s3Settings());
  const records = [];
  for (const [index, [playing, progress, outcome]] of [
    [0, 0, "failed_before_start"], [10000, 0.5, "started"], [300000, 1, "started"],
  ].entries()) {
    const attempt = events().map(event => ({ ...event,
      browser_context: { schema_version: 1, browser_name: index === 0 ? "Firefox" : "Chrome", device_type: "desktop" },
      occurred_at: "2026-10-19T12:00:00Z" }));
    Object.assign(attempt.at(-1), { playing_time_ms: playing, furthest_progress_ratio: progress,
      media_duration_seconds: 600, startup_outcome: outcome,
      played_ranges: index === 0 ? [] : [{ start_seconds: 0, end_seconds: 10 }, { start_seconds: 20, end_seconds: 30 }],
      rendition_playing_time: [{ width: 1280, height: 720, codec: "avc1", bitrate_bps: 2000000,
        frame_rate: 30, playing_time_ms: playing }] });
    records.push(...attempt);
  }
  const snapshots = [];
  for (const app of [local, remote]) {
    await app.command(["init"]);
    await app.start();
    for (const record of records) {
      await app.send(record);
      await app.send(record, undefined, app === local ? 200 : 201);
    }
    const responses = [];
    for (const kind of ["engagement", "quality"]) {
      const path = `/v1/${kind}?video_id=s3-video`;
      responses.push(await app.get(path));
      responses.push(await app.get(`${path}&group_by=browser_name`));
      responses.push(await app.get(`/v1/timeseries?kind=${kind}&video_id=s3-video&from=2026-10-19T00:00:00Z&to=2026-10-21T00:00:00Z&interval=day`));
    }
    responses.push(await app.get("/v1/renditions?video_id=s3-video"));
    responses.push(await app.get("/v1/retention?video_id=s3-video"));
    snapshots.push(responses);
  }
  assert.deepEqual(snapshots[1], snapshots[0]);
  assert.equal(snapshots[1][0].progress_reached.sample_views, 3);
  assert.equal(snapshots[1][0].watch_time_ms.distribution.from_10_to_30_seconds, 1);
  assert.equal(snapshots[1][3].startup_outcomes.failed_before_start.views, 1);
  assert.equal(snapshots[1][6].average_bitrate_bps, 2000000);
  assert.equal(snapshots[1][6].total_playing_time_ms, 310000);
  assert.equal(snapshots[1][6].sample_views, 2);
  assert.equal(snapshots[1][7].sample_views, 3);
  assert.equal(snapshots[1][7].buckets[0].views, 2);
  assert.equal(snapshots[1][7].buckets[4].views, 0);
  await remote.replaceTask();
  assert.deepEqual(await remote.get("/v1/engagement?video_id=s3-video"), snapshots[0][0]);
  assert.deepEqual(await remote.get("/v1/quality?video_id=s3-video"), snapshots[0][3]);
  assert.deepEqual(await remote.get("/v1/renditions?video_id=s3-video"), snapshots[0][6]);
  assert.deepEqual(await remote.get("/v1/retention?video_id=s3-video"), snapshots[0][7]);
});


test("S3 persists country, city and page context without request addresses across replacement", {
  skip: !endpoint && "Set CHUNKIFY_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings(), 0, {}, { trusted_proxies: ["127.0.0.1", "10.0.0.0/8"] });
  await app.command(["init"]);
  await app.start();
  const event = { ...events()[0], browser_context: { schema_version: 1,
    browser_name: "Chrome", page_origin: "https://example.com", page_path: "/watch/first" } };
  const response = await fetch(`${app.url}/v1/events`, { method: "POST", headers: {
    "content-type": "application/json", "x-chunkify-ingest-key": app.keys.public_key,
    "x-forwarded-for": "81.2.69.160, 10.1.2.3",
  }, body: JSON.stringify(event) });
  assert.equal(response.status, 201, await response.text());
  const expected = { schema_version: 1, continent: "EU", country: "GB", city: "London", browser_name: "Chrome",
    page_origin: "https://example.com", page_path: "/watch/first" };
  const before = (await app.get("/v1/events")).events;
  assert.deepEqual(before[0].event_context, expected);
  await app.replaceTask();
  const after = (await app.get("/v1/events")).events;
  assert.deepEqual(after, before);
  for (const [dimension, value] of [["country", "GB"], ["city", "London"], ["continent", "EU"], ["page_origin", "https://example.com"], ["page_path", "/watch/first"]]) {
    for (const route of dimension.startsWith("page_") ? ["summary"] : ["summary", "quality", "engagement"]) {
      const result = await app.get(`/v1/${route}?group_by=${dimension}&country=GB`);
      assert.equal(result.groups.length, 1);
      assert.equal(result.groups[0].value, value);
      assert.equal(result.groups[0].data.views, 1);
    }
    assert.deepEqual((await app.get(`/v1/dimensions/${dimension}?country=GB`)).values, [{ value, views: 1 }]);
  }
  assert.equal((await app.get("/v1/summary?country=JP")).views, 0);
  assert.equal((await app.get("/v1/events?country=GB&city=London&continent=EU")).events.length, 1);
  for (const value of ["81.2.69.160", "10.1.2.3", "peer_ip", "client_ip"]) {
    assert.ok(!JSON.stringify(after).includes(value));
  }
});
