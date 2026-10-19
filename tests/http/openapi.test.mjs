import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { gzipSync } from "node:zlib";
import test from "node:test";
import { attachAnalytics } from "@chunkify/analytics";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";
import { component, assertRequest, assertResponse } from "./openapi-support.mjs";

const binary = process.env.COLLECTOR_BINARY ?? resolve("target/debug/chunkify-analytics-collector");

async function collector(t, settings = {}) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-openapi-test-"));
  const inherited = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("COLLECTOR_")));
  const env = { ...inherited, COLLECTOR_DATA_DIR: directory, COLLECTOR_BIND: "127.0.0.1:0",
    COLLECTOR_ALLOWED_ORIGINS: "https://player.example", COLLECTOR_RETENTION_SECONDS: "0", ...settings };
  let child;
  let closed;
  let logs = "";
  t.after(async () => {
    if (child && child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    if (closed) {
      const forceStop = setTimeout(() => child.kill("SIGKILL"), 5000);
      try { await closed; } finally { clearTimeout(forceStop); }
    }
    await rm(directory, { recursive: true, force: true });
  });
  const init = spawn(binary, ["init"], { env, stdio: "pipe" });
  init.stdout.resume();
  init.stderr.on("data", chunk => { logs += chunk; });
  assert.equal((await once(init, "close"))[0], 0, logs);
  const keys = JSON.parse(await readFile(join(directory, "keys.json"), "utf8"));
  child = spawn(binary, ["serve"], { env, stdio: "pipe" });
  closed = once(child, "close");
  child.stderr.on("data", chunk => { logs += chunk; });
  const url = await new Promise((resolveReady, reject) => {
    const timeout = setTimeout(() => reject(new Error(`Collector startup timed out: ${logs}`)), 10000);
    child.once("error", error => { clearTimeout(timeout); reject(error); });
    child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Collector exited: ${logs}`)); });
    child.stdout.on("data", chunk => {
      logs += chunk;
      const match = logs.match(/Listening on (http:\/\/\S+)/);
      if (match) { clearTimeout(timeout); resolveReady(match[1]); }
    });
  });
  return {
    url, keys,
    async post(path, body, status, headers = {}) {
      const response = await fetch(`${url}${path}`, { method: "POST", body,
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": keys.ingest_key, ...headers } });
      return assertResponse(path, "post", response, status);
    },
    async get(path, status = 200, headers = { Authorization: `Bearer ${keys.read_token}` }) {
      return assertResponse(path.split("?")[0], "get", await fetch(`${url}${path}`, { headers }), status);
    },
  };
}

function sdkPlayback(t, videoId = "contract-a") {
  let now = 0;
  t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: Date.parse("2026-10-19T12:00:00Z") });
  const clock = t.mock.method(performance, "now", () => now);
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const events = [];
  let rendition;
  const session = attachAnalytics(media, { videoId, viewerId: "contract-viewer", snapshotIntervalMs: 1000,
    destinations: [{ send(event) { events.push(event); } }] });
  try {
    session.setConnector({ name: "contract-engine", version: "1", playerName: null, playerVersion: null,
      snapshot: () => null, subscribeToErrors: () => () => {},
      subscribeToRenditions(listener) { rendition = listener; return () => {}; } });
    rendition({ width: 640, height: 360, codec: "avc1", bitrate_bps: 500000, frame_rate: 30 });
    session.startAttempt();
    media.dispatchEvent(new Event("playing"));
    now = 1000;
    t.mock.timers.tick(1000);
    rendition({ width: 1280, height: 720, codec: "avc1", bitrate_bps: 1000000, frame_rate: 30 });
    media.dispatchEvent(new Event("seeking"));
    media.currentTime = 30;
    media.dispatchEvent(new Event("seeked"));
    media.dispatchEvent(new Event("playing"));
    now = 1400;
    t.mock.timers.tick(400);
    media.error = { code: 2 };
    media.dispatchEvent(new Event("error"));
    session.destroy();
    assert.equal(new Set(events.map(event => event.event_name)).size, 7);
    for (const event of events) assertRequest("/v1/events", "post", event);
    return events;
  } finally {
    session.destroy();
    clock.mock.restore();
    t.mock.timers.reset();
  }
}

async function otlpPayload(events) {
  const requests = [];
  const receiver = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    requests.push(JSON.parse(Buffer.concat(chunks)));
    response.writeHead(200, { "Content-Type": "application/json" }).end("{}");
  });
  receiver.listen(0, "127.0.0.1");
  await once(receiver, "listening");
  try {
    const adapter = createOtlpAdapter(`http://127.0.0.1:${receiver.address().port}/v1/logs`);
    for (const event of events) await adapter.send(event);
    const batch = { resourceLogs: requests.flatMap(request => request.resourceLogs) };
    assertRequest("/v1/logs", "post", batch);
    return batch;
  } finally {
    await new Promise(resolveClose => receiver.close(resolveClose));
  }
}

const browserContext = {
  schema_version: 1, user_agent: "Contract test", browser_name: "Chrome", browser_version: "130.0",
  os_name: "macOS", os_version: "15.0", device_type: "desktop", device_family: null, language: "en-US",
  language_prefix: "en", timezone: "Europe/Paris", timezone_offset_minutes: -120,
  screen_width_px: 1920, screen_height_px: 1080, viewport_width_px: 1280, viewport_height_px: 720, pixel_ratio: 2,
};

test("documented native requests, read responses, filters and pagination match the public API", async t => {
  const events = sdkPlayback(t).map(event => ({ ...event, browser_context: browserContext, custom_tag: "preserved" }));
  component("BrowserContext", browserContext);
  const server = await collector(t);
  await server.get("/healthz", 200, {});
  assert.deepEqual(await server.get("/v1/ingestion"), { mode: "direct" });
  for (const event of events) {
    assertRequest("/v1/events", "post", event);
    assert.deepEqual(await server.post("/v1/events", JSON.stringify(event), 201), { accepted: true, duplicate: false });
  }
  assert.deepEqual(await server.post("/v1/events", JSON.stringify(events[0]), 200), { accepted: false, duplicate: true });
  const filters = "video_id=contract-a&viewer_id=contract-viewer&browser_name=Chrome&os_name=macOS&device_type=desktop";
  let page = await server.get(`/v1/events?${filters}&limit=2`);
  const received = [...page.events];
  assert.ok(page.next_cursor);
  const firstCursor = page.next_cursor;
  while (page.next_cursor) {
    page = await server.get(`/v1/events?${filters}&limit=2&cursor=${encodeURIComponent(page.next_cursor)}`);
    received.push(...page.events);
  }
  assert.deepEqual(received.map(record => record.event), events);
  assert.equal(received[0].event_context.browser_name, "Chrome");
  assert.equal(received[0].event_context.client_ip, "127.0.0.1");
  await server.get(`/v1/events?${filters.replace("Chrome", "Firefox")}&cursor=${encodeURIComponent(firstCursor)}`, 400);
  const summary = await server.get(`/v1/summary?${filters}`);
  assert.deepEqual(summary, { views: 1, unique_viewers: 1, total_playing_time_ms: events.at(-1).playing_time_ms });
  assert.deepEqual(await server.get("/v1/summary?browser_name=Firefox"), { views: 0, unique_viewers: 0, total_playing_time_ms: 0 });
  const extra = sdkPlayback(t, "contract-b");
  for (const event of extra) await server.post("/v1/events", JSON.stringify(event), 201);
  const videos = await server.get("/v1/videos?limit=1");
  assert.equal(videos.videos[0].video_id, "contract-a");
  assert.ok(videos.next_cursor);
  const next = await server.get(`/v1/videos?limit=1&cursor=${encodeURIComponent(videos.next_cursor)}`);
  assert.equal(next.videos[0].video_id, "contract-b");
  assert.equal(next.next_cursor, null);
  await server.get(`/v1/events?cursor=${encodeURIComponent(videos.next_cursor)}`, 400);
  for (const path of ["/v1/events?limit=0", "/v1/events?limit=1001", "/v1/events?limit=no", "/v1/events?unknown=1",
    "/v1/events?device_type=TV", "/v1/events?browser_name=", "/v1/events?browser_name=" + "a".repeat(65),
    "/v1/events?from=2026-10-20T00:00:00Z&to=2026-10-19T00:00:00Z", "/v1/summary?limit=1", "/v1/videos?event_name=chunkify.video.view_started"]) {
    await server.get(path, 400);
  }
});

test("OTLP gzip, all seven SDK events and cross-transport duplicates match the contract", async t => {
  const events = sdkPlayback(t);
  const batch = await otlpPayload(events);
  // ProtoJSON nulls are preserved in the original log context. Nullable event
  // fields still normalize to the same canonical SDK payload.
  for (const resource of batch.resourceLogs) {
    resource.resource = null;
    resource.schemaUrl = null;
    for (const scope of resource.scopeLogs) {
      scope.scope = null;
      scope.schemaUrl = null;
      for (const log of scope.logRecords) {
        log.attributes.push({ key: "environment", value: { stringValue: null, bytesValue: null } });
        log.attributes.find(attribute => attribute.key === "viewer_id").value.boolValue = null;
      }
    }
  }
  assertRequest("/v1/logs", "post", batch);
  const server = await collector(t);
  await server.post("/v1/logs", gzipSync(JSON.stringify(batch)), 200, { "Content-Encoding": "gzip" });
  assert.deepEqual(await server.post("/v1/logs", "{}", 200), {});
  const page = await server.get("/v1/events");
  assert.deepEqual(page.events.map(record => record.event), events);
  assert.ok(page.events.every(record => record.otlp.log_record.attributes.length > 0));
  for (const [index, record] of page.events.entries()) {
    assert.equal(record.otlp.resource_logs.resource, null);
    assert.equal(record.otlp.scope_logs.scope, null);
    assert.deepEqual(record.otlp.log_record, batch.resourceLogs[index].scopeLogs[0].logRecords[0]);
  }
  await server.post("/v1/events", JSON.stringify(events[0]), 200);
  assert.equal((await server.get("/v1/events")).events.length, events.length);
  await server.post("/v1/logs", JSON.stringify({ resourceLogs: [{ scopeLogs: [{ logRecords: [{}] }] }] }), 400);
  assert.equal((await server.get("/v1/events")).events.length, events.length);
});

test("documented authentication, CORS and protocol errors match the API", async t => {
  const event = sdkPlayback(t)[0];
  const server = await collector(t, { COLLECTOR_MAX_BODY_BYTES: "8192", COLLECTOR_MAX_OTLP_RECORDS: "1" });
  for (const path of ["/v1/events", "/v1/summary", "/v1/videos", "/v1/ingestion"]) {
    await server.get(path, 401, {});
    await server.get(path, 401, { Authorization: `Bearer ${server.keys.ingest_key}` });
    const response = await fetch(`${server.url}${path}`, { headers: { Authorization: `Bearer ${server.keys.read_token}`, Origin: "https://player.example" } });
    assert.equal(response.headers.get("access-control-allow-origin"), null);
    await assertResponse(path, "get", response, 200);
  }
  for (const path of ["/v1/events", "/v1/logs"]) {
    await server.post(path, "{}", 401, { "X-Chunkify-Ingest-Key": "" });
    await server.post(path, "{}", 401, { "X-Chunkify-Ingest-Key": server.keys.read_token });
    await server.post(path, "{}", 403, { Origin: "https://elsewhere.example" });
    await server.post(path, "{}", 415, { "Content-Type": "application/x-protobuf" });
    await server.post(path, "{}", 415, { "Content-Encoding": "br" });
    await server.post(path, "{", 400);
    await server.post(path, " ".repeat(8193), 413);
    const headers = { Origin: "https://player.example", "Access-Control-Request-Method": "POST",
      "Access-Control-Request-Headers": "content-type, x-chunkify-ingest-key" };
    const preflight = await fetch(`${server.url}${path}`, { method: "OPTIONS", headers });
    assert.equal(preflight.headers.get("access-control-allow-origin"), headers.Origin);
    assert.equal(preflight.headers.get("access-control-allow-credentials"), null);
    await assertResponse(path, "options", preflight, 204);
    await assertResponse(path, "options", await fetch(`${server.url}${path}`, { method: "OPTIONS", headers: { ...headers, "Access-Control-Request-Headers": "Authorization" } }), 403);
  }
  await server.post("/v1/events", gzipSync(JSON.stringify(event)), 415, { "Content-Encoding": "gzip" });
  await server.post("/v1/events", JSON.stringify({ ...event, schema_version: 2 }), 400);
  await server.post("/v1/logs", gzipSync(" ".repeat(8193)), 413, { "Content-Encoding": "gzip" });
  await server.post("/v1/logs", "not gzip", 400, { "Content-Encoding": "gzip" });
  const batch = await otlpPayload([event, event]);
  await server.post("/v1/logs", JSON.stringify(batch), 413);
  const response = await fetch(`${server.url}/v1/events`, { method: "POST", body: JSON.stringify(event),
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key, Origin: "https://player.example" } });
  assert.equal(response.headers.get("access-control-allow-origin"), "https://player.example");
  await assertResponse("/v1/events", "post", response, 201);
});

test("rate limit responses use the documented native and OTLP error envelopes", async t => {
  const server = await collector(t, { COLLECTOR_INGEST_PER_SECOND: "1" });
  await server.post("/v1/logs", "{}", 200);
  for (const path of ["/v1/events", "/v1/logs"]) {
    const response = await fetch(`${server.url}${path}`, { method: "POST", body: "{}",
      headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key } });
    assert.equal(response.headers.get("retry-after"), "1");
    await assertResponse(path, "post", response, 429);
  }
});

test("experimental queue responses and committed visibility follow the same contract", async t => {
  const events = sdkPlayback(t);
  const server = await collector(t, { COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE: "true", COLLECTOR_EXPERIMENTAL_READ_WORKER: "true", COLLECTOR_BATCH_DELAY_MS: "200" });
  const start = events[0];
  assert.deepEqual(await server.post("/v1/events", JSON.stringify(start), 202), { accepted: true, queued: true });
  assert.deepEqual(await server.post("/v1/events", JSON.stringify(start), 202), { accepted: true, queued: true });
  const batch = await otlpPayload(events);
  await server.post("/v1/logs", JSON.stringify(batch), 200);
  let status = await server.get("/v1/ingestion");
  assert.equal(status.mode, "durable_queue");
  const deadline = Date.now() + 10000;
  while (status.queue.pending_requests && Date.now() < deadline) {
    await delay(20);
    status = await server.get("/v1/ingestion");
  }
  assert.equal(status.queue.pending_requests, 0);
  assert.equal(status.queue.failed, false);
  const page = await server.get("/v1/events");
  assert.deepEqual(page.events.map(record => record.event), events);
  assert.equal(page.events[0].otlp, undefined, "the first native delivery keeps its original transport context");
  assert.equal((await server.get("/v1/summary")).views, 1);
});
