import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { cp, mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { createServer, request as httpRequest } from "node:http";
import { gzipSync } from "node:zlib";
import { setTimeout as delay } from "node:timers/promises";
import { join, resolve } from "node:path";
import test from "node:test";
import { chromium } from "@playwright/test";
import { attachAnalytics } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";

const binary = process.env.COLLECTOR_BINARY ?? resolve("services/analytics-collector/target/debug/chunkify-analytics-collector");

// Capture the public adapter's actual HTTP output for tests of gateway batching.
async function otlpRequests(events, options) {
  const requests = [];
  const receiver = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    requests.push(JSON.parse(Buffer.concat(chunks).toString()));
    response.writeHead(200, { "Content-Type": "application/json" }).end("{}");
  });
  receiver.listen(0, "127.0.0.1");
  await once(receiver, "listening");
  try {
    const adapter = createOtlpAdapter(`http://127.0.0.1:${receiver.address().port}/v1/logs`, options);
    for (const event of events) await adapter.send(event);
    return requests;
  } finally {
    await new Promise((resolveClose) => receiver.close(resolveClose));
  }
}

function postOtlp(server, body, headers = {}) {
  return fetch(`${server.url}/v1/logs`, { method: "POST", body,
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key, ...headers },
  });
}

async function runCommand(args, env) {
  const child = spawn(binary, args, { env, stdio: "pipe", timeout: 10000, killSignal: "SIGKILL" });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => { stdout += chunk; });
  child.stderr.on("data", (chunk) => { stderr += chunk; });
  const [code] = await once(child, "close");
  return { code, stdout, stderr };
}

async function collector(t, settings = {}, initialBinary = binary) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-collector-test-"));
  let server;
  t.after(async () => { await server?.stop(); await rm(directory, { recursive: true, force: true }); });
  const env = { ...process.env, COLLECTOR_DATA_DIR: directory, COLLECTOR_BIND: "127.0.0.1:0",
    COLLECTOR_ALLOWED_ORIGINS: "https://player.example", COLLECTOR_STORE_IP: "true",
    COLLECTOR_TRUSTED_PROXIES: "", ...settings };
  const init = spawn(initialBinary, ["init"], { env, stdio: "pipe" });
  let errors = "";
  init.stderr.on("data", (chunk) => { errors += chunk; });
  assert.equal((await once(init, "exit"))[0], 0, errors);
  const keys = JSON.parse(await readFile(join(directory, "keys.json"), "utf8"));
  let child;
  let exited;
  server = {
    directory,
    keys,
    command: (args, overrides = {}) => runCommand(args, { ...env, ...overrides }),
    async reloadKeys() {
      Object.assign(keys, JSON.parse(await readFile(join(directory, "keys.json"), "utf8")));
    },
    async start(executable = binary, overrides = {}) {
      child = spawn(executable, ["serve"], { env: { ...env, ...overrides }, stdio: "pipe" });
      exited = once(child, "exit");
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
      if (!child || child.exitCode !== null || child.signalCode !== null) return;
      child.kill(signal);
      const [code, receivedSignal] = await exited;
      if (signal === "SIGTERM") assert.equal(code, 0, `${receivedSignal}: ${errors}`);
    },
    async get(path) {
      const response = await fetch(`${server.url}${path}`, { headers: { Authorization: `Bearer ${keys.read_token}` } });
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    },
    async send(event, headers = {}) {
      await createHttpAdapter(`${server.url}/v1/events`, { headers: { "X-Chunkify-Ingest-Key": keys.ingest_key, ...headers } }).send(event);
    },
    async sendOtlp(event, options = {}) {
      await createOtlpAdapter(`${server.url}/v1/logs`, {
        ...options, headers: { "X-Chunkify-Ingest-Key": keys.ingest_key, ...options.headers },
      }).send(event);
    },
  };
  await server.start(initialBinary);
  return server;
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

test("connection IP is stored separately and cannot be replaced by event fields or untrusted headers", async (t) => {
  const server = await collector(t);
  const event = { ...sdkEvents()[0], event_context: { client_ip: "203.0.113.99" } };
  await server.send(event, { "X-Forwarded-For": "198.51.100.99", "X-Real-IP": "198.51.100.99",
    "Forwarded": "for=198.51.100.99", "CF-Connecting-IP": "198.51.100.99" });
  const stored = await server.get("/v1/events");
  assert.deepEqual(stored.events[0].event, event);
  assert.deepEqual(stored.events[0].event_context, {
    schema_version: 1, peer_ip: "127.0.0.1", client_ip: "127.0.0.1", client_ip_source: "connection",
  });
  await server.sendOtlp(event);
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), stored);
});

test("IP collection can be disabled without rewriting earlier events or enriching retries", async (t) => {
  const server = await collector(t, { COLLECTOR_STORE_IP: "false" });
  const events = sdkEvents();
  await server.send(events[0]);
  await server.sendOtlp(events[1]);
  const withoutIp = (await server.get("/v1/events")).events;
  assert.ok(withoutIp.every((record) => !Object.hasOwn(record, "event_context")));
  await server.stop();
  await server.start(binary, { COLLECTOR_STORE_IP: "true" });
  for (const event of events) await server.send(event);
  const added = sdkEvents("with-ip")[0];
  await server.send(added);
  const withIp = (await server.get("/v1/events")).events;
  assert.deepEqual(withIp.slice(0, 2), withoutIp);
  assert.equal(withIp[2].event_context.client_ip, "127.0.0.1");
  await server.stop();
  await server.start(binary, { COLLECTOR_STORE_IP: "false" });
  await server.send(added);
  assert.deepEqual((await server.get("/v1/events")).events, withIp);
  await server.sendOtlp(sdkEvents("disabled-again")[0]);
  assert.equal((await server.get("/v1/events")).events.at(-1).event_context, undefined);
});

test("flat event context copies browser fields and reserves IP fields for the collector", async (t) => {
  const server = await collector(t);
  const event = { ...sdkEvents()[0], browser_context: {
    schema_version: 1, browser_name: "Firefox", os_name: "Linux", device_type: "desktop",
    viewport_width_px: 960, client_ip: "203.0.113.99", peer_ip: "203.0.113.99",
    browser_version: { invalid: "nested" },
    client_ip_source: "forged", extra: "preserve only in original payload",
  } };
  await server.send(event);
  const stored = (await server.get("/v1/events")).events[0];
  assert.deepEqual(stored.event, event);
  assert.deepEqual(stored.event_context, {
    schema_version: 1, browser_name: "Firefox", os_name: "Linux", device_type: "desktop", viewport_width_px: 960,
    peer_ip: "127.0.0.1", client_ip: "127.0.0.1", client_ip_source: "connection",
  });
  await server.stop("SIGKILL");
  await server.start(binary, { COLLECTOR_STORE_IP: "false" });
  const withoutIp = { ...sdkEvents()[0], browser_context: event.browser_context };
  await server.sendOtlp(withoutIp);
  const records = (await server.get("/v1/events")).events;
  assert.deepEqual(records[0], stored);
  assert.deepEqual(records[1].event, withoutIp);
  assert.deepEqual(records[1].event_context, {
    schema_version: 1, browser_name: "Firefox", os_name: "Linux", device_type: "desktop", viewport_width_px: 960,
  });
});

test("trusted proxies resolve the first untrusted address from the right", async (t) => {
  const server = await collector(t, { COLLECTOR_TRUSTED_PROXIES: "127.0.0.1,10.20.0.0/16,2001:db8:abcd::/48" });
  const event = sdkEvents()[0];
  await server.send(event, { "X-Forwarded-For": "203.0.113.99, 198.51.100.8, 10.20.1.2, 2001:db8:abcd::1" });
  const stored = (await server.get("/v1/events")).events;
  assert.deepEqual(stored[0].event, event);
  assert.deepEqual(stored[0].event_context, {
    schema_version: 1, peer_ip: "127.0.0.1", client_ip: "198.51.100.8", client_ip_source: "x_forwarded_for",
  });
  await server.sendOtlp(event, { headers: { "X-Forwarded-For": "203.0.113.33" } });
  assert.deepEqual((await server.get("/v1/events")).events, stored, "a retry cannot replace the first request context");
  assert.ok(!server.logs().includes("198.51.100.8"));
});

test("proxy headers are ignored unless the connection peer matches the configured trust list", async (t) => {
  const server = await collector(t, { COLLECTOR_TRUSTED_PROXIES: "10.0.0.0/8,::1" });
  await server.sendOtlp(sdkEvents()[0], { headers: { "X-Forwarded-For": "203.0.113.10, 10.1.2.3" } });
  const context = (await server.get("/v1/events")).events[0].event_context;
  assert.equal(context.client_ip, "127.0.0.1");
  assert.equal(context.client_ip_source, "connection");
});

test("trusted forwarding accepts literal IPs and leaves invalid or missing client addresses unresolved", async (t) => {
  const server = await collector(t, { COLLECTOR_TRUSTED_PROXIES: "127.0.0.0/8,10.20.0.0/16,2001:db8:abcd::/48" });
  const cases = [
    ["198.51.100.2", "198.51.100.2"],
    ["2001:db8:beef::5, 2001:db8:abcd::1", "2001:db8:beef::5"],
    ["::ffff:198.51.100.6, ::ffff:10.20.1.2", "198.51.100.6"],
    ["203.0.113.5, 10.21.1.2", "10.21.1.2"],
    [undefined, null], ["", null], ["unknown", null], ["for=198.51.100.2", null],
    ["198.51.100.2:443", null], ["[2001:db8::2]", null], ["fe80::1%en0", null],
    ["198.51.100.2,", null], [",198.51.100.2", null], ["malformed, 198.51.100.2", null],
    ["127.0.0.1, 10.20.1.2", null],
    [["198.51.100.2", ...Array(31).fill("10.20.1.2")].join(","), "198.51.100.2"],
    [["198.51.100.2", ...Array(32).fill("10.20.1.2")].join(","), null],
    [`198.51.100.2,${" ".repeat(4096)}10.20.1.2`, null],
  ];
  for (const [forwarded, expected] of cases) {
    const event = sdkEvents()[0];
    await server.send(event, {
      ...(forwarded === undefined ? {} : { "X-Forwarded-For": forwarded }),
      "Forwarded": "for=192.0.2.99", "X-Real-IP": "192.0.2.99", "CF-Connecting-IP": "192.0.2.99",
    });
    const context = (await server.get("/v1/events")).events.at(-1).event_context;
    assert.deepEqual(context, { schema_version: 1, peer_ip: "127.0.0.1", client_ip: expected,
      client_ip_source: expected ? "x_forwarded_for" : null }, `forwarded case ${cases.findIndex((entry) => entry[0] === forwarded)}`);
  }
});

test("multiple X-Forwarded-For header lines form one ordered chain", async (t) => {
  const server = await collector(t, { COLLECTOR_TRUSTED_PROXIES: "127.0.0.1,10.20.0.0/16" });
  const body = JSON.stringify(sdkEvents()[0]);
  const status = await new Promise((resolveStatus, reject) => {
    const request = httpRequest(`${server.url}/v1/events`, { method: "POST", headers: [
      "Content-Type", "application/json", "Content-Length", String(Buffer.byteLength(body)),
      "X-Chunkify-Ingest-Key", server.keys.ingest_key,
      "X-Forwarded-For", "203.0.113.99, 198.51.100.7", "X-Forwarded-For", "10.20.1.2",
    ] }, (response) => { response.resume(); response.on("end", () => resolveStatus(response.statusCode)); });
    request.on("error", reject);
    request.end(body);
  });
  assert.equal(status, 201);
  assert.equal((await server.get("/v1/events")).events[0].event_context.client_ip, "198.51.100.7");
});

test("IPv6 connection addresses are captured and can be trusted explicitly", async (t) => {
  const server = await collector(t, { COLLECTOR_BIND: "[::1]:0" });
  await server.send(sdkEvents()[0], { "X-Forwarded-For": "2001:db8::4" });
  assert.deepEqual((await server.get("/v1/events")).events[0].event_context, {
    schema_version: 1, peer_ip: "::1", client_ip: "::1", client_ip_source: "connection",
  });
  await server.stop();
  await server.start(binary, { COLLECTOR_TRUSTED_PROXIES: "::1/128" });
  await server.send(sdkEvents()[0], { "X-Forwarded-For": "2001:db8::4" });
  assert.deepEqual((await server.get("/v1/events")).events.at(-1).event_context, {
    schema_version: 1, peer_ip: "::1", client_ip: "2001:db8::4", client_ip_source: "x_forwarded_for",
  });
});

test("invalid IP configuration fails at startup without changing events or credentials", async (t) => {
  const server = await collector(t);
  await server.send(sdkEvents()[0]);
  const before = await server.get("/v1/events");
  const keys = { ...server.keys };
  await server.stop();
  for (const settings of [
    ...["0", "1", "yes", "FALSE", ""].map((value) => ({ COLLECTOR_STORE_IP: value })),
    ...["*", "localhost", "127.0.0.1:80", "127.0.0.1/33", "::1/129", "127.0.0.1,", "::ffff:127.0.0.0/104"]
      .map((value) => ({ COLLECTOR_TRUSTED_PROXIES: value })),
  ]) {
    const result = await server.command(["serve"], settings);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, new RegExp(Object.keys(settings)[0]));
  }
  await server.reloadKeys();
  assert.deepEqual(server.keys, keys);
  await server.start(binary, { COLLECTOR_TRUSTED_PROXIES: " ::ffff:127.0.0.1 , 2001:db8::/32 " });
  assert.deepEqual(await server.get("/v1/events"), before);
  await server.send(sdkEvents()[0], { "X-Forwarded-For": "198.51.100.11" });
  assert.equal((await server.get("/v1/events")).events.at(-1).event_context.client_ip, "198.51.100.11");
});

test("the browser Chunkify adapter persists context through authenticated ingestion, retries, and restart", async (t) => {
  const fixture = createServer(async (request, response) => {
    if (request.url === "/") {
      response.writeHead(200, { "Content-Type": "text/html" }).end("<!doctype html><video></video>");
    } else if (/^\/[a-z-]+\.js$/.test(request.url ?? "")) {
      try {
        const source = await readFile(resolve("packages/analytics/dist", request.url.slice(1)));
        response.writeHead(200, { "Content-Type": "text/javascript" }).end(source);
      } catch { response.writeHead(404).end(); }
    } else { response.writeHead(404).end(); }
  });
  let browser;
  t.after(async () => { await browser?.close(); await new Promise((done) => fixture.close(done)); });
  fixture.listen(0, "127.0.0.1");
  await once(fixture, "listening");
  const origin = `http://127.0.0.1:${fixture.address().port}`;
  const server = await collector(t, { COLLECTOR_ALLOWED_ORIGINS: origin });
  browser = await chromium.launch();
  const page = await browser.newPage({ locale: "fr-FR", timezoneId: "Europe/Paris", viewport: { width: 960, height: 540 } });
  await page.goto(origin);
  const destination = { endpoint: `${server.url}/v1/events`, ingestKey: server.keys.ingest_key };
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
    assert.deepEqual(event_context, { ...event.browser_context, peer_ip: "127.0.0.1", client_ip: "127.0.0.1", client_ip_source: "connection" });
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

test("retention expires native and OTLP events by receipt time and updates summaries", async (t) => {
  const server = await collector(t, { COLLECTOR_RETENTION_SECONDS: "2", COLLECTOR_RETENTION_INTERVAL_SECONDS: "1" });
  const past = { ...sdkEvents("past-video")[0], occurred_at: "2000-01-01T00:00:00Z" };
  const future = { ...sdkEvents("future-video")[0], occurred_at: "2099-01-01T00:00:00Z" };
  await server.send(past);
  await server.sendOtlp(future);
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), [past, future]);
  assert.equal((await server.get("/v1/summary")).views, 2);
  const deadline = Date.now() + 7000;
  while ((await server.get("/v1/events")).events.length && Date.now() < deadline) await delay(200);
  assert.deepEqual((await server.get("/v1/events")).events, []);
  assert.deepEqual(await server.get("/v1/summary"), { views: 0, unique_viewers: 0, total_playing_time_ms: 0 });
  assert.deepEqual((await server.get("/v1/videos")).videos, []);
  // Expired event IDs can be received again, with a new retention lifetime.
  await server.sendOtlp(future);
  assert.equal((await server.get("/v1/events")).events.length, 1);
  await server.stop("SIGKILL");
  await server.start(binary, { COLLECTOR_RETENTION_SECONDS: "0" });
  assert.equal((await server.get("/v1/summary")).views, 1);
});

test("retention defaults to 30 days, can be disabled, and purges at startup", async (t) => {
  const server = await collector(t);
  assert.match(server.output, /Retention: 2592000 seconds/);
  const event = { ...sdkEvents()[0], occurred_at: "2000-01-01T00:00:00Z" };
  await server.sendOtlp(event);
  await server.stop();
  await delay(1200);
  await server.start(binary, { COLLECTOR_RETENTION_SECONDS: "0", COLLECTOR_RETENTION_INTERVAL_SECONDS: "1" });
  assert.match(server.output, /Retention: disabled/);
  await delay(1200);
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), [event]);
  await server.stop();
  await server.start(binary, { COLLECTOR_RETENTION_SECONDS: "1" });
  assert.deepEqual((await server.get("/v1/events")).events, []);
  assert.equal((await server.get("/v1/summary")).views, 0);
});

test("offline viewer deletion removes both transports across videos and preserves other viewers", async (t) => {
  const server = await collector(t);
  const viewerId = "viewer's-id; SELECT * FROM events";
  const first = sdkEvents("video-a", viewerId);
  const second = sdkEvents("video-b", viewerId);
  const other = sdkEvents("video-a", "keep-me");
  const anonymous = sdkEvents("video-b", null);
  first.at(-1).playing_time_ms = 1000;
  second.at(-1).playing_time_ms = 2000;
  other.at(-1).playing_time_ms = 3000;
  anonymous.at(-1).playing_time_ms = 500;
  for (const event of [...first, ...other]) await server.send(event);
  for (const event of [...second, ...anonymous]) await server.sendOtlp(event);
  const original = (await server.get("/v1/events")).events;
  const busy = await server.command(["delete-viewer", viewerId]);
  assert.notEqual(busy.code, 0);
  assert.match(busy.stderr, /Stop the collector/);
  assert.deepEqual((await server.get("/v1/events")).events, original);
  await server.stop("SIGKILL");
  const deleted = await server.command(["delete-viewer", viewerId]);
  assert.equal(deleted.code, 0, deleted.stderr);
  assert.deepEqual(JSON.parse(deleted.stdout), { deleted_events: 6 });
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events,
    original.filter((record) => record.event.viewer_id !== viewerId));
  assert.deepEqual(await server.get("/v1/summary"), { views: 2, unique_viewers: 1, total_playing_time_ms: 3500 });
  assert.deepEqual((await server.get("/v1/videos")).videos, [
    { video_id: "video-a", views: 1, unique_viewers: 1, total_playing_time_ms: 3000 },
    { video_id: "video-b", views: 1, unique_viewers: 0, total_playing_time_ms: 500 },
  ]);
  await server.stop();
  const repeated = await server.command(["delete-viewer", viewerId]);
  assert.equal(repeated.code, 0, repeated.stderr);
  assert.deepEqual(JSON.parse(repeated.stdout), { deleted_events: 0 });
  await server.start();
  // Deletion removes stored data; it does not block future collection of this ID.
  await server.sendOtlp(second[0]);
  assert.equal((await server.get("/v1/summary")).views, 3);
});

test("offline key rotation replaces each credential independently without exposing keys or losing data", async (t) => {
  const server = await collector(t);
  const originalKeys = { ...server.keys };
  const event = sdkEvents()[0];
  await server.send(event);
  const original = await server.get("/v1/events");
  const busy = await server.command(["rotate-key", "ingest"]);
  assert.notEqual(busy.code, 0);
  assert.match(busy.stderr, /Stop the collector/);
  await server.reloadKeys();
  assert.deepEqual(server.keys, originalKeys);
  await server.stop();
  const invalid = await server.command(["rotate-key", "unknown"]);
  assert.notEqual(invalid.code, 0);
  const ingest = await server.command(["rotate-key", "ingest"]);
  assert.equal(ingest.code, 0, ingest.stderr);
  await server.reloadKeys();
  assert.notEqual(server.keys.ingest_key, originalKeys.ingest_key);
  assert.equal(server.keys.read_token, originalKeys.read_token);
  assert.ok(server.keys.ingest_key.length >= 32);
  for (const key of [...Object.values(originalKeys), ...Object.values(server.keys)]) {
    assert.equal(`${ingest.stdout}${ingest.stderr}`.includes(key), false);
  }
  if (process.platform !== "win32") {
    assert.equal((await stat(join(server.directory, "keys.json"))).mode & 0o777, 0o600);
  }
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), original);
  const deniedWrite = await fetch(`${server.url}/v1/events`, { method: "POST",
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": originalKeys.ingest_key },
    body: JSON.stringify(event),
  });
  assert.equal(deniedWrite.status, 401);
  assert.equal((await postOtlp(server, "{}", { "X-Chunkify-Ingest-Key": originalKeys.ingest_key })).status, 401);
  const rotatedIngest = server.keys.ingest_key;
  await server.sendOtlp(sdkEvents("after-ingest-rotation")[0]);
  await server.stop("SIGKILL");
  const read = await server.command(["rotate-key", "read"]);
  assert.equal(read.code, 0, read.stderr);
  await server.reloadKeys();
  assert.notEqual(server.keys.read_token, originalKeys.read_token);
  assert.equal(server.keys.ingest_key, rotatedIngest);
  for (const key of [...Object.values(originalKeys), ...Object.values(server.keys)]) {
    assert.equal(`${read.stdout}${read.stderr}`.includes(key), false);
  }
  await server.start();
  const deniedRead = await fetch(`${server.url}/v1/events`, {
    headers: { Authorization: `Bearer ${originalKeys.read_token}` },
  });
  assert.equal(deniedRead.status, 401);
  await server.send(sdkEvents("after-read-rotation")[0]);
  assert.equal((await server.get("/v1/summary")).views, 3);
});

test("a stopped data-directory backup restores events, OTLP context, summaries and keys", async (t) => {
  const events = sdkPlayback(t);
  const server = await collector(t);
  const backups = await mkdtemp(join(tmpdir(), "chunkify-collector-backup-test-"));
  t.after(async () => { await server.stop(); await rm(backups, { recursive: true, force: true }); });
  for (const [index, event] of events.entries()) {
    if (index % 2) await server.sendOtlp(event);
    else await server.send(event);
  }
  const originalKeys = { ...server.keys };
  const originalEvents = await server.get("/v1/events");
  const originalSummary = await server.get("/v1/summary");
  await server.stop();
  await cp(server.directory, join(backups, "snapshot"), { recursive: true, force: false, errorOnExist: true });
  // Changes made after the backup must not appear in the restored installation.
  const rotation = await server.command(["rotate-key", "ingest"]);
  assert.equal(rotation.code, 0, rotation.stderr);
  await server.reloadKeys();
  assert.notEqual(server.keys.ingest_key, originalKeys.ingest_key);
  await server.start();
  await server.send(sdkEvents("after-backup")[0]);
  await server.stop();
  const restored = join(backups, "restored");
  await cp(join(backups, "snapshot"), restored, { recursive: true, force: false, errorOnExist: true });
  const restoredKeys = JSON.parse(await readFile(join(restored, "keys.json"), "utf8"));
  assert.deepEqual(restoredKeys, originalKeys);
  Object.assign(server.keys, restoredKeys);
  await server.start(binary, { COLLECTOR_DATA_DIR: restored });
  assert.deepEqual(await server.get("/v1/events"), originalEvents);
  assert.deepEqual(await server.get("/v1/summary"), originalSummary);
  await server.sendOtlp(events[0]);
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
    { COLLECTOR_RETENTION_SECONDS: "-1" },
    { COLLECTOR_RETENTION_SECONDS: "1.5" },
    { COLLECTOR_RETENTION_SECONDS: "18446744073709551615" },
    { COLLECTOR_RETENTION_INTERVAL_SECONDS: "0" },
    { COLLECTOR_RETENTION_INTERVAL_SECONDS: "-1" },
    { COLLECTOR_RETENTION_INTERVAL_SECONDS: "86401" },
  ]) {
    const result = await server.command(["serve"], settings);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, new RegExp(Object.keys(settings)[0]));
  }
  await server.reloadKeys();
  assert.deepEqual(server.keys, keys);
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), before);
});

test("ingestion validates SDK payloads and separates browser writes from private reads", async (t) => {
  const server = await collector(t, { COLLECTOR_MAX_BODY_BYTES: "8192" });
  const event = sdkEvents()[0];
  const post = (body, headers = {}) => fetch(`${server.url}/v1/events`, {
    method: "POST", headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key, ...headers },
    body: JSON.stringify(body),
  });
  assert.equal((await post(event, { "X-Chunkify-Ingest-Key": server.keys.read_token })).status, 401);
  assert.equal((await fetch(`${server.url}/v1/events`, {
    headers: { Authorization: `Bearer ${server.keys.ingest_key}` },
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
    Origin: "https://player.example", Authorization: `Bearer ${server.keys.read_token}`,
  } });
  assert.equal(read.headers.get("access-control-allow-origin"), null);
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
      headers: { Authorization: `Bearer ${server.keys.read_token}` },
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
  assert.deepEqual(videos.videos, [{ video_id: "video-a", views: 2, unique_viewers: 1, total_playing_time_ms: 6000 }]);
  assert.ok(videos.next_cursor);
  await server.send(at(second.at(-1), "2026-10-15T12:00:03Z", { sequence: 4, playing_time_ms: 2500 }));
  const next = await server.get(`/v1/videos?limit=1&cursor=${encodeURIComponent(videos.next_cursor)}`);
  assert.deepEqual(next.videos, [{ video_id: "video-b", views: 1, unique_viewers: 0, total_playing_time_ms: 500 }]);
  assert.equal(next.next_cursor, null);
  assert.equal((await server.get("/v1/summary")).total_playing_time_ms, 7000);
  await server.stop();
  await server.start();
  assert.equal((await server.get("/v1/summary")).total_playing_time_ms, 7000);
});

test("ingestion rate limits reject excess requests without storing more events", async (t) => {
  const event = sdkEvents()[0];
  const [envelope] = await otlpRequests([event]);
  const server = await collector(t, { COLLECTOR_INGEST_PER_SECOND: "1" });
  const responses = await Promise.all(Array.from({ length: 8 }, (_, index) => fetch(`${server.url}/v1/${index % 2 ? "logs" : "events"}`, {
    method: "POST", headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key },
    body: JSON.stringify(index % 2 ? envelope : event),
  })));
  assert.equal(responses.filter((response) => response.ok).length, 1);
  assert.equal(responses.filter((response) => response.status === 429).length, 7);
  assert.equal(responses.find((response) => response.status === 429).headers.get("retry-after"), "1");
  for (const response of responses.filter((response) => response.status === 429 && response.url.endsWith("/v1/logs"))) {
    assert.equal(typeof (await response.json()).message, "string");
  }
  assert.equal((await server.get("/v1/events")).events.length, 1);
  assert.deepEqual(await (await fetch(`${server.url}/healthz`)).json(), { status: "ok" });
});

test("upgrading a previous collector preserves events and keys and accepts OTLP", {
  skip: !process.env.COLLECTOR_PREVIOUS_BINARY && "Set COLLECTOR_PREVIOUS_BINARY to test a previous executable",
}, async (t) => {
  const server = await collector(t, {}, process.env.COLLECTOR_PREVIOUS_BINARY);
  const events = sdkPlayback(t).map((event, index) => index % 2 ? event : {
    ...event, browser_context: { schema_version: 1, browser_name: "Firefox", device_type: "desktop" },
  });
  for (const event of events) await server.send(event);
  const original = await server.get("/v1/events");
  const beforePage = await server.get("/v1/events?limit=2");
  const expected = structuredClone(original);
  for (const record of expected.events) {
    if (record.event.browser_context) record.event_context = record.event.browser_context;
  }
  const summary = await server.get("/v1/summary");
  await server.stop("SIGKILL");
  await server.start();
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), expected);
  const afterPage = await server.get(`/v1/events?limit=2&cursor=${encodeURIComponent(beforePage.next_cursor)}`);
  assert.deepEqual(afterPage.events, expected.events.slice(2, 4), "pre-upgrade cursors retain their event ordering");
  assert.deepEqual(await server.get("/v1/summary"), summary);
  for (const event of events) await server.sendOtlp(event);
  assert.deepEqual(await server.get("/v1/events"), expected, "migration must not invent IP context for old events, even on retry");
  const added = sdkEvents("after-upgrade")[0];
  await server.sendOtlp(added);
  await server.stop("SIGKILL");
  await server.start();
  const upgraded = (await server.get("/v1/events")).events;
  assert.deepEqual(upgraded.map((record) => record.event), [...events, added]);
  assert.deepEqual(upgraded.slice(0, events.length), expected.events);
  assert.equal(upgraded.at(-1).event_context.client_ip, "127.0.0.1");
  assert.equal((await server.get("/v1/summary")).views, 2);
});

test("the OTLP adapter preserves all seven SDK events and produces the same reports as native ingestion", async (t) => {
  const events = sdkPlayback(t);
  const native = await collector(t);
  const otlp = await collector(t);
  for (const event of events) {
    await native.send(event);
    await otlp.sendOtlp(event);
  }
  const records = (await otlp.get("/v1/events")).events;
  assert.deepEqual(records.map((record) => record.event), events);
  for (const record of records) {
    assert.equal(record.otlp.log_record.eventName, record.event.event_name);
    assert.equal(record.otlp.scope_logs.scope.name, "@chunkify/analytics");
    assert.deepEqual(record.otlp.resource_logs, {});
  }
  for (const path of ["/v1/summary", "/v1/videos", "/v1/summary?video_id=video-a&viewer_id=viewer-a"]) {
    assert.deepEqual(await otlp.get(path), await native.get(path));
  }
  // First delivery wins, whichever transport arrives first.
  for (const event of events) {
    await native.sendOtlp(event);
    await otlp.send(event);
    await otlp.sendOtlp(event);
  }
  assert.equal((await native.get("/v1/events")).events.length, events.length);
  assert.deepEqual((await otlp.get("/v1/events")).events, records);
  await otlp.stop("SIGKILL");
  await otlp.start();
  assert.deepEqual((await otlp.get("/v1/events")).events, records);
});

test("OTLP gzip batches retain resource and scope context and deduplicate within a batch", async (t) => {
  const events = [...sdkEvents("video-a"), ...sdkEvents("video-b")]
    .map((event, index) => ({ ...event, browser_context: { schema_version: 1, browser_name: index < 2 ? "Firefox" : "Safari" } }));
  const extra = { enabled: true, fraction: 0.060875000000010004, tags: ["demo", "customer"], details: { count: 4 } };
  const envelopes = await otlpRequests(events, { mapAttributes: (event) => ({ ...event, extra }) });
  const logs = envelopes.map((envelope) => envelope.resourceLogs[0].scopeLogs[0].logRecords[0]);
  // ProtoJSON permits int64 values as JSON numbers as well as decimal strings.
  for (const log of logs) {
    const sequence = log.attributes.find((attribute) => attribute.key === "sequence");
    sequence.value.intValue = Number(sequence.value.intValue);
  }
  const resource = { attributes: [{ key: "service.name", value: { stringValue: "customer-player" } }] };
  const scope = { name: "customer-gateway", version: "1.0" };
  const batch = { futureField: true, resourceLogs: [
    { resource, schemaUrl: "https://example.com/resource", scopeLogs: [
      { scope, schemaUrl: "https://example.com/scope", logRecords: logs.slice(0, 2) },
      { logRecords: logs.slice(2, 3) },
    ] },
    { scopeLogs: [{ logRecords: [...logs.slice(3), logs[0]] }] },
  ] };
  const server = await collector(t, { COLLECTOR_TRUSTED_PROXIES: "127.0.0.1" });
  const response = await postOtlp(server, gzipSync(JSON.stringify(batch)), {
    "Content-Encoding": "gzip", "X-Forwarded-For": "198.51.100.50",
  });
  assert.equal(response.status, 200, await response.clone().text());
  assert.deepEqual(await response.json(), {});
  const stored = (await server.get("/v1/events")).events;
  assert.deepEqual(stored.map((record) => record.event), events.map((event) => ({ ...event, extra })));
  assert.ok(stored.every((record) => record.event_context.client_ip === "198.51.100.50"),
    "the request IP describes the gateway while per-event browser contexts remain distinct");
  assert.deepEqual(stored[0].otlp, {
    resource_logs: { resource, schemaUrl: "https://example.com/resource" },
    scope_logs: { scope, schemaUrl: "https://example.com/scope" }, log_record: logs[0],
  });
  assert.deepEqual(stored[2].otlp.scope_logs, {});
  assert.deepEqual(stored[3].otlp.resource_logs, {});
  assert.deepEqual(await server.get("/v1/summary"), { views: 2, unique_viewers: 1, total_playing_time_ms: 0 });
  assert.equal((await postOtlp(server, JSON.stringify(batch))).status, 200);
  assert.deepEqual((await server.get("/v1/events")).events, stored);
  await server.stop("SIGKILL");
  await server.start();
  assert.deepEqual((await server.get("/v1/events")).events, stored);
});

test("OTLP rejects invalid or excessive exports atomically with protocol errors", async (t) => {
  const server = await collector(t, { COLLECTOR_MAX_BODY_BYTES: "32768", COLLECTOR_MAX_OTLP_RECORDS: "2" });
  const event = sdkEvents()[0];
  const [envelope] = await otlpRequests([event]);
  const log = envelope.resourceLogs[0].scopeLogs[0].logRecords[0];
  const body = JSON.stringify(envelope);
  const batch = (logs) => JSON.stringify({ resourceLogs: [{ scopeLogs: [{ logRecords: logs }] }] });
  async function rejected(payload, status, headers = {}) {
    const response = await postOtlp(server, payload, headers);
    assert.equal(response.status, status, await response.clone().text());
    assert.match(response.headers.get("content-type"), /^application\/json/);
    assert.equal(response.headers.get("cache-control"), "no-store");
    const error = await response.json();
    assert.equal(typeof error.message, "string");
    assert.ok(error.message.length > 0);
    assert.equal(error.error, undefined);
    assert.deepEqual((await server.get("/v1/events")).events, []);
    assert.equal((await server.get("/v1/summary")).views, 0);
  }
  await rejected(body, 401, { "X-Chunkify-Ingest-Key": "" });
  await rejected(body, 401, { "X-Chunkify-Ingest-Key": server.keys.read_token });
  await rejected(body, 403, { Origin: "https://elsewhere.example" });
  await rejected(body, 415, { "Content-Type": "application/x-protobuf" });
  await rejected(body, 415, { "Content-Encoding": "br" });
  await rejected("{", 400);
  await rejected("null", 400);
  await rejected('{"resourceLogs":{}}', 400);
  await rejected(Buffer.from("not gzip"), 400, { "Content-Encoding": "gzip" });
  await rejected(gzipSync(body).subarray(0, -8), 400, { "Content-Encoding": "gzip" });
  await rejected(" ".repeat(32769), 413);
  await rejected(gzipSync(" ".repeat(32769)), 413, { "Content-Encoding": "gzip" });
  await rejected(batch([log, log, log]), 413);
  const invalidAttributes = [
    log.attributes.filter((attribute) => attribute.key !== "video_id"),
    log.attributes.map((attribute) => attribute.key === "schema_version" ? { ...attribute, value: { intValue: "2" } } : attribute),
    log.attributes.map((attribute) => attribute.key === "event_name" ? { ...attribute, value: { stringValue: "unknown" } } : attribute),
    [...log.attributes, log.attributes[0]],
    [...log.attributes, { key: 3, value: { stringValue: "invalid key" } }],
    [...log.attributes, { key: "extra", value: { intValue: "one" } }],
    [...log.attributes, { key: "extra", value: { stringValue: "x", boolValue: true } }],
  ];
  for (const attributes of invalidAttributes) {
    await rejected(batch([log, { ...log, attributes }]), 400);
  }
  await assert.rejects(server.sendOtlp(event, { mapAttributes: () => ({ video_id: "only-one-field" }) }));
  assert.deepEqual((await server.get("/v1/events")).events, []);
  for (const empty of [{}, { resourceLogs: [] }, { resourceLogs: [{ scopeLogs: [{ logRecords: [] }] }] }]) {
    const response = await postOtlp(server, JSON.stringify(empty));
    assert.equal(response.status, 200);
    assert.deepEqual(await response.json(), {});
  }
  const preflight = await fetch(`${server.url}/v1/logs`, { method: "OPTIONS", headers: {
    Origin: "https://player.example", "Access-Control-Request-Method": "POST",
    "Access-Control-Request-Headers": "content-type,content-encoding,x-chunkify-ingest-key",
  } });
  assert.equal(preflight.status, 204);
  assert.equal(preflight.headers.get("access-control-allow-origin"), "https://player.example");
  assert.match(preflight.headers.get("access-control-allow-headers"), /content-encoding/i);
  const accepted = await postOtlp(server, body, { Origin: "https://player.example" });
  assert.equal(accepted.status, 200);
  assert.equal(accepted.headers.get("access-control-allow-origin"), "https://player.example");
  assert.deepEqual((await server.get("/v1/events")).events.map((record) => record.event), [event]);
});
