import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { attachAnalytics } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";

const binary = process.env.COLLECTOR_BINARY ?? resolve("services/analytics-collector/target/debug/chunkify-analytics-collector");

async function collector(t, settings = {}) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-collector-test-"));
  let server;
  t.after(async () => { await server?.stop(); await rm(directory, { recursive: true, force: true }); });
  const env = { ...process.env, COLLECTOR_DATA_DIR: directory, COLLECTOR_BIND: "127.0.0.1:0",
    COLLECTOR_ALLOWED_ORIGINS: "https://player.example", ...settings };
  const init = spawn(binary, ["init"], { env, stdio: "pipe" });
  let errors = "";
  init.stderr.on("data", (chunk) => { errors += chunk; });
  assert.equal((await once(init, "exit"))[0], 0, errors);
  const keys = JSON.parse(await readFile(join(directory, "keys.json"), "utf8"));
  let child;
  let exited;
  server = {
    keys,
    async start() {
      child = spawn(binary, ["serve"], { env, stdio: "pipe" });
      exited = once(child, "exit");
      let output = "";
      errors = "";
      child.stderr.on("data", (chunk) => { errors += chunk; });
      server.url = await new Promise((resolveReady, reject) => {
        const timeout = setTimeout(() => reject(new Error(`Collector startup timed out: ${errors}`)), 10000);
        child.once("error", reject);
        child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Collector exited: ${errors}`)); });
        child.stdout.on("data", (chunk) => {
          output += chunk;
          const match = output.match(/Listening on (http:\/\/\S+)/);
          if (match) { clearTimeout(timeout); resolveReady(match[1]); }
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
    async send(event) {
      await createHttpAdapter(`${server.url}/v1/events`, { headers: { "X-Chunkify-Ingest-Key": keys.ingest_key } }).send(event);
    },
  };
  await server.start();
  return server;
}

function sdkEvents(videoId = "video-a", viewerId = "viewer-a") {
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const events = [];
  const session = attachAnalytics(media, { videoId, viewerId,
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
  const server = await collector(t, { COLLECTOR_INGEST_PER_SECOND: "1" });
  const event = sdkEvents()[0];
  const responses = await Promise.all(Array.from({ length: 8 }, () => fetch(`${server.url}/v1/events`, {
    method: "POST", headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key },
    body: JSON.stringify({ ...event, event_id: crypto.randomUUID() }),
  })));
  assert.equal(responses.filter((response) => response.status === 201).length, 1);
  assert.equal(responses.filter((response) => response.status === 429).length, 7);
  assert.equal(responses.find((response) => response.status === 429).headers.get("retry-after"), "1");
  assert.equal((await server.get("/v1/events")).events.length, 1);
  assert.deepEqual(await (await fetch(`${server.url}/healthz`)).json(), { status: "ok" });
});
