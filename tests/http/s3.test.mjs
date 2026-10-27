import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { attachAnalytics } from "@chunkify/analytics";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";

const binary = process.env.COLLECTOR_BINARY ?? resolve("target/debug/chunkify-analytics");

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

async function installation(t, storage, retention = 0, environment = {}) {
  let directory = await mkdtemp(join(tmpdir(), "chunkify-storage-test-"));
  const directories = [directory];
  let child, closed;
  const inherited = { ...Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("COLLECTOR_"))),
    ...environment };
  let config;
  async function configure() {
    config = join(directory, "collector.toml");
    const fields = Object.entries(storage).map(([key, value]) => `${key} = ${JSON.stringify(value)}`).join("\n");
    await writeFile(config, `data_dir = "state"\n[server]\nbind = "127.0.0.1:0"\n[storage]\n${fields}\n[retention]\nseconds = ${retention}\ninterval_seconds = 86400\n`, { mode: 0o600 });
  }
  async function stop() {
    if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    if (closed) assert.equal((await closed)[0], 0);
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
      return stdout;
    },
    async start() {
      child = spawn(binary, ["--config", config, "serve"], { cwd: directory, env: inherited, stdio: "pipe" });
      closed = once(child, "close");
      let logs = "";
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
      const filename = storage.backend === "s3" ? "s3-keys.json" : "keys.json";
      app.keys = JSON.parse(await readFile(join(directory, "state", filename), "utf8"));
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
    async send(event, key = app.keys.public_key) {
      const response = await fetch(`${app.url}/v1/events`, { method: "POST",
        headers: { "content-type": "application/json", "x-chunkify-ingest-key": key }, body: JSON.stringify(event) });
      assert.equal(response.status, 201, await response.clone().text());
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

const endpoint = process.env.COLLECTOR_S3_TEST_ENDPOINT;
function s3Settings() {
  return { backend: "s3", url: `s3://${process.env.COLLECTOR_S3_TEST_BUCKET ?? "chunkify-analytics-test"}/test-${randomUUID()}/`,
    endpoint, region: process.env.COLLECTOR_S3_TEST_REGION ?? "us-east-1",
    access_key_id: process.env.COLLECTOR_S3_TEST_ACCESS_KEY_ID ?? "test",
    secret_access_key: process.env.COLLECTOR_S3_TEST_SECRET_ACCESS_KEY ?? "test",
    url_style: "path", use_ssl: process.env.COLLECTOR_S3_TEST_USE_SSL === "true" };
}

test("S3 stores repeated events, preserves OTLP, isolates projects and survives task replacement", {
  skip: !endpoint && "Set COLLECTOR_S3_TEST_ENDPOINT for an S3-compatible test bucket",
}, async t => {
  const app = await installation(t, s3Settings());
  await app.command(["init"]);
  const live = JSON.parse(await app.command(["project", "create", "live"]));
  await app.start();
  assert.deepEqual((await app.get("/v1/events")).events, []);
  assert.equal((await app.get("/v1/summary")).views, 0);
  const dashboard = JSON.parse(await app.command(["key", "create", "default", "secret", "S3 dashboard"]));
  const records = events();
  for (const event of records) await app.send(event);
  await app.send(records[0]);
  assert.equal((await app.get("/v1/events")).events.length, records.length + 1, "S3 does not deduplicate");
  const adapter = createOtlpAdapter(`${app.url}/v1/logs`, { headers: { "X-Chunkify-Ingest-Key": live.public_key },
    resourceAttributes: { "service.name": "s3-test" } });
  for (const event of records) await adapter.send(event);
  let liveRows = (await app.get("/v1/events", live.secret_key)).events;
  assert.equal(liveRows.length, records.length);
  assert.ok(liveRows.every(row => row.project_id === live.project_id && row.otlp));
  for (const path of ["/v1/summary", "/v1/quality", "/v1/engagement", "/v1/videos"]) {
    assert.deepEqual(await app.get(path), await app.get(path, live.secret_key));
  }
  await app.manage("/allowed-origins", "PUT", { allowed_origins: ["https://s3-customer.example"] }, live.secret_key);
  const projectKeys = await app.manage("/keys", "GET", undefined, live.secret_key);
  const secretId = projectKeys.keys.find(key => key.permission === "secret").id;
  const oldSecret = live.secret_key;
  live.secret_key = (await app.manage(`/keys/${secretId}/rotate`, "POST", undefined, oldSecret)).key;
  const first = await app.get("/v1/events?limit=1");
  const originalKeys = app.keys;
  await app.replaceTask();
  assert.equal(app.keys.public_key, originalKeys.public_key);
  assert.equal(app.keys.secret_key, originalKeys.secret_key);
  assert.deepEqual(app.keys.projects.find(project => project.id === live.project_id).settings.allowed_origins, ["https://s3-customer.example"]);
  assert.equal((await fetch(`${app.url}/v1/project/keys`, { headers: { authorization: `Bearer ${oldSecret}` } })).status, 401);
  assert.equal((await app.get("/v1/events", dashboard.key)).events.length, records.length + 1);
  const next = await app.get(`/v1/events?limit=100&cursor=${encodeURIComponent(first.next_cursor)}`);
  assert.equal(next.events.length, records.length);
  await app.send(records[0]);
  assert.equal((await app.get("/v1/events")).events.length, records.length + 2);
  liveRows = (await app.get("/v1/events", live.secret_key)).events;
  assert.equal(liveRows.length, records.length);
  await assert.rejects(stat(join(app.directory(), "state/analytics.duckdb")), { code: "ENOENT" });
});

test("S3 uses the AWS credential chain when explicit storage keys are omitted", {
  skip: !endpoint && "Set COLLECTOR_S3_TEST_ENDPOINT for an S3-compatible test bucket",
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
  skip: !endpoint && "Set COLLECTOR_S3_TEST_ENDPOINT for an S3-compatible test bucket",
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
