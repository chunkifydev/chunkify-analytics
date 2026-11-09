import { sdkDist } from "./sdk-path.mjs";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { chmod, cp, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createServer, request as httpRequest } from "node:http";
import { setTimeout as delay } from "node:timers/promises";
import { join, resolve } from "node:path";
import test from "node:test";
import { chromium } from "@playwright/test";
import { attachAnalytics } from "@chunkify/player-analytics";
import { createHttpAdapter } from "@chunkify/player-analytics/http";
import { component, assertRequest, assertResponse } from "./openapi-support.mjs";
import { collectorEnvironment, writeConfig } from "./configuration.mjs";

const binary = process.env.CHUNKIFY_BINARY ?? resolve("target/debug/chunkify-analytics");

function postEvent(server, body, headers = {}) {
  return fetch(`${server.url}/v1/events`, { method: "POST", body,
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key, ...headers },
  });
}

async function runCommand(args, env) {
  const configuration = await mkdtemp(join(tmpdir(), "chunkify-command-config-"));
  const config = await writeConfig(configuration, env);
  const child = spawn(binary, ["--config", config, ...args], { env: collectorEnvironment(env), cwd: tmpdir(), stdio: "pipe", timeout: 10000, killSignal: "SIGKILL" });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const [code] = await once(child, "close");
  await rm(configuration, { recursive: true, force: true });
  return { code, stdout, stderr };
}

async function collector(t, settings = {}) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-collector-test-"));
  let server;
  t.after(async () => { await server?.stop(); await rm(directory, { recursive: true, force: true }); });
  const env = { ...process.env, CHUNKIFY_ADMIN_KEY: undefined,
    "rate_limits.read_per_minute": "1000000", "rate_limits.read_burst": "1000000",
    "rate_limits.read_concurrent_per_project": "32", "rate_limits.read_concurrent": "32",
    "data_dir": directory, "server.bind": "127.0.0.1:0",
    "server.allowed_origins": "https://player.example",
    "server.trusted_proxies": "", ...settings };
  await writeConfig(directory, env);
  const init = spawn(binary, ["init"], { env: collectorEnvironment(env), cwd: directory, stdio: "pipe" });
  let errors = "";
  let initialized = "";
  init.stdout.on("data", chunk => { initialized += chunk; });
  init.stderr.on("data", (chunk) => { errors += chunk; });
  assert.equal((await once(init, "close"))[0], 0, errors);
  const {public_key, secret_key} = JSON.parse(initialized);
  const keys = {public_key, secret_key};
  let child;
  let exited;
  server = {
    directory,
    keys,
    async command(args, overrides = {}) {
      const result = await runCommand(args, { ...env, ...overrides });
      if (result.code === 0 && args[0] === "key" && args[1] === "rotate-primary") {
        keys[`${args[2]}_key`] = JSON.parse(result.stdout).key;
      }
      return result;
    },
    async assertStoredKeyHashes() {
      const stored = JSON.parse(await readFile(join(directory, "keys.json"), "utf8"));
      assert.equal(stored.public_key, undefined);
      assert.equal(stored.secret_key, undefined);
      const primary = stored.projects.find(project => project.id === "default");
      for (const kind of ["public", "secret"]) {
        const credential = primary.keys.find(key => key.id === `primary-${kind}`);
        if (credential) assert.equal(credential.hash, createHash("sha256").update(keys[`${kind}_key`]).digest("hex"));
      }
    },
    async start(executable = binary, overrides = {}) {
      await writeConfig(directory, { ...env, ...overrides });
      child = spawn(executable, ["serve"], { cwd: directory, env: collectorEnvironment({ ...env, ...overrides }), stdio: "pipe" });
      // Drain stdout/stderr as well as waiting for the process to exit.
      exited = once(child, "close");
      let output = "";
      errors = "";
      server.logs = () => `${output}${errors}`;
      child.stderr.on("data", (chunk) => { errors += chunk; });
      server.url = await new Promise((resolveReady, reject) => {
        const timeout = setTimeout(() => reject(new Error(`Collector startup timed out: ${errors}`)), 10000);
        child.once("error", reject);
        child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Collector exited: ${errors}`)); });
        child.stdout.on("data", (chunk) => {
          output += chunk;
          const match = output.match(/Listening on (http:\/\/\S+)/);
          if (match) { server.output = output; clearTimeout(timeout); resolveReady(match[1]); }
        });
      });
    },
    async stop(signal = "SIGTERM") {
      if (!child) return;
      if (child.exitCode !== null || child.signalCode !== null) { await exited; return; }
      child.kill(signal);
      const [code, receivedSignal] = await exited;
      if (signal === "SIGTERM") assert.equal(code, 0, `${receivedSignal}: ${errors}`);
    },
    async get(path, token = keys.secret_key) {
      const response = await fetch(`${server.url}${path}`, { headers: { Authorization: `Bearer ${token}` } });
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    },
    async send(event, headers = {}) {
      await createHttpAdapter(`${server.url}/v1/events`, { headers: { "X-Chunkify-Ingest-Key": keys.public_key, ...headers } }).send(event);
    },

  };
  await server.start();
  return server;
}

function profiles(server) {
  return server.logs().split("\n").filter((line) => line.startsWith('{"type":"collector_profile"')).map((line) => JSON.parse(line));
}

function writerProfile(server) {
  return profiles(server).find((profile) => profile.worker === "writer");
}

function sdkEvents(videoId = "video-a", viewerId = "viewer-a") {
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const events = [];
  const session = attachAnalytics(media, { videoId, viewerId: viewerId ?? undefined,
    destinations: [{ send(event) { events.push(event); } }] });
  session.startAttempt();
  session.destroy();
  return events;
}

function sdkPlayback(t) {
  let now = 0;
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: Date.parse("2026-10-15T12:00:00Z") });
  const clock = t.mock.method(performance, "now", () => now);
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const events = [];
  let changeRendition;
  const session = attachAnalytics(media, { videoId: "video-a", viewerId: "viewer-a", snapshotIntervalMs: 1000,
    destinations: [{ send(event) { events.push(event); } }] });
  try {
    session.setConnector({ name: "test-engine", version: "1", playerName: null, playerVersion: null,
      snapshot: () => null, subscribeToErrors: () => () => {},
      subscribeToRenditions(listener) { changeRendition = listener; return () => {}; },
    });
    changeRendition({ width: 640, height: 360, codec: "avc1", bitrate_bps: 500000, frame_rate: 30 });
    session.startAttempt();
    media.dispatchEvent(new Event("playing"));
    now = 1000;
    t.mock.timers.tick(1000);
    changeRendition({ width: 1280, height: 720, codec: "avc1", bitrate_bps: 1000000, frame_rate: 30 });
    media.dispatchEvent(new Event("seeking"));
    media.currentTime = 30;
    media.dispatchEvent(new Event("seeked"));
    media.dispatchEvent(new Event("playing"));
    now = 1400;
    t.mock.timers.tick(400);
    media.error = { code: 2 };
    media.dispatchEvent(new Event("error"));
    session.destroy();
    return events;
  } finally {
    session.destroy();
    clock.mock.restore();
    t.mock.timers.reset();
  }
}

test("real SDK events survive a collector restart and duplicate delivery", async (t) => {
  const server = await collector(t);
  const events = sdkEvents();
  for (const event of events) await server.send(event);
  await server.send(events[0]);
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), events);
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), events);
  assert.equal((await fetch(`${server.url}/v1/events`)).status, 401);
});

test("batch logging works live without profiling and respects the configured level", async (t) => {
  const server = await collector(t, { "retention.seconds": "0" });
  const [event] = sdkEvents("private-log-video", "private-log-viewer");
  await server.send(event);
  await server.send(event);
  // stderr is a separate pipe from the HTTP response; wait for it to drain.
  for (let attempt = 0; attempt < 100 && server.logs().split("batch_saved").length < 3; attempt++) await delay(10);
  const lines = server.logs().split("\n").filter(line => line.includes("batch_saved"));
  assert.equal(lines.length, 2);
  assert.match(lines[0], /INFO batch_saved backend="duckdb" requests=1 received=1 inserted=1 duplicates=0/);
  assert.match(lines[1], /inserted=0 duplicates=1/);
  assert.ok(!server.logs().includes("batch_started"));
  for (const secret of [server.keys.public_key, server.keys.secret_key, event.event_id, "private-log-video", "private-log-viewer"]) {
    assert.ok(!server.logs().includes(secret), "batch logs must not expose credentials or event values");
  }
  await server.stop();
  await server.start(binary, { "logging.level": "warn" });
  await server.send(event);
  await server.stop();
  assert.ok(!server.logs().includes("batch_saved") && !server.logs().includes("batch_started"));
  await server.start(binary, { "logging.level": "debug" });
  await server.send(event);
  await server.stop();
  assert.match(server.logs(), /DEBUG batch_started/);
  assert.match(server.logs(), /INFO batch_saved/);
  for (const secret of [server.keys.public_key, server.keys.secret_key, event.event_id, "private-log-video", "private-log-viewer"]) {
    assert.ok(!server.logs().includes(secret));
  }
  await server.start(binary, { "logging.level": "off" });
  await server.send(event);
  await server.stop();
  assert.ok(!server.logs().includes("batch_saved") && !server.logs().includes("batch_started"));
});

test("optional profiling reports database work on shutdown without event data or credentials", async (t) => {
  const server = await collector(t, { "profile": "true", "retention.seconds": "0", "batch.insert_mode": "row" });
  const events = sdkEvents("private-video", "private-viewer");
  await server.send(events[0]);
  await server.send(events[0]);
  await server.send(events[1]);
  await server.get("/v1/events");
  await server.get("/v1/summary");
  await server.get("/v1/videos");
  await server.stop();
  const profile = writerProfile(server);
  assert.ok(profile, "profiling must emit a structured shutdown report");
  const readProfile = profiles(server).find((profile) => profile.worker === "reader") ?? profile;
  assert.equal(profile.version, 2);
  assert.equal(profile.ingest.transactions, 3);
  assert.equal(profile.ingest.requests, 3);
  assert.equal(profile.ingest.received_records, 3);
  assert.equal(profile.ingest.inserted_records, 2);
  assert.equal(profile.ingest.max_records, 1);
  assert.ok(profile.ingest.total_bytes >= profile.ingest.max_bytes);
  assert.equal(profile.jobs.ingest.execution.count, 3);
  assert.equal(profile.jobs.ingest.queue_wait.count, 3);
  for (const name of ["events", "summary", "videos"]) assert.equal(readProfile.jobs[name].execution.count, 1);
  for (const name of ["begin", "prepare_request", "insert_event", "commit"]) {
    assert.equal(profile.ingest_stages[name].count, 3);
    assert.ok(profile.ingest_stages[name].total_ms >= profile.ingest_stages[name].max_ms);
  }
  assert.equal(profile.ingest_stages.insert_otlp, undefined);
  for (const secret of [server.keys.public_key, server.keys.secret_key, "private-video", "private-viewer", events[0].event_id, "127.0.0.1"]) {
    assert.ok(!JSON.stringify(profiles(server)).includes(secret), "profiles must contain only aggregate measurements");
  }
  await server.start(binary, { "profile": "false" });
  assert.equal((await server.get("/v1/events")).events.length, 2);
  await server.stop();
  assert.ok(!server.logs().includes('"collector_profile"'));
});

test("checkpoint configuration is applied and acknowledged events survive a crash", async (t) => {
  const server = await collector(t, { "profile": "true", "duckdb.checkpoint_threshold_mib": "64" });
  const events = sdkEvents("checkpoint");
  await server.send(events[0]);
  await server.send(events.at(-1));
  const records = (await server.get("/v1/events")).events;
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events, records);
  await server.stop();
  const profile = writerProfile(server);
  assert.equal(profile.database?.checkpoint_threshold, "64.0 MiB");
  assert.ok(profile.database.threads > 0);
  assert.ok(profile.database.version.startsWith("v"));
  for (const value of ["0", "1025", "no", "1; DROP TABLE events"]) {
    const result = await server.command(["serve"], { "duckdb.checkpoint_threshold_mib": value });
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /duckdb.checkpoint_threshold_mib|Invalid TOML configuration/);
  }
});

test("concurrent event requests share bounded transactions and preserve their data", async (t) => {
  const server = await collector(t, { "profile": "true", "retention.seconds": "0",
    "batch.max_events": "8", "batch.delay_ms": "100", "batch.insert_mode": "row" });
  const events = Array.from({ length: 12 }, (_, index) => sdkEvents(`batch-${index}`)[0]);
  await Promise.all(events.map(event => server.send(event)));
  const records = (await server.get("/v1/events")).events;
  assert.deepEqual(new Set(records.map((r) => r.event.event_id)), new Set(events.map((e) => e.event_id)));
  for (const record of records) {
    const index = events.findIndex((event) => event.event_id === record.event.event_id);
    assert.deepEqual(record.event, events[index]);
    assert.equal(record.otlp, undefined);
    assert.deepEqual(record.event_context, { schema_version: 1, continent: null, country: null, city: null });
  }
  await server.stop();
  const profile = writerProfile(server);
  assert.ok(profile.ingest.transactions < 12, "concurrent requests must share transactions");
  assert.equal(profile.ingest.requests, 12);
  assert.equal(profile.ingest.inserted_records, 12);
  assert.ok(profile.ingest.max_records > 1 && profile.ingest.max_records <= 8);
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events, records);
});

test("a lone event flushes by the collection deadline and keeps its earlier receipt time", async (t) => {
  const server = await collector(t, { "batch.delay_ms": "250", "retention.seconds": "0" });
  const before = Date.now();
  await server.send(sdkEvents()[0]);
  const responded = Date.now();
  const [record] = (await server.get("/v1/events")).events;
  assert.ok(Date.parse(record.received_at) >= before);
  assert.ok(responded - Date.parse(record.received_at) >= 200, "receipt time must precede the collection wait");
  assert.ok(responded - before < 3000, "an incomplete batch must flush without another request");
});

test("oversized write requests are rejected atomically and a full byte budget recovers", async (t) => {
  const server = await collector(t, { "batch.max_bytes": "16384", "batch.queue_max_bytes": "16384",
    "batch.delay_ms": "250", "batch.max_events": "2", "server.ingest_per_second": "1000" });
  const events = Array.from({ length: 3 }, (_, i) => sdkEvents(`limits-${i}`)[0]);
  assert.equal((await postEvent(server, JSON.stringify(events))).status, 400);
  assert.equal((await server.get("/v1/events")).events.length, 0);
  const post = (event) => fetch(`${server.url}/v1/events`, { method: "POST", body: JSON.stringify(event),
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key } });
  assert.equal((await post({ ...events[0], extra: "x".repeat(20000) })).status, 413);
  const responses = await Promise.all(Array.from({ length: 12 }, (_, i) => post({ ...sdkEvents(`budget-${i}`)[0], extra: "x".repeat(4000) })));
  assert.ok(responses.some((r) => r.status === 503), "outstanding bytes must be bounded");
  assert.ok(responses.some((r) => r.status === 201));
  assert.ok(responses.every((r) => [201, 503].includes(r.status)));
  const accepted = responses.filter((r) => r.status === 201).length;
  assert.equal((await server.get("/v1/events")).events.length, accepted);
  assert.equal((await post(events[0])).status, 201, "completed and rejected requests must release their budget");
});

test("bulk writes keep the first event and return per-request duplicate results", async (t) => {
  const server = await collector(t, { "batch.insert_mode": "bulk", "profile": "true", "batch.delay_ms": "100" });
  const playback = sdkEvents("bulk");
  const events = [playback[0], playback.at(-1)];
  for (const event of events) await server.send(event);
  const responses = await Promise.all(Array.from({ length: 6 }, () => fetch(`${server.url}/v1/events`, {
    method: "POST", body: JSON.stringify({ ...events[0], replaced: true }),
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key },
  })));
  for (const response of responses) {
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), { accepted: false, duplicate: true });
  }
  const records = (await server.get("/v1/events")).events;
  assert.deepEqual(records.map((r) => r.event), events);
  assert.ok(records.every(record => record.otlp === undefined));
  await server.stop();
  const profile = writerProfile(server);
  assert.ok(profile.ingest_stages.append_events?.count > 0, "bulk writes must use the measured bulk path");
  assert.equal(profile.ingest.inserted_records, 2);
  assert.equal(profile.ingest.requests, 8);
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events, records);
  await server.send(sdkEvents("after-bulk")[0]);
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await server.get("/v1/events")).events.length, 3);
});

test("a failed batch commit never acknowledges or retains any of its requests", { skip: process.platform === "win32" || process.getuid?.() === 0 }, async (t) => {
  const server = await collector(t, { "retention.seconds": "0", "batch.delay_ms": "200" });
  // A clean checkpoint removes the WAL. Prevent its recreation to exercise a
  // real commit failure through the deployment filesystem, without DB access.
  await server.stop();
  await server.start();
  assert.ok(!(await readdir(server.directory)).some((name) => name.endsWith(".wal")));
  const events = Array.from({ length: 8 }, (_, i) => sdkEvents(`failed-${i}`)[0]);
  await chmod(server.directory, 0o500);
  try {
    const responses = await Promise.all(events.map((event) => fetch(`${server.url}/v1/events`, { method: "POST", body: JSON.stringify(event),
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key } })));
    assert.ok(responses.every((r) => r.status === 500), `failed writes returned ${responses.map((r) => r.status)}`);
  } finally { await chmod(server.directory, 0o700); }
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await server.get("/v1/events")).events.length, 0);
  await server.send(events[0]);
  assert.equal((await server.get("/v1/events")).events.length, 1);
});

test("zero collection delay commits each event request separately", async (t) => {
  const server = await collector(t, { "batch.delay_ms": "0", "profile": "true" });
  await Promise.all(Array.from({ length: 12 }, (_, i) => server.send(sdkEvents(`zero-${i}`)[0])));
  await server.stop();
  const profile = writerProfile(server);
  assert.equal(profile.ingest.requests, 12);
  assert.equal(profile.ingest.transactions, 12);
  assert.equal(profile.ingest.max_requests, 1);
  assert.equal(profile.ingest.inserted_records, 12);
});

test("shared-worker reads flush pending writes and graceful shutdown drains an incomplete batch", async (t) => {
  const server = await collector(t, { "batch.delay_ms": "1000", "retention.seconds": "0",
    "experimental.read_worker": "false" });
  let acknowledged = false;
  const writing = server.send(sdkEvents("read-barrier")[0]).then(() => { acknowledged = true; });
  await delay(100);
  assert.equal(acknowledged, false, "write should still be waiting for the collection deadline");
  const before = Date.now();
  assert.equal((await server.get("/v1/events")).events.length, 1);
  await writing;
  assert.ok(Date.now() - before < 800, "a queued read should flush the batch before its one-second deadline");
  const events = Array.from({ length: 6 }, (_, i) => sdkEvents(`shutdown-${i}`)[0]);
  const pending = Promise.all(events.map(event => server.send(event)));
  await delay(100);
  await Promise.all([server.stop(), pending]);
  await server.start();
  assert.equal((await server.get("/v1/events")).events.length, 7);
});

test("separate reads return committed data without flushing unacknowledged writes", async (t) => {
  const server = await collector(t, { "batch.delay_ms": "1000", "retention.seconds": "0",
    "experimental.read_worker": "true" });
  let acknowledged = false;
  const writing = server.send(sdkEvents("independent-read")[0]).then(() => { acknowledged = true; });
  await delay(100);
  assert.equal(acknowledged, false);
  const before = Date.now();
  assert.equal((await server.get("/v1/events")).events.length, 0);
  assert.ok(Date.now() - before < 800, "read must not wait for the writer's collection deadline");
  assert.equal(acknowledged, false, "reading must not force an early write commit");
  await writing;
  assert.equal((await server.get("/v1/events")).events.length, 1, "a read after direct acknowledgement sees the commit");
});

test("a concurrent native duplicate has exactly one acknowledged owner", async (t) => {
  const server = await collector(t, { "batch.delay_ms": "100" });
  const event = sdkEvents()[0];
  const responses = await Promise.all(Array.from({ length: 12 }, (_, i) => fetch(`${server.url}/v1/events`, {
    method: "POST", body: JSON.stringify({ ...event, delivery: i }),
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key },
  })));
  assert.equal(responses.filter((r) => r.status === 201).length, 1);
  assert.equal(responses.filter((r) => r.status === 200).length, 11);
  const owner = responses.findIndex((r) => r.status === 201);
  assert.deepEqual((await server.get("/v1/events")).events.map((r) => r.event), [{ ...event, delivery: owner }]);
});

test("invalid batch settings fail startup with the setting name", async (t) => {
  const server = await collector(t);
  await server.stop();
  for (const [name, value] of Object.entries({ "batch.max_events": "0", "batch.delay_ms": "not-a-number",
    "batch.queue_max_bytes": "100", "batch.queue_max_requests": "0", "batch.insert_mode": "unknown",
    "experimental.read_worker": "invalid" })) {
    const result = await server.command(["serve"], { [name]: value });
    assert.notEqual(result.code, 0);
    assert.ok((result.stderr.includes(name) || result.stderr.includes("Invalid TOML configuration")), result.stderr);
  }
});

test("local geolocation ignores forged context and untrusted proxy headers", async (t) => {
  const server = await collector(t);
  const event = { ...sdkEvents()[0], event_context: { country: "FR", city: "Paris" } };
  await server.send(event, { "X-Forwarded-For": "81.2.69.160", "X-Real-IP": "81.2.69.160",
    "Forwarded": "for=81.2.69.160", "CF-Connecting-IP": "81.2.69.160" });
  const stored = await server.get("/v1/events");
  assert.deepEqual(stored.events[0].event, event);
  assert.deepEqual(stored.events[0].event_context, { schema_version: 1, continent: null, country: null, city: null });
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), stored);
});

test("flat context combines browser fields with collector-owned country and city", async (t) => {
  const server = await collector(t, { "server.trusted_proxies": "127.0.0.1" });
  const event = { ...sdkEvents()[0], browser_context: {
    schema_version: 1, browser_name: "Firefox", os_name: "Linux", device_type: "desktop",
    viewport_width_px: 960, page_origin: "https://example.com", page_path: "/watch/first", country: "FR", city: "Paris", browser_version: { invalid: "nested" },
  } };
  await server.send(event, { "X-Forwarded-For": "81.2.69.160" });
  const stored = (await server.get("/v1/events")).events[0];
  assert.deepEqual(stored.event, event);
  assert.deepEqual(stored.event_context, {
    schema_version: 1, browser_name: "Firefox", os_name: "Linux", device_type: "desktop", viewport_width_px: 960,
    page_origin: "https://example.com", page_path: "/watch/first", continent: "EU", country: "GB", city: "London",
  });
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events[0], stored);
});

test("trusted proxy lookups persist location but no request IP, including retries and CSV", async (t) => {
  const server = await collector(t, { "server.trusted_proxies": "127.0.0.1,10.20.0.0/16,2001:db8:abcd::/48" });
  const event = sdkEvents()[0];
  await server.send(event, { "X-Forwarded-For": "216.160.83.56, 81.2.69.160, 10.20.1.2, 2001:db8:abcd::1" });
  const stored = (await server.get("/v1/events")).events;
  assert.deepEqual(stored[0].event, event);
  assert.deepEqual(stored[0].event_context, { schema_version: 1, continent: "EU", country: "GB", city: "London" });
  await server.send(event, { "X-Forwarded-For": "216.160.83.56" });
  assert.deepEqual((await server.get("/v1/events")).events, stored, "a retry cannot replace the first location");
  const grant = await fetch(`${server.url}/v1/exports`, { method: "POST", headers: {
    Authorization: `Bearer ${server.keys.secret_key}`, "Content-Type": "application/json",
  }, body: JSON.stringify({ resource: "events", query: {} }) });
  assert.equal(grant.status, 201, await grant.clone().text());
  const csv = await (await fetch((await grant.json()).download_url)).text();
  assert.match(csv, /context.country/); assert.match(csv, /context.city/); assert.match(csv, /London/);
  await server.stop();
  const database = await readFile(join(server.directory, "analytics.duckdb"));
  for (const ip of ["216.160.83.56", "81.2.69.160", "10.20.1.2", "2001:db8:abcd::1"]) {
    assert.ok(!JSON.stringify(stored).includes(ip));
    assert.ok(!csv.includes(ip)); assert.ok(!server.logs().includes(ip)); assert.ok(!database.includes(Buffer.from(ip)));
  }
  for (const field of ["peer_ip", "client_ip", "client_ip_source"]) assert.ok(!csv.includes(field));
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events, stored);
});

test("missing, invalid, private and unknown forwarded addresses have unknown locations", async (t) => {
  const server = await collector(t, { "server.trusted_proxies": "127.0.0.1,10.20.0.0/16" });
  for (const forwarded of [undefined, "", "unknown", "for=81.2.69.160", "81.2.69.160:443", "malformed, 81.2.69.160",
    "81.2.69.160,", "10.1.2.3", "1.1.1.1", "127.0.0.1, 10.20.1.2",
    ["81.2.69.160", ...Array(32).fill("10.20.1.2")].join(","), `81.2.69.160,${" ".repeat(4096)}10.20.1.2`]) {
    await server.send(sdkEvents()[0], forwarded === undefined ? {} : { "X-Forwarded-For": forwarded });
    assert.deepEqual((await server.get("/v1/events")).events.at(-1).event_context,
      { schema_version: 1, continent: null, country: null, city: null });
  }
});

test("multiple X-Forwarded-For header lines form one ordered chain", async (t) => {
  const server = await collector(t, { "server.trusted_proxies": "127.0.0.1,10.20.0.0/16" });
  const body = JSON.stringify(sdkEvents()[0]);
  const status = await new Promise((resolveStatus, reject) => {
    const request = httpRequest(`${server.url}/v1/events`, { method: "POST", headers: [
      "Content-Type", "application/json", "Content-Length", String(Buffer.byteLength(body)),
      "X-Chunkify-Ingest-Key", server.keys.public_key,
      "X-Forwarded-For", "216.160.83.56, 81.2.69.160", "X-Forwarded-For", "10.20.1.2",
    ] }, (response) => { response.resume(); response.on("end", () => resolveStatus(response.statusCode)); });
    request.on("error", reject);
    request.end(body);
  });
  assert.equal(status, 201);
  assert.deepEqual((await server.get("/v1/events")).events[0].event_context,
    { schema_version: 1, continent: "EU", country: "GB", city: "London" });
});

test("IPv6 and mapped IPv4 addresses are located through explicitly trusted proxies", async (t) => {
  const server = await collector(t, { "server.bind": "[::1]:0" });
  await server.send(sdkEvents()[0], { "X-Forwarded-For": "2001:218::1" });
  assert.deepEqual((await server.get("/v1/events")).events[0].event_context,
    { schema_version: 1, continent: null, country: null, city: null });
  await server.stop();
  await server.start(binary, { "server.trusted_proxies": "::1/128" });
  for (const [ip, continent, country, city] of [["2001:218::1", "AS", "JP", null], ["::ffff:81.2.69.160", "EU", "GB", "London"]]) {
    await server.send(sdkEvents()[0], { "X-Forwarded-For": ip });
    assert.deepEqual((await server.get("/v1/events")).events.at(-1).event_context,
      { schema_version: 1, continent, country, city });
  }
});

test("invalid proxy or geolocation configuration fails before touching events or credentials", async (t) => {
  const server = await collector(t);
  await server.send(sdkEvents()[0]);
  const before = await server.get("/v1/events");
  await server.stop();
  for (const settings of [
    { "server.store_ip": true }, { "server.store_ip": false },
    ...["*", "localhost", "127.0.0.1:80", "127.0.0.1/33", "::1/129", "127.0.0.1,", "::ffff:127.0.0.0/104"]
      .map((value) => ({ "server.trusted_proxies": value })),
    ...["missing.mmdb", resolve("Cargo.toml"), resolve("tests/fixtures/GeoIP2-Country-Test.mmdb")]
      .map((value) => ({ "server.geoip_database": value })),
  ]) {
    const result = await server.command(["serve"], settings);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /trusted_proxies|Invalid TOML configuration|geolocation|Geolocation/);
  }
  await server.assertStoredKeyHashes();
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), before);
});

test("the browser Chunkify adapter persists context through authenticated ingestion, retries, and restart", async (t) => {
  const fixture = createServer(async (request, response) => {
    if (request.url === "/") {
      response.writeHead(200, { "Content-Type": "text/html" }).end("<!doctype html><video></video>");
    } else if (/^\/[a-z-]+\.js$/.test(request.url ?? "")) {
      try {
        const source = await readFile(resolve(sdkDist, request.url.slice(1)));
        response.writeHead(200, { "Content-Type": "text/javascript" }).end(source);
      } catch { response.writeHead(404).end(); }
    } else { response.writeHead(404).end(); }
  });
  let browser;
  t.after(async () => { await browser?.close(); await new Promise((done) => fixture.close(done)); });
  fixture.listen(0, "127.0.0.1");
  await once(fixture, "listening");
  const origin = `http://127.0.0.1:${fixture.address().port}`;
  const server = await collector(t, { "server.allowed_origins": origin });
  browser = await chromium.launch();
  const page = await browser.newPage({ locale: "fr-FR", timezoneId: "Europe/Paris", viewport: { width: 960, height: 540 } });
  await page.goto(origin);
  const destination = { endpoint: `${server.url}/v1/events`, ingestKey: server.keys.public_key };
  const events = await page.evaluate(async ({ endpoint, ingestKey }) => {
    const { attachAnalytics } = await import("/core.js");
    const { createChunkifyAdapter } = await import("/chunkify.js");
    const adapter = createChunkifyAdapter(endpoint, { ingestKey });
    const events = [];
    const pending = [];
    const media = document.querySelector("video");
    const session = attachAnalytics(media, { videoId: "browser-video", viewerId: "browser-viewer",
      includeEvents: ["view_started", "view_ended"], destinations: [
        { send(event) { pending.push(adapter.send(event)); } },
        { send(event) { events.push(event); } },
      ],
    });
    session.startAttempt();
    media.dispatchEvent(new Event("playing"));
    await new Promise((done) => setTimeout(done, 30));
    session.destroy();
    await Promise.all(pending);
    return events;
  }, destination);
  const stored = await server.get("/v1/events");
  assert.equal(stored.events.length, 2);
  assert.deepEqual(stored.events.map(({ event: { browser_context, ...event } }) => event), events);
  t.diagnostic(`Browser context added bytes per event: ${stored.events.map(({ event }, index) =>
    Buffer.byteLength(JSON.stringify(event)) - Buffer.byteLength(JSON.stringify(events[index]))).join(", ")}`);
  for (const { event, event_context } of stored.events) {
    assert.equal(event.browser_context.browser_name, "Chrome");
    assert.equal(event.browser_context.language, "fr-FR");
    assert.equal(event.browser_context.timezone, "Europe/Paris");
    assert.equal(event.browser_context.viewport_width_px, 960);
    assert.deepEqual(event_context, { ...event.browser_context, continent: null, country: null, city: null });
  }
  assert.ok(events[1].playing_time_ms > 0);
  assert.deepEqual(await server.get("/v1/summary"), {
    views: 1, unique_viewers: 1, total_playing_time_ms: events[1].playing_time_ms,
  });
  await page.setViewportSize({ width: 640, height: 360 });
  await page.evaluate(async ({ endpoint, ingestKey, event }) => {
    const { createChunkifyAdapter } = await import("/chunkify.js");
    await createChunkifyAdapter(endpoint, { ingestKey }).send(event);
  }, { ...destination, event: events[0] });
  assert.deepEqual(await server.get("/v1/events"), stored, "retry must preserve the first accepted context");
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), stored);
});

test("retention expires events by receipt time and updates summaries", async (t) => {
  const server = await collector(t, { "retention.seconds": "2", "retention.interval_seconds": "1" });
  const past = { ...sdkEvents("past-video")[0], occurred_at: "2000-01-01T00:00:00Z" };
  const future = { ...sdkEvents("future-video")[0], occurred_at: "2099-01-01T00:00:00Z" };
  await server.send(past);
  await server.send(future);
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), [past, future]);
  assert.equal((await server.get("/v1/summary")).views, 2);
  const deadline = Date.now() + 7000;
  while ((await server.get("/v1/events")).events.length && Date.now() < deadline) await delay(200);
  assert.deepEqual((await server.get("/v1/events")).events, []);
  assert.deepEqual(await server.get("/v1/summary"), { views: 0, unique_viewers: 0, total_playing_time_ms: 0 });
  assert.deepEqual((await server.get("/v1/videos")).videos, []);
  // Expired event IDs can be received again, with a new retention lifetime.
  await server.send(future);
  assert.equal((await server.get("/v1/events")).events.length, 1);
  await server.stop("SIGKILL");
  await server.start(binary, { "retention.seconds": "0" });
  assert.equal((await server.get("/v1/summary")).views, 1);
});

test("retention defaults to 30 days, can be disabled, and purges at startup", async (t) => {
  const server = await collector(t);
  assert.match(server.output, /Retention: 2592000 seconds/);
  const event = { ...sdkEvents()[0], occurred_at: "2000-01-01T00:00:00Z" };
  await server.send(event);
  await server.stop();
  await delay(1200);
  await server.start(binary, { "retention.seconds": "0", "retention.interval_seconds": "1" });
  assert.match(server.output, /Retention: disabled/);
  await delay(1200);
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), [event]);
  await server.stop();
  await server.start(binary, { "retention.seconds": "1" });
  assert.deepEqual((await server.get("/v1/events")).events, []);
  assert.equal((await server.get("/v1/summary")).views, 0);
});

test("viewer deletion is not a supported command", async (t) => {
  const server = await collector(t);
  const result = await server.command(["viewer", "delete", "viewer"]);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /unrecognized subcommand/);
});

test("offline key rotation returns each credential once without persisting secrets or losing data", async (t) => {
  const server = await collector(t);
  const originalKeys = { ...server.keys };
  const event = sdkEvents()[0];
  await server.send(event);
  const original = await server.get("/v1/events");
  const busy = await server.command(["project", "rename", "default", "Renamed"]);
  assert.notEqual(busy.code, 0);
  assert.match(busy.stderr, /Stop the collector/);
  await server.assertStoredKeyHashes();
  assert.deepEqual(server.keys, originalKeys);
  await server.stop();
  const invalid = await server.command(["key", "rotate-primary", "unknown"]);
  assert.notEqual(invalid.code, 0);
  const ingest = await server.command(["key", "rotate-primary", "public"]);
  assert.equal(ingest.code, 0, ingest.stderr);
  await server.assertStoredKeyHashes();
  assert.notEqual(server.keys.public_key, originalKeys.public_key);
  assert.equal(server.keys.secret_key, originalKeys.secret_key);
  assert.ok(server.keys.public_key.length >= 32);
  for (const key of [...Object.values(originalKeys), ...Object.values(server.keys)]) {
    assert.equal((await readFile(join(server.directory, "keys.json"), "utf8")).includes(key), false);
    assert.equal(ingest.stderr.includes(key), false);
  }
  if (process.platform !== "win32") {
    assert.equal((await stat(join(server.directory, "keys.json"))).mode & 0o777, 0o600);
  }
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), original);
  const deniedWrite = await fetch(`${server.url}/v1/events`, { method: "POST",
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": originalKeys.public_key },
    body: JSON.stringify(event),
  });
  assert.equal(deniedWrite.status, 401);
  assert.equal((await postEvent(server, "{}", { "X-Chunkify-Ingest-Key": originalKeys.public_key })).status, 401);
  const rotatedIngest = server.keys.public_key;
  await server.send(sdkEvents("after-ingest-rotation")[0]);
  await server.stop("SIGKILL");
  const read = await server.command(["key", "rotate-primary", "secret"]);
  assert.equal(read.code, 0, read.stderr);
  await server.assertStoredKeyHashes();
  assert.notEqual(server.keys.secret_key, originalKeys.secret_key);
  assert.equal(server.keys.public_key, rotatedIngest);
  for (const key of [...Object.values(originalKeys), ...Object.values(server.keys)]) {
    assert.equal((await readFile(join(server.directory, "keys.json"), "utf8")).includes(key), false);
    assert.equal(read.stderr.includes(key), false);
  }
  await server.start();
  const deniedRead = await fetch(`${server.url}/v1/events`, {
    headers: { Authorization: `Bearer ${originalKeys.secret_key}` },
  });
  assert.equal(deniedRead.status, 401);
  await server.send(sdkEvents("after-read-rotation")[0]);
  assert.equal((await server.get("/v1/summary")).views, 3);
});

test("a stopped data-directory backup restores events, summaries and keys", async (t) => {
  const events = sdkPlayback(t);
  const server = await collector(t);
  const backups = await mkdtemp(join(tmpdir(), "chunkify-collector-backup-test-"));
  t.after(async () => { await server.stop(); await rm(backups, { recursive: true, force: true }); });
  for (const [index, event] of events.entries()) {
    await server.send(event);
  }
  const originalKeys = { ...server.keys };
  const originalEvents = await server.get("/v1/events");
  const originalSummary = await server.get("/v1/summary");
  await server.stop();
  await cp(server.directory, join(backups, "snapshot"), { recursive: true, force: false, errorOnExist: true });
  // Changes made after the backup must not appear in the restored installation.
  const rotation = await server.command(["key", "rotate-primary", "public"]);
  assert.equal(rotation.code, 0, rotation.stderr);
  await server.assertStoredKeyHashes();
  assert.notEqual(server.keys.public_key, originalKeys.public_key);
  await server.start();
  await server.send(sdkEvents("after-backup")[0]);
  await server.stop();
  const restored = join(backups, "restored");
  await cp(join(backups, "snapshot"), restored, { recursive: true, force: false, errorOnExist: true });
  const restoredKeys = JSON.parse(await readFile(join(restored, "keys.json"), "utf8"));
  assert.equal(restoredKeys.public_key, undefined);
  assert.equal(restoredKeys.secret_key, undefined);
  Object.assign(server.keys, originalKeys);
  await server.start(binary, { "data_dir": restored });
  assert.deepEqual(await server.get("/v1/events"), originalEvents);
  assert.deepEqual(await server.get("/v1/summary"), originalSummary);
  await server.send(events[0]);
  assert.deepEqual(await server.get("/v1/events"), originalEvents);
  await server.send(sdkEvents("after-restore")[0]);
  assert.equal((await server.get("/v1/summary")).views, 2);
});

test("invalid retention settings fail clearly without changing stored events or keys", async (t) => {
  const server = await collector(t);
  await server.send(sdkEvents()[0]);
  const before = await server.get("/v1/events");
  const keys = { ...server.keys };
  await server.stop();
  for (const settings of [
    { "retention.seconds": "-1" },
    { "retention.seconds": "1.5" },
    { "retention.seconds": "18446744073709551615" },
    { "retention.interval_seconds": "0" },
    { "retention.interval_seconds": "-1" },
    { "retention.interval_seconds": "86401" },
  ]) {
    const result = await server.command(["serve"], settings);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, new RegExp(`${Object.keys(settings)[0]}|Invalid TOML configuration`));
  }
  await server.assertStoredKeyHashes();
  assert.deepEqual(server.keys, keys);
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), before);
});

test("ingestion validates SDK payloads and separates browser writes from private reads", async (t) => {
  const server = await collector(t, { "server.max_body_bytes": "8192" });
  const event = sdkEvents()[0];
  const post = (body, headers = {}) => fetch(`${server.url}/v1/events`, {
    method: "POST", headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key, ...headers },
    body: JSON.stringify(body),
  });
  assert.equal((await post(event, { "X-Chunkify-Ingest-Key": server.keys.secret_key })).status, 401);
  assert.equal((await fetch(`${server.url}/v1/events`, {
    headers: { Authorization: `Bearer ${server.keys.public_key}` },
  })).status, 401);
  assert.equal((await post(event, { Origin: "https://untrusted.example" })).status, 403);
  const preflight = await fetch(`${server.url}/v1/events`, { method: "OPTIONS", headers: {
    Origin: "https://player.example", "Access-Control-Request-Method": "POST",
    "Access-Control-Request-Headers": "content-type,x-chunkify-ingest-key",
  } });
  assert.equal(preflight.status, 204);
  assert.equal(preflight.headers.get("access-control-allow-origin"), "https://player.example");
  for (const bad of [null, [], { ...event, schema_version: 2 }, { ...event, event_name: "unknown" },
    { ...event, event_id: "invalid" }, { ...event, sequence: -1 }, { ...event, viewer_id: 3 },
    { ...event, occurred_at: "yesterday" }, { ...event, playing_time_ms: -1 },
    { ...event, playing_time_ms: undefined }, { ...event, reason: "unknown" },
    { ...event, rendition_playing_time: [{ width: "bad" }] },
    { ...event, event_name: "chunkify.video.seek_completed" }]) {
    assert.equal((await post(bad)).status, 400, JSON.stringify(bad));
  }
  assert.equal((await post({ ...event, extra: "x".repeat(10000) })).status, 413);
  assert.deepEqual((await server.get("/v1/events")).events, []);
  const valid = await post({ ...event, extra: { customer_field: "preserved" } }, { Origin: "https://player.example" });
  assert.equal(valid.status, 201);
  assert.equal(valid.headers.get("access-control-allow-origin"), "https://player.example");
  assert.deepEqual((await server.get("/v1/events")).events[0].event.extra, { customer_field: "preserved" });
  const read = await fetch(`${server.url}/v1/events`, { headers: {
    Origin: "https://player.example", Authorization: `Bearer ${server.keys.secret_key}`,
  } });
  assert.equal(read.headers.get("access-control-allow-origin"), null);
});

test("optional user IDs and video titles are validated, filtered and reported", async (t) => {
  const server = await collector(t);
  const post = (body) => fetch(`${server.url}/v1/events`, {
    method: "POST", headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key },
    body: JSON.stringify(body),
  });
  const [start] = sdkEvents();
  for (const bad of [{ user_id: 3 }, { user_id: "" }, { user_id: "u".repeat(257) },
    { video_title: false }, { video_title: "" }, { video_title: "t".repeat(257) }]) {
    assert.equal((await post({ ...start, ...bad })).status, 400, JSON.stringify(bad));
  }
  const tag = (events, changes) => events.map((event) => ({ ...event, ...changes }));
  const first = tag(sdkEvents("video-a", "viewer-a"),
    { user_id: "user-1", video_title: "Old title", occurred_at: "2026-10-15T12:00:00Z" });
  // Limits count Unicode characters, not bytes.
  const latestTitle = "🎬".repeat(256);
  // Received last but with an earlier device clock: arrival order picks the title.
  const second = tag(sdkEvents("video-a", "viewer-b"),
    { user_id: "user-2", video_title: latestTitle, occurred_at: "2026-10-15T11:00:00Z" });
  const third = tag(sdkEvents("video-b", "viewer-c"), { occurred_at: "2026-10-15T14:00:00Z" });
  for (const event of [...first, ...second, ...third]) assert.equal((await post(event)).status, 201);

  assert.equal((await server.get("/v1/summary?user_id=user-1")).views, 1);
  const events = (await server.get("/v1/events?user_id=user-2")).events;
  assert.equal(events.length, second.length);
  assert.ok(events.every((record) => record.event.user_id === "user-2"));
  const videos = (await server.get("/v1/videos")).videos;
  assert.deepEqual(videos.map(({ video_id, video_title }) => ({ video_id, video_title })),
    [{ video_id: "video-a", video_title: latestTitle }, { video_id: "video-b", video_title: null }]);
  const [view] = (await server.get("/v1/views?user_id=user-1")).views;
  assert.equal(view.user_id, "user-1");
  assert.equal(view.video_title, "Old title");
  assert.deepEqual((await server.get("/v1/dimensions/user_id")).values,
    [{ value: null, views: 1 }, { value: "user-1", views: 1 }, { value: "user-2", views: 1 }]);
  for (const query of ["user_id=", `user_id=${"u".repeat(257)}`]) {
    const response = await fetch(`${server.url}/v1/summary?${query}`, {
      headers: { Authorization: `Bearer ${server.keys.secret_key}` },
    });
    assert.equal(response.status, 400, query);
  }
});

test("all seven SDK event types can be filtered and paged while new events arrive", async (t) => {
  const events = sdkPlayback(t);
  assert.equal(new Set(events.map((event) => event.event_name)).size, 7);
  const server = await collector(t);
  for (const event of events) await server.send(event);
  let page = await server.get("/v1/events?limit=2");
  assert.equal(page.events.length, 2);
  const firstCursor = page.next_cursor;
  assert.ok(firstCursor);
  const received = [...page.events];
  await server.send(sdkEvents("video-b", "viewer-b")[0]);
  while (page.next_cursor) {
    page = await server.get(`/v1/events?limit=2&cursor=${encodeURIComponent(page.next_cursor)}`);
    received.push(...page.events);
  }
  assert.deepEqual(received.map((record) => record.event), events);
  const filtered = await server.get("/v1/events?video_id=video-a&viewer_id=viewer-a&event_name=chunkify.video.view_snapshot&from=2026-10-15T12:00:01Z&to=2026-10-15T12:00:02Z");
  assert.equal(filtered.events.length, 1);
  assert.equal(filtered.events[0].event.playing_time_ms, 1000);
  assert.deepEqual((await server.get("/v1/events?viewer_id=absent")).events, []);
  for (const query of ["limit=0", "limit=1001", "cursor=invalid", "unknown=1",
    "from=invalid", "from=2026-10-16T00:00:00Z&to=2026-10-15T00:00:00Z",
    `video_id=video-b&cursor=${encodeURIComponent(firstCursor)}`]) {
    const response = await fetch(`${server.url}/v1/events?${query}`, {
      headers: { Authorization: `Bearer ${server.keys.secret_key}` },
    });
    assert.equal(response.status, 400, query);
  }
});

test("summaries count views and viewers and use final or latest cumulative playtime", async (t) => {
  const server = await collector(t);
  const first = sdkEvents();
  const second = sdkEvents();
  const third = sdkEvents("video-b", undefined).map((event) => ({ ...event, viewer_id: null }));
  const at = (event, occurred_at, changes = {}) => ({ ...event, occurred_at, ...changes });
  const snapshot = (event, sequence, playing_time_ms) => ({ ...event, event_id: crypto.randomUUID(),
    event_name: "chunkify.video.view_snapshot", sequence, playing_time_ms });
  await server.send(at(first[0], "2026-10-14T23:59:59Z"));
  const firstSnapshot = at(snapshot(first[0], 2, 1000), "2026-10-15T00:00:00Z");
  await server.send(firstSnapshot);
  await server.send(at(first.at(-1), "2026-10-15T00:00:03Z", { sequence: 4, playing_time_ms: 4000 }));
  await server.send(at(snapshot(first[0], 3, 3000), "2026-10-15T00:00:02Z"));
  await server.send(at(second[0], "2026-10-15T12:00:00Z"));
  await server.send(at(snapshot(second[0], 3, 2000), "2026-10-15T12:00:02Z"));
  await server.send(at(snapshot(second[0], 2, 500), "2026-10-15T12:00:01Z"));
  await server.send(at(third[0], "2026-10-15T12:00:00Z"));
  await server.send(at(third.at(-1), "2026-10-15T12:00:01Z", { playing_time_ms: 500 }));
  await server.send(firstSnapshot);
  // A repeated start with a different event ID still describes the same view.
  await server.send(at(first[0], "2026-10-14T23:59:59Z", { event_id: crypto.randomUUID() }));
  await server.send({ ...sdkEvents("orphan").at(-1), playing_time_ms: 100000 });
  assert.deepEqual(await server.get("/v1/summary"), { views: 3, unique_viewers: 1, total_playing_time_ms: 6500 });
  assert.deepEqual(await server.get("/v1/summary?viewer_id=viewer-a"), { views: 2, unique_viewers: 1, total_playing_time_ms: 6000 });
  assert.deepEqual(await server.get("/v1/summary?from=2026-10-14T00:00:00Z&to=2026-10-15T00:00:00Z"),
    { views: 1, unique_viewers: 1, total_playing_time_ms: 4000 });
  assert.deepEqual(await server.get("/v1/summary?from=2026-10-15T00:00:00Z&to=2026-10-16T00:00:00Z"),
    { views: 2, unique_viewers: 1, total_playing_time_ms: 2500 });
  assert.deepEqual(await server.get("/v1/summary?video_id=absent"), { views: 0, unique_viewers: 0, total_playing_time_ms: 0 });
  const videos = await server.get("/v1/videos?limit=1");
  assert.deepEqual(videos.videos, [{ video_id: "video-a", video_title: null, views: 2, unique_viewers: 1, total_playing_time_ms: 6000, playback_score: { value: null, sample_views: 0 } }]);
  assert.ok(videos.next_cursor);
  await server.send(at(second.at(-1), "2026-10-15T12:00:03Z", { sequence: 4, playing_time_ms: 2500 }));
  const next = await server.get(`/v1/videos?limit=1&cursor=${encodeURIComponent(videos.next_cursor)}`);
  assert.deepEqual(next.videos, [{ video_id: "video-b", video_title: null, views: 1, unique_viewers: 0, total_playing_time_ms: 500, playback_score: { value: null, sample_views: 0 } }]);
  assert.equal(next.next_cursor, null);
  assert.equal((await server.get("/v1/summary")).total_playing_time_ms, 7000);
  await server.stop();
  await server.start();
  assert.equal((await server.get("/v1/summary")).total_playing_time_ms, 7000);
});

test("ingestion rate limits reject excess requests without storing more events", async (t) => {
  const event = sdkEvents()[0];
  const server = await collector(t, { "server.ingest_per_second": "1" });
  const responses = await Promise.all(Array.from({ length: 8 }, () => fetch(`${server.url}/v1/events`, {
    method: "POST", headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key },
    body: JSON.stringify(event),
  })));
  assert.equal(responses.filter((response) => response.ok).length, 1);
  assert.equal(responses.filter((response) => response.status === 429).length, 7);
  assert.equal(responses.find((response) => response.status === 429).headers.get("retry-after"), "1");
  for (const response of responses.filter((response) => response.status === 429)) {
    assert.equal(typeof (await response.json()).error, "string");
  }
  assert.equal((await server.get("/v1/events")).events.length, 1);
  assert.deepEqual(await (await fetch(`${server.url}/healthz`)).json(), { status: "ok" });
});

test("separate reader serves committed SDK events with stable pagination", async (t) => {
  await t.test("direct", async (t) => {
    const server = await collector(t, { "experimental.read_worker": "true",
      "retention.seconds": "0", "profile": "true" });
    const events = Array.from({ length: 8 }, (_, index) => {
      const view = sdkEvents(`video-${index % 2}`, `viewer-${index % 3}`);
      view.at(-1).playing_time_ms = 1000;
      return view;
    }).flat();
    await Promise.all([
      Promise.all(events.map(event => server.send(event))),
      (async () => {
        for (let i = 0; i < 3; i++) {
          await Promise.all([server.get("/v1/summary"), server.get("/v1/videos"), server.get("/v1/events")]);
          await delay(5);
        }
      })(),
    ]);
    assert.deepEqual(await server.get("/v1/summary"), { views: 8, unique_viewers: 3, total_playing_time_ms: 8000 });
    assert.equal((await server.get("/v1/summary?video_id=video-0")).views, 4);
    const first = await server.get("/v1/events?limit=1");
    const extra = sdkEvents("after-first-page");
    for (const event of extra) await server.send(event);
    const records = [...first.events];
    let cursor = first.next_cursor;
    while (cursor) {
      const page = await server.get(`/v1/events?limit=5&cursor=${encodeURIComponent(cursor)}`);
      records.push(...page.events);
      cursor = page.next_cursor;
    }
    assert.deepEqual(new Set(records.map(({ event }) => event.event_id)), new Set(events.map((event) => event.event_id)), "later commits must stay outside the pagination watermark");
    for (const record of records) {
      assert.deepEqual(record.event, events.find((event) => event.event_id === record.event.event_id));
      assert.deepEqual(record.event_context, { schema_version: 1, continent: null, country: null, city: null });
    }
    await server.stop();
    const reader = profiles(server).find((profile) => profile.worker === "reader");
    assert.ok(reader, "read worker must report its own execution");
    assert.ok(reader.jobs.summary.execution.count > 0);
    assert.equal(writerProfile(server).jobs.summary, undefined, "dashboard queries must bypass the writer");
    await server.start();
    assert.equal((await server.get("/v1/events")).events.length, events.length + extra.length);
    assert.equal((await server.get("/v1/summary")).views, 9);
  });
});

async function projectCommand(server, args) {
  const result = await server.command(args);
  assert.equal(result.code, 0, result.stderr);
  return JSON.parse(result.stdout);
}

function projectPost(server, key, event, headers = {}) {
  return fetch(`${server.url}/v1/events`, { method: "POST", body: JSON.stringify(event),
    headers: { "content-type": "application/json", "x-chunkify-ingest-key": key, ...headers } });
}

for (const mode of ["row", "bulk"]) test(`projects isolate identical IDs and cursors (${mode})`, async (t) => {
  const server = await collector(t, { "retention.seconds": "0", "server.ingest_per_second": "1000",
    "batch.insert_mode": mode === "row" ? "row" : "bulk",
    "batch.delay_ms": "100" });
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "Production website"]);
  const second = await projectCommand(server, ["key", "create", live.project_id, "public", "Mobile app"]);
  const list = await projectCommand(server, ["project", "list"]);
  const firstKey = list.find(p => p.id === live.project_id).keys.find(k => k.permission === "public").id;
  assert.equal(list.find(p => p.id === live.project_id).name, "Production website");
  assert.ok(list.every(p => p.keys.every(k => !k.hash && !k.key)));
  const configuration = await readFile(join(server.directory, "keys.json"), "utf8");
  for (const secret of [live.public_key, live.secret_key, second.key]) assert.ok(!configuration.includes(secret));
  await server.start();
  const events = sdkEvents("shared-video", "shared-viewer");
  events.at(-1).playing_time_ms = 1200;
  // The same identifiers are independent across projects. Body properties never route storage.
  events[0].project_id = live.project_id;
  const liveEvents = structuredClone(events);
  liveEvents[0].project_id = "default";
  liveEvents.at(-1).playing_time_ms = 5400;
  await Promise.all([
    ...events.map(event => server.send(event)),
    ...liveEvents.map(event => server.send(event, { "X-Chunkify-Ingest-Key": live.public_key })),
  ]);
  // Retrying via another key must keep first delivery.
  await server.stop();
  await server.start();
  await server.send(liveEvents[0], { "X-Chunkify-Ingest-Key": second.key });
  await server.stop();
  await server.start();
  const defaultRows = (await server.get("/v1/events")).events;
  const liveRows = (await server.get("/v1/events", live.secret_key)).events;
  assert.equal(defaultRows.length, events.length);
  assert.equal(liveRows.length, events.length);
  assert.ok(defaultRows.every(r => r.project_id === "default" && r.ingest_key_id === "primary-public" && !r.otlp));
  assert.ok(liveRows.every(r => r.project_id === live.project_id && r.ingest_key_id === firstKey && !r.otlp));
  assert.deepEqual(await server.get("/v1/summary"), { views: 1, unique_viewers: 1, total_playing_time_ms: 1200 });
  assert.deepEqual(await server.get("/v1/summary", live.secret_key), { views: 1, unique_viewers: 1, total_playing_time_ms: 5400 });
  assert.equal((await server.get("/v1/videos?viewer_id=shared-viewer", live.secret_key)).videos[0].total_playing_time_ms, 5400);
  const page = await server.get("/v1/events?limit=1");
  let response = await fetch(`${server.url}/v1/events?limit=1&cursor=${encodeURIComponent(page.next_cursor)}`,
    { headers: { authorization: `Bearer ${live.secret_key}` } });
  assert.equal(response.status, 400);
  // Cursors are positions, not authorization: even a forged scope cannot leak another project.
  const cursor = JSON.parse(Buffer.from(page.next_cursor, "base64url"));
  cursor.project_id = live.project_id;
  cursor.after = 0;
  cursor.through = Number.MAX_SAFE_INTEGER;
  const forged = Buffer.from(JSON.stringify(cursor)).toString("base64url");
  const forgedPage = await server.get(`/v1/events?cursor=${forged}`, live.secret_key);
  assert.ok(forgedPage.events.every(r => r.project_id === live.project_id));
  for (const path of ["/v1/events?project_id=live", "/v1/summary?project_id=live", "/v1/videos?project_id=live"]) {
    response = await fetch(`${server.url}${path}`, { headers: { authorization: `Bearer ${server.keys.secret_key}` } });
    assert.equal(response.status, 400);
  }
  assert.equal((await fetch(`${server.url}/v1/events`, { headers: { authorization: `Bearer ${live.public_key}` } })).status, 401);
  assert.equal((await projectPost(server, live.secret_key, events[0])).status, 401);
  await server.stop();
  await projectCommand(server, ["project", "rename", live.project_id, "Customer A — Live"]);
  await server.start();
  assert.equal((await server.get("/v1/events")).events.length, events.length);
  assert.deepEqual((await server.get("/v1/events", live.secret_key)).events, liveRows);
  await server.stop();
  assert.equal((await projectCommand(server, ["project", "list"])).find(p => p.id === live.project_id).name, "Customer A — Live");
  await server.start();
  assert.deepEqual((await server.get("/v1/events", live.secret_key)).events, liveRows);
});

test("project credentials rotate and revoke independently without changing ownership", async (t) => {
  const server = await collector(t);
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "live"]);
  const extra = await projectCommand(server, ["key", "create", live.project_id, "public", "Website"]);
  const extraRead = await projectCommand(server, ["key", "create", live.project_id, "secret", "Dashboard"]);
  const events = sdkEvents();
  await server.start();
  assert.equal((await projectPost(server, extra.key, events[0])).status, 201);
  await server.stop();
  const rotated = await projectCommand(server, ["key", "rotate", live.project_id, extra.key_id]);
  assert.equal(rotated.key_id, extra.key_id);
  await projectCommand(server, ["key", "revoke", live.project_id, extraRead.key_id]);
  await server.start();
  assert.equal((await projectPost(server, extra.key, events[1])).status, 401);
  assert.equal((await projectPost(server, rotated.key, events[1])).status, 201);
  assert.equal((await fetch(`${server.url}/v1/events`, { headers: { authorization: `Bearer ${extraRead.key}` } })).status, 401);
  assert.ok((await server.get("/v1/events", live.secret_key)).events.every(r => r.ingest_key_id === extra.key_id));
  assert.equal((await server.get("/v1/events")).events.length, 0);
  await server.stop();
  await projectCommand(server, ["key", "revoke", "default", "primary-public"]);
  await server.start();
  assert.equal((await projectPost(server, server.keys.public_key, events[0])).status, 401, "legacy root export is not an auth fallback");
  assert.equal((await projectPost(server, live.public_key, sdkEvents()[0])).status, 201);
});

test("project retention updates live, preserves other settings, and persists across restart", async (t) => {
  const server = await collector(t, { "retention.seconds": "0", "retention.interval_seconds": "1" });
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "live"]);
  const settings = join(server.directory, "live-settings.json");
  await writeFile(settings, JSON.stringify({ allowed_origins: ["https://live.example"], retention_seconds: 60 }));
  await projectCommand(server, ["project", "configure", live.project_id, settings]);
  await server.start();
  const url = server.url;
  const events = sdkEvents("shared-video");
  events.at(-1).playing_time_ms = 1200;
  for (const event of events) {
    await server.send(event);
    await server.send(event, { "X-Chunkify-Ingest-Key": live.public_key });
  }
  await delay(1200);
  const result = await projectCommand(server, ["project", "retention", live.project_id, "1"]);
  assert.deepEqual(result, { project_id: live.project_id, retention_seconds: 1, effective_retention_seconds: 1, restart_required: false });
  const deadline = Date.now() + 7000;
  while ((await server.get("/v1/events", live.secret_key)).events.length && Date.now() < deadline) await delay(100);
  assert.equal((await server.get("/v1/events", live.secret_key)).events.length, 0);
  assert.equal((await server.get("/v1/summary", live.secret_key)).total_playing_time_ms, 0);
  assert.equal((await server.get("/v1/events")).events.length, events.length);
  assert.equal((await server.get("/v1/summary")).total_playing_time_ms, 1200);
  const listed = (await projectCommand(server, ["project", "list"])).find(p => p.id === live.project_id);
  assert.deepEqual(listed.settings.allowed_origins, ["https://live.example"]);
  assert.equal(listed.effective_retention_seconds, 1);
  assert.equal(server.url, url, "retention change must not restart the HTTP server");
  // A retry after expiration is new data. Disable expiry before resending it.
  await projectCommand(server, ["project", "retention", live.project_id, "0"]);
  for (const event of events) await server.send(event, { "X-Chunkify-Ingest-Key": live.public_key });
  await delay(2200);
  assert.equal((await server.get("/v1/events", live.secret_key)).events.length, events.length);
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await projectCommand(server, ["project", "list"])).find(p => p.id === live.project_id).effective_retention_seconds, 0);
  assert.equal((await server.get("/v1/events", live.secret_key)).events.length, events.length);
  // The running server's default is authoritative, even if the CLI has a different environment.
  const inherited = await server.command(["project", "retention", live.project_id, "inherit"], { "retention.seconds": "1" });
  assert.equal(inherited.code, 0, inherited.stderr);
  assert.equal(JSON.parse(inherited.stdout).effective_retention_seconds, 0);
  assert.equal(JSON.parse(inherited.stdout).retention_seconds, null);
  // Enabling a policy after startup with all expiry disabled must start expiring data.
  await projectCommand(server, ["project", "retention", "default", "1"]);
  const secondDeadline = Date.now() + 7000;
  while ((await server.get("/v1/events")).events.length && Date.now() < secondDeadline) await delay(100);
  assert.equal((await server.get("/v1/events")).events.length, 0);
  assert.equal((await server.get("/v1/events", live.secret_key)).events.length, events.length);
});

test("startup retention uses inherited, longer, shorter and disabled project policies", async (t) => {
  const server = await collector(t, { "retention.seconds": "0" });
  await server.stop();
  const projects = [];
  for (const id of ["short", "long", "keep"]) projects.push(await projectCommand(server, ["project", "create", id]));
  await server.start();
  const event = sdkEvents()[0];
  await server.send(event);
  for (const project of projects) await server.send(event, { "X-Chunkify-Ingest-Key": project.public_key });
  await server.stop();
  for (const [id, seconds] of [["short", "1"], ["long", "60"], ["keep", "0"]]) {
    assert.equal((await projectCommand(server, ["project", "retention", projects.find(project => project.name === id).project_id, seconds])).restart_required, true);
  }
  await delay(1200);
  await server.start(binary, { "retention.seconds": "1" });
  assert.equal((await server.get("/v1/events")).events.length, 0);
  for (const project of projects) assert.equal((await server.get("/v1/events", project.secret_key)).events.length, project.name === "short" ? 0 : 1);
  await server.stop();
  // Removing an override makes existing data eligible under the inherited policy at startup.
  await projectCommand(server, ["project", "retention", projects[1].project_id, "inherit"]);
  await server.start(binary, { "retention.seconds": "1" });
  assert.equal((await server.get("/v1/events", projects[1].secret_key)).events.length, 0);
  assert.equal((await server.get("/v1/events", projects[2].secret_key)).events.length, 1);
});

test("invalid or unpersisted project retention changes preserve policy and data", async (t) => {
  const server = await collector(t, { "retention.seconds": "0", "retention.interval_seconds": "1" });
  await server.send(sdkEvents()[0]);
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  for (const value of ["-1", "1.5", "abc", "9223372036854776", "18446744073709551616"]) {
    assert.notEqual((await server.command(["project", "retention", "default", value])).code, 0);
    assert.equal(await readFile(file, "utf8"), before);
  }
  assert.notEqual((await server.command(["project", "retention", "missing", "1"])).code, 0);
  await chmod(server.directory, 0o500);
  try {
    assert.notEqual((await server.command(["project", "retention", "default", "1"])).code, 0);
    assert.equal(await readFile(file, "utf8"), before);
  } finally { await chmod(server.directory, 0o700); }
  await delay(2200);
  assert.equal((await projectCommand(server, ["project", "list"]))[0].effective_retention_seconds, 0);
  assert.equal((await server.get("/v1/events")).events.length, 1);
  await server.stop();
  const settings = join(server.directory, "settings.json");
  for (const retention_seconds of [-1, 1.5, "1", 9223372036854776]) {
    await writeFile(settings, JSON.stringify({ retention_seconds }));
    assert.notEqual((await server.command(["project", "configure", "default", settings])).code, 0);
    assert.equal(await readFile(file, "utf8"), before);
  }
});

test("project origin policies and rate quotas apply across all of its keys", async (t) => {
  const server = await collector(t, { "server.ingest_per_second": "1000" });
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "live"]);
  const extra = await projectCommand(server, ["key", "create", live.project_id, "public", "Another integration"]);
  const settings = join(server.directory, "live-settings.json");
  await writeFile(settings, JSON.stringify({ allowed_origins: ["https://live.example"], ingest_per_second: 2 }));
  await projectCommand(server, ["project", "configure", live.project_id, settings]);
  await server.start();
  const preflight = await fetch(`${server.url}/v1/events`, { method: "OPTIONS", headers: {
    origin: "https://live.example", "access-control-request-method": "POST", "access-control-request-headers": "x-chunkify-ingest-key,content-type" } });
  assert.equal(preflight.status, 204);
  assert.equal((await projectPost(server, live.public_key, sdkEvents()[0], { origin: "https://player.example" })).status, 201);
  assert.equal((await projectPost(server, server.keys.public_key, sdkEvents()[0], { origin: "https://live.example" })).status, 403);
  assert.equal((await projectPost(server, live.public_key, sdkEvents()[0], { origin: "https://live.example" })).status, 201);
  const limited = await projectPost(server, extra.key, sdkEvents()[0]);
  assert.equal(limited.status, 429);
  assert.equal(limited.headers.get("retry-after"), "1");
  assert.equal((await projectPost(server, server.keys.public_key, sdkEvents()[0])).status, 201);
});

test("a busy project leaves admission slots for another project", async (t) => {
  const server = await collector(t, { "batch.delay_ms": "1000", "batch.queue_max_requests": "4",
    "server.ingest_per_second": "1000", "retention.seconds": "0" });
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "live"]);
  await server.start();
  const busy = Array.from({ length: 4 }, () => projectPost(server, live.public_key, sdkEvents()[0]));
  const other = projectPost(server, server.keys.public_key, sdkEvents()[0]);
  const responses = await Promise.all(busy);
  assert.equal(responses.filter(r => r.status === 201).length, 2);
  assert.equal(responses.filter(r => r.status === 503).length, 2);
  assert.equal((await other).status, 201);
  await Promise.all(responses.map(r => r.text()));
});

test("project administration rejects invalid changes without overwriting credentials", async (t) => {
  const server = await collector(t);
  await server.stop();
  const keyFile = join(server.directory, "keys.json");
  for (const args of [["project", "create", ""], ["project", "create", "   "],
    ["project", "create", "x".repeat(129)], ["project", "rename", "missing", "New name"], ["key", "create", "default", "admin", "Wrong kind"]]) {
    const before = await readFile(keyFile, "utf8");
    assert.notEqual((await server.command(args)).code, 0);
    assert.equal(await readFile(keyFile, "utf8"), before);
  }
  await projectCommand(server, ["key", "revoke", "default", "primary-public"]);
  const keys = JSON.parse(await readFile(keyFile, "utf8"));
  keys.projects[0].keys = Array.from({ length: 64 }, (_, index) => ({ id: `key-${index}`, name: "Integration", permission: "public",
    hash: index.toString(16).padStart(64, "0") }));
  await writeFile(keyFile, JSON.stringify(keys));
  const before = await readFile(keyFile, "utf8");
  const result = await server.command(["key", "rotate-primary", "public"]);
  assert.notEqual(result.code, 0);
  assert.match(result.stderr, /64 keys/);
  assert.equal(await readFile(keyFile, "utf8"), before, "failed legacy rotation must not break the registry");
});


test("live keys update ingestion and read access immediately and survive a crash", async (t) => {
  const server = await collector(t);
  await server.stop();
  const other = await projectCommand(server, ["project", "create", "other"]);
  await server.start();
  const originalUrl = server.url;
  const first = await projectCommand(server, ["key", "create", "default", "public", "Website"]);
  const read = await projectCommand(server, ["key", "create", "default", "secret", "Dashboard"]);
  assert.equal(first.restart_required, false);
  assert.equal(read.restart_required, false);
  const events = sdkEvents("live-keys");
  assert.equal((await projectPost(server, first.key, events[0])).status, 201);
  assert.equal((await server.get("/v1/events", read.key)).events.length, 1);
  assert.equal((await server.get("/v1/events", other.secret_key)).events.length, 0);
  assert.equal((await projectPost(server, read.key, events[1])).status, 401);

  const rotated = await projectCommand(server, ["key", "rotate", "default", first.key_id]);
  assert.equal(rotated.restart_required, false);
  assert.equal(rotated.key_id, first.key_id);
  assert.equal((await projectPost(server, first.key, events[1])).status, 401);
  assert.equal((await postEvent(server, "{}", { "X-Chunkify-Ingest-Key": first.key })).status, 401);
  for (const event of events.slice(1)) await server.send(event, { "X-Chunkify-Ingest-Key": rotated.key });
  const stored = (await server.get("/v1/events", read.key)).events;
  assert.ok(stored.every(row => row.ingest_key_id === first.key_id && row.project_id === "default"));
  assert.deepEqual(stored.map(row => row.event), events);
  await projectCommand(server, ["key", "revoke", "default", read.key_id]);
  assert.equal((await fetch(`${server.url}/v1/events`, { headers: { authorization: `Bearer ${read.key}` } })).status, 401);
  assert.equal((await projectPost(server, other.public_key, sdkEvents("other")[0])).status, 201);
  const listed = await projectCommand(server, ["project", "list"]);
  assert.ok(listed.find(project => project.id === "default").keys.some(key => key.id === first.key_id));
  for (const secret of [first.key, rotated.key, read.key, other.public_key, other.secret_key]) {
    assert.ok(!JSON.stringify(listed).includes(secret));
    assert.ok(!server.logs().includes(secret));
  }
  assert.ok(listed.every(project => project.keys.every(key => !("hash" in key))));
  assert.equal((await stat(join(server.directory, "admin.sock"))).mode & 0o777, 0o600);
  assert.equal(server.url, originalUrl);
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await projectPost(server, first.key, sdkEvents()[0])).status, 401);
  assert.equal((await projectPost(server, rotated.key, sdkEvents()[0])).status, 201);
  assert.equal((await fetch(`${server.url}/v1/events`, { headers: { authorization: `Bearer ${read.key}` } })).status, 401);
  assert.equal((await server.get("/v1/summary")).views, 2);
  await server.stop();
  await assert.rejects(stat(join(server.directory, "admin.sock")), { code: "ENOENT" });
});

test("live administration serializes concurrent commands and rejects invalid changes", async (t) => {
  const server = await collector(t);
  const created = await Promise.all(Array.from({ length: 5 }, (_, index) =>
    projectCommand(server, ["key", "create", "default", "secret", `Dashboard ${index}`])));
  const listed = await projectCommand(server, ["project", "list"]);
  assert.equal(listed[0].keys.length, 7);
  await Promise.all(created.map(key => server.get("/v1/summary", key.key)));
  const keyFile = join(server.directory, "keys.json");
  const before = await readFile(keyFile, "utf8");
  for (const args of [["key", "create", "default", "admin", "Invalid"], ["key", "create", "default", "secret", ""],
    ["key", "revoke", "default", "unknown"], ["key", "rotate", "unknown", "primary-public"],
    ["project", "rename", "default", "New name"], ["project", "configure", "default", "missing.json"]]) {
    assert.notEqual((await server.command(args)).code, 0);
    assert.equal(await readFile(keyFile, "utf8"), before);
  }
  await Promise.all(created.map(key => projectCommand(server, ["key", "revoke", "default", key.key_id])));
  assert.equal((await projectCommand(server, ["project", "list"]))[0].keys.length, 2);
});

test("live primary rotation and revocation persist only hashes", async (t) => {
  const server = await collector(t);
  const original = { ...server.keys };
  for (const kind of ["public", "secret"]) {
    const result = await server.command(["key", "rotate-primary", kind]);
    assert.equal(result.code, 0, result.stderr);
    assert.equal(JSON.parse(result.stdout).restart_required, false);
    await server.assertStoredKeyHashes();
    for (const secret of Object.values(server.keys)) assert.ok(!(await readFile(join(server.directory, "keys.json"), "utf8")).includes(secret));
  }
  assert.equal((await projectPost(server, original.public_key, sdkEvents()[0])).status, 401);
  assert.equal((await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${original.secret_key}` } })).status, 401);
  await server.send(sdkEvents()[0]);
  assert.equal((await server.get("/v1/summary")).views, 1);
  await projectCommand(server, ["key", "revoke", "default", "primary-public"]);
  assert.equal((await projectPost(server, server.keys.public_key, sdkEvents()[0])).status, 401);
  const restored = await server.command(["key", "rotate-primary", "public"]);
  assert.equal(restored.code, 0, restored.stderr);
  await server.assertStoredKeyHashes();
  await server.send(sdkEvents()[0]);
  assert.equal((await server.get("/v1/summary")).views, 2);
});

test("failed live key persistence does not change active credentials", async (t) => {
  const server = await collector(t);
  const before = await readFile(join(server.directory, "keys.json"), "utf8");
  // Make creating the replacement file fail, even though the server can still
  // read its existing database. Restore permissions before shutdown/cleanup.
  await chmod(server.directory, 0o500);
  try {
    const result = await server.command(["key", "rotate", "default", "primary-secret"]);
    assert.notEqual(result.code, 0);
    assert.equal(await readFile(join(server.directory, "keys.json"), "utf8"), before);
    assert.equal((await server.get("/v1/summary")).views, 0);
  } finally { await chmod(server.directory, 0o700); }
  const rotated = await projectCommand(server, ["key", "rotate", "default", "primary-secret"]);
  assert.equal((await server.get("/v1/summary", rotated.key)).views, 0);
});


test("live key administration supports a deeply nested data directory", async (t) => {
  const server = await collector(t);
  await server.stop();
  const directory = join(server.directory, "nested-".repeat(24));
  const settings = { "data_dir": directory };
  const init = await server.command(["init"], settings);
  assert.equal(init.code, 0, init.stderr);
  await server.start(binary, settings);
  const created = await server.command(["key", "create", "default", "secret", "Long path"], settings);
  assert.equal(created.code, 0, created.stderr);
  const token = JSON.parse(created.stdout);
  assert.equal(token.restart_required, false);
  assert.equal((await server.get("/v1/summary", token.key)).views, 0);
  const locator = join(directory, "admin.socket-id");
  const previousSocket = await readFile(locator, "utf8");
  assert.equal((await stat(locator)).mode & 0o777, 0o600);
  await server.stop("SIGKILL");
  await server.start(binary, settings);
  assert.notEqual(await readFile(locator, "utf8"), previousSocket);
  const revoked = await server.command(["key", "revoke", "default", token.key_id], settings);
  assert.equal(revoked.code, 0, revoked.stderr);
  assert.equal((await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${token.key}` } })).status, 401);
});

async function projectApi(server, path, { method = "GET", body, token = server.keys.secret_key, headers = {} } = {}) {
  return fetch(`${server.url}/v1/project${path}`, { method,
    headers: { authorization: `Bearer ${token}`, ...(body !== undefined && { "content-type": "application/json" }), ...headers },
    ...(body !== undefined && { body: JSON.stringify(body) }) });
}

test("project origins update live, merge shared origins and persist without changing other settings", async t => {
  const server = await collector(t, { "server.allowed_origins": "https://chunkify.dev", "server.ingest_per_second": "1000" });
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "live"]);
  const file = join(server.directory, "origin-settings.json");
  await writeFile(file, JSON.stringify({ allowed_origins: ["https://old.example"], retention_seconds: 86400, ingest_per_second: 100 }));
  await projectCommand(server, ["project", "configure", live.project_id, file]);
  await server.start();
  const send = origin => projectPost(server, live.public_key, sdkEvents()[0], { origin });
  assert.equal((await send("https://chunkify.dev")).status, 201);
  assert.equal((await send("https://customer.example")).status, 403);
  assert.equal((await projectApi(server, "", { method: "PATCH", body: { allowed_origins: [] }, token: live.public_key })).status, 401);
  for (const body of [{ allowed_origins: null }, { allowed_origins: "https://customer.example" },
    { allowed_origins: ["https://customer.example/"] }, { allowed_origins: ["*"] },
    { allowed_origins: [], project_id: "default" }]) {
    assert.equal((await projectApi(server, "", { method: "PATCH", body, token: live.secret_key })).status, 400);
  }
  const update = origins => projectApi(server, "", { method: "PATCH", body: { allowed_origins: origins }, token: live.secret_key });
  const response = await update(["https://customer.example", "https://customer.example"]);
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.deepEqual(await response.json(), { project_id: live.project_id, name: "live", allowed_origins: ["https://customer.example"],
    effective_allowed_origins: ["https://chunkify.dev", "https://customer.example"], retention_seconds: 86400,
    effective_retention_seconds: 86400, restart_required: false });
  assert.equal((await send("https://customer.example")).status, 201);
  assert.equal((await send("https://old.example")).status, 403);
  assert.equal((await send("https://chunkify.dev")).status, 201);
  assert.equal((await projectPost(server, server.keys.public_key, sdkEvents()[0], { origin: "https://customer.example" })).status, 403);
  await chmod(server.directory, 0o500);
  try { assert.equal((await update(["https://unsaved.example"])).status, 500); }
  finally { await chmod(server.directory, 0o700); }
  assert.equal((await send("https://customer.example")).status, 201);
  assert.equal((await send("https://unsaved.example")).status, 403);
  const [updated, created] = await Promise.all([update(["https://next.example"]),
    server.command(["key", "create", live.project_id, "secret", "Concurrent integration"])]);
  assert.equal(updated.status, 200);
  assert.equal(created.code, 0, created.stderr);
  const token = JSON.parse(created.stdout).key;
  assert.equal((await projectApi(server, "/keys", { token })).status, 200);
  const settings = (await projectCommand(server, ["project", "list"])).find(project => project.id === live.project_id).settings;
  assert.equal(settings.retention_seconds, 86400);
  assert.equal(settings.ingest_per_second, 100);
  await server.stop();
  await server.start();
  assert.equal((await send("https://next.example")).status, 201);
  assert.equal((await projectApi(server, "/keys", { token })).status, 200);
  assert.equal((await update([])).status, 200);
  assert.equal((await send("https://next.example")).status, 403);
  assert.equal((await send("https://chunkify.dev")).status, 201);
});

test("secret keys list and rotate only their own project keys", async t => {
  const server = await collector(t, { "retention.seconds": "0", "server.ingest_per_second": "1000" });
  assert.match(server.keys.public_key, /^ck_public_[0-9a-f]{64}$/);
  assert.match(server.keys.secret_key, /^ck_secret_[0-9a-f]{64}$/);
  await server.stop();
  const live = await projectCommand(server, ["project", "create", "live"]);
  await server.start();
  const options = { token: live.secret_key };
  let response = await projectApi(server, "/keys", options);
  assert.equal(response.status, 200);
  const list = await response.json();
  assert.equal(list.project_id, live.project_id);
  assert.deepEqual(list.keys.map(key => key.permission).sort(), ["public", "secret"]);
  for (const key of list.keys) assert.deepEqual(Object.keys(key).sort(), ["id", "name", "permission", "read_only"]);
  assert.equal((await projectApi(server, "/keys", { token: live.public_key })).status, 401);
  const foreign = (await (await projectApi(server, "/keys")).json()).keys[0].id;
  assert.equal((await projectApi(server, `/keys/${foreign}/rotate`, { ...options, method: "POST" })).status, 404);
  const publicId = list.keys.find(key => key.permission === "public").id;
  response = await projectApi(server, `/keys/${publicId}/rotate`, { ...options, method: "POST" });
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  const rotated = await response.json();
  assert.equal(rotated.permission, "public");
  assert.match(rotated.key, /^ck_public_[0-9a-f]{64}$/);
  assert.equal((await projectPost(server, live.public_key, sdkEvents()[0])).status, 401);
  assert.equal((await projectPost(server, rotated.key, sdkEvents()[0])).status, 201);
  const secretId = list.keys.find(key => key.permission === "secret").id;
  response = await projectApi(server, `/keys/${secretId}/rotate`, { ...options, method: "POST" });
  assert.equal(response.status, 200);
  const secret = (await response.json()).key;
  assert.match(secret, /^ck_secret_[0-9a-f]{64}$/);
  assert.equal((await projectApi(server, "/keys", options)).status, 401);
  assert.equal((await projectApi(server, "/keys", { token: secret })).status, 200);
  assert.equal((await projectApi(server, "", { ...options, method: "PATCH", body: { allowed_origins: [] } })).status, 401);
  await server.stop();
  await server.start();
  assert.equal((await projectApi(server, "/keys", { token: secret })).status, 200);
  assert.equal((await projectPost(server, rotated.key, sdkEvents()[0])).status, 201);
});

async function createProject(server, body, key) {
  return fetch(`${server.url}/v1/projects`, { method: "POST",
    headers: { "content-type": "application/json", ...(key && { authorization: `Bearer ${key}` }) },
    body: JSON.stringify(body) });
}

async function adminProjectApi(server, projectId, path, { method = "GET", token } = {}) {
  return fetch(`${server.url}/v1/projects/${projectId}${path}`, { method,
    headers: token ? { authorization: `Bearer ${token}`, origin: "https://player.example" } : {} });
}

test("admin rotation recovers a lost project secret without rotating its public key", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "retention.seconds": "0" });
  const project = await projectCommand(server, ["project", "create", "Lost secret"]);
  const readonly = await projectCommand(server, ["key", "create", project.project_id, "secret", "Dashboard", "--read-only"]);
  const listPath = "/v1/projects/{project_id}/keys";
  const rotatePath = "/v1/projects/{project_id}/keys/{key_id}/rotate";
  const list = await (await projectApi(server, "/keys", { token: project.secret_key })).json();
  const secretId = list.keys.find(key => key.permission === "secret" && !key.read_only).id;
  const publicId = list.keys.find(key => key.permission === "public").id;
  const event = sdkEvents("recovered-project")[0];
  assert.equal((await projectPost(server, project.public_key, event)).status, 201);

  // Discard the response to reproduce a user losing their only management key.
  const lost = await projectApi(server, `/keys/${secretId}/rotate`, { method: "POST", token: project.secret_key });
  assert.equal(lost.status, 200);
  await lost.arrayBuffer();
  assert.equal((await projectApi(server, "/keys", { token: project.secret_key })).status, 401);
  const listed = await adminProjectApi(server, project.project_id, "/keys", { token: adminKey });
  assert.equal(listed.headers.get("access-control-allow-origin"), null);
  assert.deepEqual(await assertResponse(listPath, "get", listed, 200), list);

  const recover = () => adminProjectApi(server, project.project_id, `/keys/${secretId}/rotate`, { method: "POST", token: adminKey });
  // Losing an admin rotation response is recoverable through the same route.
  await assertResponse(rotatePath, "post", await recover(), 200);
  const recovered = await assertResponse(rotatePath, "post", await recover(), 200);
  assert.equal(recovered.project_id, project.project_id);
  assert.equal(recovered.key_id, secretId);
  assert.equal(recovered.permission, "secret");
  assert.equal(recovered.read_only, false);
  assert.equal((await projectApi(server, "/keys", { token: recovered.key })).status, 200);
  assert.equal((await server.get("/v1/summary", recovered.key)).views, 1);
  assert.equal((await server.get("/v1/summary", readonly.key)).views, 1);
  assert.equal((await server.get("/v1/summary")).views, 0);
  assert.equal((await projectPost(server, project.public_key, event)).status, 200);

  const readResponse = await adminProjectApi(server, project.project_id, `/keys/${readonly.key_id}/rotate`, { method: "POST", token: adminKey });
  const rotatedRead = await assertResponse(rotatePath, "post", readResponse, 200);
  assert.equal(rotatedRead.read_only, true);
  assert.equal((await projectApi(server, "/keys", { token: rotatedRead.key })).status, 403);
  assert.equal((await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${readonly.key}` } })).status, 401);

  const publicResponse = await adminProjectApi(server, project.project_id, `/keys/${publicId}/rotate`, { method: "POST", token: adminKey });
  const rotatedPublic = await assertResponse(rotatePath, "post", publicResponse, 200);
  assert.equal(rotatedPublic.permission, "public");
  assert.equal(rotatedPublic.read_only, false);
  assert.equal((await projectPost(server, project.public_key, event)).status, 401);
  assert.equal((await projectPost(server, rotatedPublic.key, event)).status, 200);
  assert.equal((await server.get("/v1/summary", recovered.key)).views, 1);
  const stored = await readFile(join(server.directory, "keys.json"), "utf8");
  for (const key of [adminKey, recovered.key, rotatedRead.key, rotatedPublic.key]) {
    assert.ok(!stored.includes(key));
    assert.ok(!server.logs().includes(key));
  }
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await server.get("/v1/summary", recovered.key)).views, 1);
  assert.equal((await server.get("/v1/summary", rotatedRead.key)).views, 1);
  assert.equal((await projectPost(server, rotatedPublic.key, event)).status, 200);
  assert.deepEqual(await assertResponse(listPath, "get",
    await adminProjectApi(server, project.project_id, "/keys", { token: adminKey }), 200), list);
});

test("admin key recovery rejects project credentials and unknown, revoked or deleted keys", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey });
  const project = await projectCommand(server, ["project", "create", "Customer"]);
  const readonly = await projectCommand(server, ["key", "create", project.project_id, "secret", "Dashboard", "--read-only"]);
  const revoked = await projectCommand(server, ["key", "create", project.project_id, "secret", "Revoked"]);
  await projectCommand(server, ["key", "revoke", project.project_id, revoked.key_id]);
  const list = await (await projectApi(server, "/keys", { token: project.secret_key })).json();
  const secretId = list.keys.find(key => key.permission === "secret" && !key.read_only).id;
  const listPath = "/v1/projects/{project_id}/keys";
  const rotatePath = "/v1/projects/{project_id}/keys/{key_id}/rotate";
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  for (const token of [undefined, "wrong", project.public_key, project.secret_key, readonly.key, server.keys.secret_key]) {
    for (const [path, method, contractPath] of [["/keys", "get", listPath], [`/keys/${secretId}/rotate`, "post", rotatePath]]) {
      const response = await adminProjectApi(server, project.project_id, path, { method: method.toUpperCase(), token });
      assert.equal(response.headers.get("www-authenticate"), "Bearer");
      await assertResponse(contractPath, method, response, 401);
    }
  }
  await assertResponse(listPath, "get", await adminProjectApi(server, "unknown", "/keys", { token: adminKey }), 404);
  for (const [id, key] of [["unknown", secretId], [project.project_id, "unknown"],
    [project.project_id, revoked.key_id], [project.project_id, "primary-secret"], ["default", secretId]]) {
    await assertResponse(rotatePath, "post",
      await adminProjectApi(server, id, `/keys/${key}/rotate`, { method: "POST", token: adminKey }), 404);
  }
  assert.equal(await readFile(file, "utf8"), before);
  assert.equal((await projectApi(server, "/keys", { token: project.secret_key })).status, 200);
  assert.equal((await projectApi(server, "", { method: "DELETE", token: project.secret_key })).status, 200);
  await assertResponse(listPath, "get", await adminProjectApi(server, project.project_id, "/keys", { token: adminKey }), 404);
  await assertResponse(rotatePath, "post",
    await adminProjectApi(server, project.project_id, `/keys/${secretId}/rotate`, { method: "POST", token: adminKey }), 404);
});

test("admin rotation is disabled without an admin key and preserves credentials when saving fails", async t => {
  const server = await collector(t);
  const listPath = "/v1/projects/{project_id}/keys";
  const rotatePath = "/v1/projects/{project_id}/keys/{key_id}/rotate";
  const rotate = token => adminProjectApi(server, "default", "/keys/primary-secret/rotate", { method: "POST", token });
  await assertResponse(listPath, "get", await adminProjectApi(server, "default", "/keys"), 404);
  await assertResponse(rotatePath, "post", await rotate(server.keys.secret_key), 404);
  await server.stop();
  const adminKey = `admin-${randomUUID()}`;
  await server.start(binary, { CHUNKIFY_ADMIN_KEY: adminKey });
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  await chmod(server.directory, 0o500);
  try { await assertResponse(rotatePath, "post", await rotate(adminKey), 500); }
  finally { await chmod(server.directory, 0o700); }
  assert.equal(await readFile(file, "utf8"), before);
  assert.equal((await projectApi(server, "/keys")).status, 200);
  const recovered = await assertResponse(rotatePath, "post", await rotate(adminKey), 200);
  assert.equal((await projectApi(server, "/keys", { token: recovered.key })).status, 200);
  server.keys.secret_key = recovered.key;
  await server.assertStoredKeyHashes();
  await server.stop();
  await server.start(binary, { CHUNKIFY_ADMIN_KEY: "" });
  await assertResponse(listPath, "get", await adminProjectApi(server, "default", "/keys", { token: adminKey }), 404);
  await assertResponse(rotatePath, "post", await rotate(adminKey), 404);
});

test("admin project creation activates isolated projects immediately and persists across restart", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "server.allowed_origins": "https://chunkify.dev",
    "server.ingest_per_second": "1000", "retention.seconds": "0" });
  const url = server.url;
  const request = { name: "Production / Europe", allowed_origins: ["https://customer.example", "https://customer.example"] };
  for (const key of [undefined, "wrong", server.keys.public_key, server.keys.secret_key]) {
    assert.equal((await createProject(server, request, key)).status, 401);
  }
  const response = await createProject(server, request, adminKey);
  assert.equal(response.status, 201);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.equal(response.headers.get("access-control-allow-origin"), null);
  const project = await response.json();
  component("CreatedProject", project);
  assert.match(project.project_id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.equal(project.name, request.name);
  assert.equal(project.restart_required, false);
  assert.match(project.public_key, /^ck_public_[0-9a-f]{64}$/);
  assert.match(project.secret_key, /^ck_secret_[0-9a-f]{64}$/);
  assert.equal(server.url, url);
  const event = sdkEvents()[0];
  assert.equal((await projectPost(server, project.public_key, event, { origin: "https://customer.example" })).status, 201);
  assert.equal((await projectPost(server, project.public_key, event, { origin: "https://chunkify.dev" })).status, 200);
  assert.equal((await projectPost(server, project.public_key, event, { origin: "https://wrong.example" })).status, 403);
  assert.equal((await projectPost(server, server.keys.public_key, event, { origin: "https://customer.example" })).status, 403);
  assert.deepEqual((await server.get("/v1/events", project.secret_key)).events.map(row => row.project_id), [project.project_id]);
  assert.deepEqual((await server.get("/v1/events")).events, []);
  assert.equal((await createProject(server, { name: "Forbidden" }, project.secret_key)).status, 401);
  for (const path of ["/v1/events", "/v1/project/keys", "/mcp"]) {
    assert.equal((await fetch(`${server.url}${path}`, { headers: { authorization: `Bearer ${adminKey}` } })).status, 401);
  }
  const preflight = await fetch(`${server.url}/v1/events`, { method: "OPTIONS", headers: {
    origin: "https://customer.example", "access-control-request-method": "POST" } });
  assert.equal(preflight.status, 204);
  const sameName = await (await createProject(server, request, adminKey)).json();
  assert.notEqual(sameName.project_id, project.project_id);
  assert.deepEqual((await server.get("/v1/events", sameName.secret_key)).events, []);
  const listing = await projectCommand(server, ["project", "list"]);
  assert.deepEqual(listing.find(p => p.id === project.project_id).settings.allowed_origins, ["https://customer.example"]);
  const stored = await readFile(join(server.directory, "keys.json"), "utf8");
  for (const secret of [adminKey, project.public_key, project.secret_key]) assert.ok(!stored.includes(secret));
  await server.stop();
  await server.start();
  assert.equal((await server.get("/v1/events", project.secret_key)).events.length, 1);
  assert.equal((await projectApi(server, "/keys", { token: project.secret_key })).status, 200);
  assert.equal((await projectPost(server, project.public_key, event, { origin: "https://customer.example" })).status, 200);
});

test("project creation is disabled without an admin key and validates before saving", async t => {
  const disabled = await collector(t);
  assert.equal((await createProject(disabled, { name: "Disabled" }, disabled.keys.secret_key)).status, 404);
  await disabled.stop();
  const dotenvKey = `dotenv-${randomUUID()}`;
  await writeFile(join(disabled.directory, ".env"), `CHUNKIFY_ADMIN_KEY=${dotenvKey}\n`, { mode: 0o600 });
  await disabled.start();
  assert.equal((await createProject(disabled, { name: "From dotenv" }, dotenvKey)).status, 201);
  await disabled.stop();
  const processKey = `process-${randomUUID()}`;
  await disabled.start(binary, { CHUNKIFY_ADMIN_KEY: processKey });
  assert.equal((await createProject(disabled, { name: "Old key" }, dotenvKey)).status, 401);
  assert.equal((await createProject(disabled, { name: "Process key" }, processKey)).status, 201);
  await disabled.stop();
  await disabled.start(binary, { CHUNKIFY_ADMIN_KEY: "" });
  assert.equal((await createProject(disabled, { name: "Disabled" }, processKey)).status, 404);
  await disabled.stop();
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey });
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  for (const body of [{}, { name: "" }, { name: "  " }, { name: "x".repeat(129) }, { name: 42 },
    { name: "Customer", id: "chosen" }, { name: "Customer", project_id: "chosen" },
    { name: "Customer", allowed_origins: null }, { name: "Customer", allowed_origins: "https://customer.example" },
    { name: "Customer", allowed_origins: ["*"] }, { name: "Customer", allowed_origins: ["https://customer.example/"] }]) {
    const result = await createProject(server, body, adminKey);
    assert.equal(result.status, 400, await result.clone().text());
    assert.equal(await readFile(file, "utf8"), before);
  }
  await chmod(server.directory, 0o500);
  try { assert.equal((await createProject(server, { name: "Unsaved" }, adminKey)).status, 500); }
  finally { await chmod(server.directory, 0o700); }
  assert.equal(await readFile(file, "utf8"), before);
  assert.equal((await projectCommand(server, ["project", "list"])).length, 1);
  const [apiResponse, cli] = await Promise.all([
    createProject(server, { name: "API customer" }, adminKey),
    projectCommand(server, ["project", "create", "CLI customer"]),
  ]);
  assert.equal(apiResponse.status, 201);
  assert.equal(cli.restart_required, false);
  const api = await apiResponse.json();
  assert.notEqual(api.project_id, cli.project_id);
  assert.equal((await projectCommand(server, ["project", "list"])).length, 3);
  for (const customer of [api, cli]) assert.deepEqual((await server.get("/v1/events", customer.secret_key)).events, []);
});

test("PATCH project updates only supplied fields and persists both fields atomically", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "server.allowed_origins": "https://chunkify.dev" });
  const customer = await projectCommand(server, ["project", "create", "Customer"]);
  const update = body => projectApi(server, "", { method: "PATCH", token: customer.secret_key, body });
  const state = async () => (await projectCommand(server, ["project", "list"])).find(p => p.id === customer.project_id);
  assert.equal((await projectApi(server, "/allowed-origins", { method: "PUT", token: customer.secret_key,
    body: { allowed_origins: [] } })).status, 404);
  for (const token of [customer.public_key, adminKey, "invalid"]) {
    assert.equal((await projectApi(server, "", { method: "PATCH", token, body: { name: "Forbidden" } })).status, 401);
  }
  for (const body of [{}, null, [], { name: "" }, { name: " " }, { name: "x".repeat(129) }, { name: 1 },
    { name: null, allowed_origins: [] }, { name: "New name", allowed_origins: null },
    { name: "New name", allowed_origins: ["*"] }, { project_id: "default", name: "Forbidden" }]) {
    assert.equal((await update(body)).status, 400);
    assert.equal((await state()).name, "Customer");
  }
  let response = await update({ name: "Renamed", allowed_origins: ["https://first.example"] });
  assert.equal(response.status, 200);
  component("Project", await response.json());
  response = await update({ name: "Name only" });
  assert.equal(response.status, 200);
  assert.deepEqual((await response.json()).allowed_origins, ["https://first.example"]);
  response = await update({ allowed_origins: ["https://second.example"] });
  assert.equal(response.status, 200);
  assert.equal((await response.json()).name, "Name only");
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  await chmod(server.directory, 0o500);
  try { assert.equal((await update({ name: "Unsaved", allowed_origins: ["https://unsaved.example"] })).status, 500); }
  finally { await chmod(server.directory, 0o700); }
  assert.equal(await readFile(file, "utf8"), before);
  const results = await Promise.all([update({ name: "Concurrent name" }), update({ allowed_origins: ["https://customer.example"] })]);
  assert.ok(results.every(result => result.status === 200));
  assert.equal((await state()).name, "Concurrent name");
  assert.deepEqual((await state()).settings.allowed_origins, ["https://customer.example"]);
  const listing = await projectCommand(server, ["project", "list"]);
  assert.equal(listing.find(p => p.id === "default").name, "Default project");
  await server.stop();
  await server.start();
  assert.equal((await state()).name, "Concurrent name");
  assert.deepEqual((await state()).settings.allowed_origins, ["https://customer.example"]);
  assert.equal((await projectPost(server, customer.public_key, sdkEvents()[0], { origin: "https://customer.example" })).status, 201);
});

test("DELETE project revokes all keys, preserves retention and isolates other projects", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "server.allowed_origins": "https://chunkify.dev" });
  const customer = await projectCommand(server, ["project", "create", "Deleted customer"]);
  const extra = await projectCommand(server, ["key", "create", customer.project_id, "secret", "Dashboard"]);
  const publicKey = await projectCommand(server, ["key", "create", customer.project_id, "public", "Website"]);
  await projectCommand(server, ["project", "retention", customer.project_id, "60"]);
  assert.equal((await projectApi(server, "", { method: "PATCH", token: customer.secret_key,
    body: { allowed_origins: ["https://customer.example"] } })).status, 200);
  await server.send(sdkEvents()[0]);
  const remove = (token = customer.secret_key, path = "", body) => projectApi(server, path, { method: "DELETE", token, body });
  for (const token of [customer.public_key, adminKey, "invalid"]) assert.equal((await remove(token)).status, 401);
  assert.equal((await remove(customer.secret_key, "?project_id=default")).status, 400);
  assert.equal((await remove(customer.secret_key, "", { project_id: "default" })).status, 400);
  await chmod(server.directory, 0o500);
  try { assert.equal((await remove()).status, 500); }
  finally { await chmod(server.directory, 0o700); }
  assert.equal((await projectApi(server, "/keys", { token: customer.secret_key })).status, 200);
  const response = await remove();
  assert.equal(response.status, 200);
  assert.equal(response.headers.get("cache-control"), "no-store");
  component("DeletedProject", await response.json());
  for (const token of [customer.secret_key, extra.key]) {
    for (const path of ["/v1/events", "/mcp", "/v1/project/keys"]) {
      assert.equal((await fetch(`${server.url}${path}`, { headers: { authorization: `Bearer ${token}` } })).status, 401);
    }
  }
  for (const key of [customer.public_key, publicKey.key]) assert.equal((await projectPost(server, key, sdkEvents()[0])).status, 401);
  assert.equal((await remove()).status, 401);
  assert.equal((await projectApi(server, "", { method: "PATCH", token: customer.secret_key, body: { name: "Revive" } })).status, 401);
  const preflight = await fetch(`${server.url}/v1/events`, { method: "OPTIONS", headers: { origin: "https://customer.example",
    "access-control-request-method": "POST" } });
  assert.equal(preflight.status, 403);
  let listing = await projectCommand(server, ["project", "list"]);
  const deleted = listing.find(p => p.id === customer.project_id);
  assert.equal(deleted.deleted, true);
  assert.deepEqual(deleted.keys, []);
  assert.equal(deleted.settings.retention_seconds, 60);
  assert.notEqual((await server.command(["key", "create", customer.project_id, "secret", "Revive"])).code, 0);
  assert.equal((await server.get("/v1/events")).events.length, 1);
  await server.stop();
  await server.start();
  assert.equal((await remove()).status, 401);
  assert.equal((await server.get("/v1/events")).events.length, 1);
  listing = await projectCommand(server, ["project", "list"]);
  assert.equal(listing.find(p => p.id === customer.project_id).deleted, true);
});

test("HTTP project retention applies at creation, changes live and survives restart", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "retention.seconds": "0", "retention.interval_seconds": "1" });
  const body = { name: "Expiring customer", allowed_origins: ["https://customer.example"], retention_seconds: 1 };
  assertRequest("/v1/projects", "post", body);
  const project = await assertResponse("/v1/projects", "post", await createProject(server, body, adminKey), 201);
  assert.equal(project.retention_seconds, 1);
  assert.equal(project.effective_retention_seconds, 1);
  const inherited = await assertResponse("/v1/projects", "post", await createProject(server, { name: "Inherited" }, adminKey), 201);
  assert.equal(inherited.retention_seconds, null);
  assert.equal(inherited.effective_retention_seconds, 0);
  const update = async body => {
    assertRequest("/v1/project", "patch", body);
    return assertResponse("/v1/project", "patch", await projectApi(server, "", { method: "PATCH", token: project.secret_key, body }), 200);
  };
  const event = sdkEvents()[0];
  await server.send(event);
  assert.equal((await projectPost(server, project.public_key, event)).status, 201);
  const deadline = Date.now() + 7000;
  while ((await server.get("/v1/events", project.secret_key)).events.length && Date.now() < deadline) await delay(100);
  assert.equal((await server.get("/v1/events", project.secret_key)).events.length, 0);
  assert.equal((await server.get("/v1/summary", project.secret_key)).views, 0);
  assert.equal((await server.get("/v1/events")).events.length, 1);

  let changed = await update({ name: "Keep history", retention_seconds: 0 });
  assert.equal(changed.effective_retention_seconds, 0);
  assert.deepEqual(changed.allowed_origins, body.allowed_origins);
  assert.equal((await projectPost(server, project.public_key, event)).status, 201);
  await delay(2200);
  assert.equal((await server.get("/v1/events", project.secret_key)).events.length, 1);
  changed = await update({ allowed_origins: [] });
  assert.equal(changed.retention_seconds, 0, "omission preserves the override");
  await server.stop("SIGKILL");
  await server.start(binary, { "retention.seconds": "1" });
  assert.equal((await server.get("/v1/events", project.secret_key)).events.length, 1, "unlimited override survives replacement");
  changed = await update({ retention_seconds: null });
  assert.equal(changed.retention_seconds, null);
  assert.equal(changed.effective_retention_seconds, 1);
  const resetDeadline = Date.now() + 7000;
  while ((await server.get("/v1/events", project.secret_key)).events.length && Date.now() < resetDeadline) await delay(100);
  assert.equal((await server.get("/v1/events", project.secret_key)).events.length, 0, "null restores the active installation policy");
  await server.stop();
  await server.start();
  const saved = (await projectCommand(server, ["project", "list"])).find(p => p.id === project.project_id);
  assert.equal(saved.settings.retention_seconds, null);
  assert.equal(saved.name, "Keep history");
});

test("HTTP retention rejects invalid changes and keeps active policy when persistence fails", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "retention.seconds": "0", "retention.interval_seconds": "1" });
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  for (const retention_seconds of [-1, 0.5, "60", true, {}, [], 1e16]) {
    await assertResponse("/v1/projects", "post", await createProject(server, { name: "Invalid", retention_seconds }, adminKey), 400);
    await assertResponse("/v1/project", "patch", await projectApi(server, "", {
      method: "PATCH", body: { name: "Must not change", retention_seconds } }), 400);
    assert.equal(await readFile(file, "utf8"), before);
  }
  for (const token of [adminKey, server.keys.public_key, "invalid"]) {
    await assertResponse("/v1/project", "patch", await projectApi(server, "", {
      method: "PATCH", token, body: { retention_seconds: 1 } }), 401);
  }
  const event = sdkEvents()[0];
  await server.send(event);
  await chmod(server.directory, 0o500);
  try {
    await assertResponse("/v1/project", "patch", await projectApi(server, "", {
      method: "PATCH", body: { name: "Unsaved", allowed_origins: [], retention_seconds: 1 } }), 500);
  } finally { await chmod(server.directory, 0o700); }
  assert.equal(await readFile(file, "utf8"), before);
  await delay(2200);
  assert.equal((await server.get("/v1/events")).events.length, 1);
});

test("HTTP key creation scopes public, write and read-only keys and returns secrets only once", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { CHUNKIFY_ADMIN_KEY: adminKey, "retention.seconds": "0" });
  const project = await projectCommand(server, ["project", "create", "API keys"]);
  const create = (body, token = project.secret_key) => projectApi(server, "/keys", { method: "POST", body, token });
  const publicBody = { name: "Website", permission: "public" };
  for (const token of [adminKey, project.public_key, "invalid"]) {
    await assertResponse("/v1/project/keys", "post", await create(publicBody, token), 401);
  }
  const created = [];
  for (const body of [publicBody, { name: "Backend", permission: "secret" },
    { name: "Dashboard", permission: "secret", read_only: true }]) {
    assertRequest("/v1/project/keys", "post", body);
    const key = await assertResponse("/v1/project/keys", "post", await create(body), 201);
    assert.equal(key.project_id, project.project_id);
    assert.equal(key.permission, body.permission);
    assert.equal(key.read_only, body.read_only ?? false);
    assert.equal(key.restart_required, false);
    created.push(key);
  }
  const [publicKey, writeKey, readKey] = created;
  assert.equal((await projectPost(server, publicKey.key, sdkEvents()[0])).status, 201);
  assert.equal((await server.get("/v1/summary", readKey.key)).views, 1);
  assert.equal((await server.get("/v1/summary")).views, 0);
  await assertResponse("/v1/project", "patch", await projectApi(server, "", {
    method: "PATCH", token: writeKey.key, body: { name: "Managed by API key", retention_seconds: 0 } }), 200);
  await assertResponse("/v1/project/keys", "post", await create(publicBody, readKey.key), 403);
  await assertResponse("/v1/project", "patch", await projectApi(server, "", {
    method: "PATCH", token: readKey.key, body: { retention_seconds: 1 } }), 403);
  const listing = await server.get("/v1/project/keys", writeKey.key);
  assert.equal(listing.keys.length, 5);
  const stored = await readFile(join(server.directory, "keys.json"), "utf8");
  for (const key of created) {
    assert.ok(listing.keys.some(metadata => metadata.id === key.key_id));
    assert.ok(!JSON.stringify(listing).includes(key.key));
    assert.ok(!stored.includes(key.key));
  }
  const rotated = await assertResponse("/v1/project/keys/{key_id}/rotate", "post", await projectApi(server,
    `/keys/${readKey.key_id}/rotate`, { method: "POST", token: writeKey.key }), 200);
  assert.equal(rotated.read_only, true);
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await server.get("/v1/summary", rotated.key)).views, 1);
  assert.equal((await projectPost(server, publicKey.key, sdkEvents()[0])).status, 201);
  assert.equal((await server.get("/v1/project/keys", writeKey.key)).keys.length, 5);
  assert.equal((await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${readKey.key}` } })).status, 401);
});

test("HTTP key creation validates before saving and enforces the key limit under concurrent requests", async t => {
  const server = await collector(t);
  const create = body => projectApi(server, "/keys", { method: "POST", body });
  const file = join(server.directory, "keys.json");
  const before = await readFile(file, "utf8");
  for (const body of [{}, { name: "Missing permission" }, { permission: "public" },
    { name: "", permission: "public" }, { name: "  ", permission: "secret" }, { name: "x".repeat(129), permission: "secret" },
    { name: "é".repeat(65), permission: "secret" }, { name: "Invalid", permission: "admin" },
    { name: "Invalid", permission: "public", read_only: true }, { name: "Invalid", permission: "secret", read_only: "true" },
    { name: "Invalid", permission: "secret", read_only: null }, { name: "Foreign", permission: "public", project_id: "other" }]) {
    await assertResponse("/v1/project/keys", "post", await create(body), 400);
    assert.equal(await readFile(file, "utf8"), before);
  }
  await chmod(server.directory, 0o500);
  try { await assertResponse("/v1/project/keys", "post", await create({ name: "Unsaved", permission: "secret" }), 500); }
  finally { await chmod(server.directory, 0o700); }
  assert.equal(await readFile(file, "utf8"), before);
  assert.equal((await server.get("/v1/project/keys")).keys.length, 2);
  const results = await Promise.all(Array.from({ length: 64 }, (_, index) => create({ name: `Key ${index}`, permission: "secret" })));
  assert.equal(results.filter(r => r.status === 201).length, 62);
  assert.equal(results.filter(r => r.status === 409).length, 2);
  for (const response of results) await assertResponse("/v1/project/keys", "post", response, response.status);
  assert.equal((await server.get("/v1/project/keys")).keys.length, 64);
});

test("read-only private keys read analytics live and cannot manage projects or keys", async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await collector(t, { "retention.seconds": "0", CHUNKIFY_ADMIN_KEY: adminKey });
  const project = await projectCommand(server, ["project", "create", "Read-only customer"]);
  const created = await projectCommand(server, ["key", "create", project.project_id, "secret", "Dashboard", "--read-only"]);
  assert.equal(created.restart_required, false);
  assert.equal(created.permission, "secret");
  assert.equal(created.read_only, true);
  assert.match(created.key, /^ck_secret_[0-9a-f]{64}$/);
  const events = sdkEvents("readonly-video");
  for (const event of events) assert.equal((await projectPost(server, project.public_key, event)).status, 201);
  const from = events[0].occurred_at;
  const to = new Date(Date.parse(from) + 86400000).toISOString();
  const routes = ["/v1/events", "/v1/summary", "/v1/videos", "/v1/quality", "/v1/engagement",
    "/v1/summary?group_by=browser_name", "/v1/quality?group_by=os_name", "/v1/engagement?group_by=device_type",
    `/v1/timeseries?${new URLSearchParams({ from, to, interval: "day" })}`];
  for (const route of routes) {
    assert.deepEqual(await server.get(route, created.key), await server.get(route, project.secret_key));
    const head = await fetch(`${server.url}${route}`, { method: "HEAD", headers: { authorization: `Bearer ${created.key}` } });
    assert.equal(head.status, 200);
  }
  assert.equal((await server.get("/v1/events")).events.length, 0, "read-only access stays in its project");
  const list = await (await projectApi(server, "/keys", { token: project.secret_key })).json();
  const metadata = list.keys.find(key => key.id === created.key_id);
  component("ProjectKey", metadata);
  assert.equal(metadata.read_only, true);
  assert.ok(list.keys.filter(key => key.id !== created.key_id).every(key => key.read_only === false));
  for (const [path, method, body] of [["/keys", "GET"], ["", "PATCH", { name: "Forbidden" }], ["", "DELETE"],
    [`/keys/${created.key_id}/rotate`, "POST"], ["/keys/primary-secret/rotate", "POST"], ["/keys/foreign/rotate", "POST"]]) {
    const response = await projectApi(server, path, { method, body, token: created.key });
    assert.equal(response.status, 403, `${method} ${path}`);
    assert.equal(response.headers.get("cache-control"), "no-store");
    component("Error", await response.json());
  }
  assert.deepEqual(await (await projectApi(server, "/keys", { token: project.secret_key })).json(), list);
  assert.equal((await projectPost(server, created.key, events[0])).status, 401);
  assert.equal((await createProject(server, { name: "Forbidden" }, created.key)).status, 401);
  const rotatedResponse = await projectApi(server, `/keys/${created.key_id}/rotate`, { method: "POST", token: project.secret_key });
  assert.equal(rotatedResponse.status, 200);
  const rotated = await rotatedResponse.json();
  component("RotatedProjectKey", rotated);
  assert.equal(rotated.read_only, true);
  assert.equal(rotated.restart_required, false);
  assert.equal((await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${created.key}` } })).status, 401);
  assert.equal((await server.get("/v1/summary", rotated.key)).views, 1);
  await server.stop("SIGKILL");
  await server.start();
  assert.equal((await server.get("/v1/summary", rotated.key)).views, 1);
  assert.equal((await projectApi(server, "", { method: "DELETE", token: rotated.key })).status, 403);
  const projects = await projectCommand(server, ["project", "list"]);
  const saved = projects.find(p => p.id === project.project_id);
  assert.equal(saved.name, "Read-only customer");
  assert.equal(saved.keys.find(key => key.id === created.key_id).read_only, true);
  const rejected = await server.command(["key", "create", project.project_id, "public", "Invalid", "--read-only"]);
  assert.notEqual(rejected.code, 0);
  assert.match(rejected.stderr, /Only secret keys support read-only access/);
  await projectCommand(server, ["key", "revoke", project.project_id, created.key_id]);
  assert.equal((await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${rotated.key}` } })).status, 401);
  assert.equal((await projectApi(server, "/keys", { token: project.secret_key })).status, 200);
});

for (const readers of [false, true]) test(`retention reports watched sections and exports CSV (readers=${readers})`, async t => {
  const server = await collector(t, { "experimental.read_worker": String(readers) });
  const records = sdkEvents("retention-video");
  const last = { ...records.at(-1), media_duration_seconds: 5,
    played_ranges: [{ start_seconds: 0, end_seconds: 1 }, { start_seconds: 3, end_seconds: 4 }], playhead_seconds: 4 };
  await server.send(records[0]); await server.send(last); await server.send(last);
  const report = await server.get("/v1/retention?video_id=retention-video");
  component("RetentionReport", report);
  assert.equal(report.views, 1); assert.equal(report.sample_views, 1);
  assert.deepEqual(report.buckets.map(b => b.views), [1, 0, 0, 1, 0]);
  for (const query of ["", "video_id=retention-video&limit=1", "video_id=retention-video&group_by=country"]) {
    assert.equal((await fetch(`${server.url}/v1/retention?${query}`, { headers: { Authorization: `Bearer ${server.keys.secret_key}` } })).status, 400);
  }
  assert.equal((await fetch(`${server.url}/v1/retention?video_id=retention-video`)).status, 401);
  const invalid = { ...last, event_id: randomUUID(), played_ranges: [{ start_seconds: 3, end_seconds: 2 }] };
  assert.equal((await postEvent(server, JSON.stringify(invalid))).status, 400);
  const response = await fetch(`${server.url}/v1/exports`, { method: "POST", headers: { Authorization: `Bearer ${server.keys.secret_key}`, "content-type": "application/json" }, body: JSON.stringify({ resource: "retention", query: { video_id: "retention-video" } }) });
  assert.equal(response.status, 201, await response.clone().text());
  const grant = await response.json();
  const csv = await (await fetch(grant.download_url)).text();
  assert.match(csv, /start_seconds,end_seconds,views,ratio/);
  assert.equal(csv.trim().split("\n").length, 6);
});
