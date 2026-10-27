import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { gzipSync } from "node:zlib";
import test from "node:test";
import { attachAnalytics } from "@chunkify/analytics";
import { contract, component, assertRequest, assertResponse } from "./openapi-support.mjs";
import { collectorEnvironment, writeConfig } from "./configuration.mjs";

const binary = process.env.COLLECTOR_BINARY ?? resolve("target/debug/chunkify-analytics");

async function collector(t, settings = {}) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-openapi-test-"));
  const inherited = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("COLLECTOR_")));
  const configuration = { ...inherited, "data_dir": directory, "server.bind": "127.0.0.1:0",
    "server.allowed_origins": "https://player.example", "retention.seconds": "0", ...settings };
  await writeConfig(directory, configuration);
  const env = collectorEnvironment(inherited);
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
  const init = spawn(binary, ["init"], { env, cwd: directory, stdio: "pipe" });
  init.stdout.resume();
  init.stderr.on("data", chunk => { logs += chunk; });
  assert.equal((await once(init, "close"))[0], 0, logs);
  const keys = JSON.parse(await readFile(join(directory, "keys.json"), "utf8"));
  child = spawn(binary, ["serve"], { env, cwd: directory, stdio: "pipe" });
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
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": keys.public_key, ...headers } });
      return assertResponse(path, "post", response, status);
    },
    async get(path, status = 200, headers = { Authorization: `Bearer ${keys.secret_key}` }) {
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
  let descending = await server.get(`/v1/events?${filters}&limit=2&order=desc`);
  const descCursor = descending.next_cursor;
  const reversed = [...descending.events];
  while (descending.next_cursor) {
    descending = await server.get(`/v1/events?${filters}&limit=3&order=desc&cursor=${encodeURIComponent(descending.next_cursor)}`);
    reversed.push(...descending.events);
  }
  assert.deepEqual(reversed.map(record => record.event), events.toReversed());
  await server.get(`/v1/events?${filters}&cursor=${encodeURIComponent(descCursor)}`, 400);
  await server.get(`/v1/events?${filters}&order=desc&cursor=${encodeURIComponent(firstCursor)}`, 400);
  for (const query of ['order=sideways', 'order=asc&order=desc']) await server.get(`/v1/events?${query}`, 400);
  for (const route of ['summary', 'quality', 'engagement']) await server.get(`/v1/${route}?order=desc`, 400);

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

test("removed ingestion routes return 404 without accepting events", async t => {
  const server = await collector(t);
  for (const headers of [{}, {
    Authorization: `Bearer ${server.keys.secret_key}`,
    "X-Chunkify-Ingest-Key": server.keys.public_key,
    Origin: "https://player.example",
    "Access-Control-Request-Method": "POST",
    "Access-Control-Request-Headers": "content-type, x-chunkify-ingest-key",
  }]) {
    for (const [path, methods] of [["/v1/logs", ["GET", "POST", "OPTIONS"]], ["/v1/ingestion", ["GET"]]]) {
      for (const method of methods) {
        const response = await fetch(`${server.url}${path}`, { method, headers });
        assert.equal(response.status, 404);
        assert.equal(response.headers.get("access-control-allow-origin"), null);
      }
    }
  }
  assert.deepEqual((await server.get("/v1/events")).events, []);
});

test("documented authentication, CORS and protocol errors match the API", async t => {
  const event = sdkPlayback(t)[0];
  const server = await collector(t, { "server.max_body_bytes": "8192" });
  for (const path of ["/v1/events", "/v1/summary", "/v1/quality", "/v1/engagement", "/v1/videos"]) {
    await server.get(path, 401, {});
    await server.get(path, 401, { Authorization: `Bearer ${server.keys.public_key}` });
    const response = await fetch(`${server.url}${path}`, { headers: { Authorization: `Bearer ${server.keys.secret_key}`, Origin: "https://player.example" } });
    assert.equal(response.headers.get("access-control-allow-origin"), null);
    await assertResponse(path, "get", response, 200);
  }
  for (const path of ["/v1/events"]) {
    await server.post(path, "{}", 401, { "X-Chunkify-Ingest-Key": "" });
    await server.post(path, "{}", 401, { "X-Chunkify-Ingest-Key": server.keys.secret_key });
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
  const response = await fetch(`${server.url}/v1/events`, { method: "POST", body: JSON.stringify(event),
    headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key, Origin: "https://player.example" } });
  assert.equal(response.headers.get("access-control-allow-origin"), "https://player.example");
  await assertResponse("/v1/events", "post", response, 201);
});

test("rate limit responses use the documented event error envelope", async t => {
  const server = await collector(t, { "server.ingest_per_second": "1" });
  await server.post("/v1/events", JSON.stringify(contract.paths["/v1/events"].post.requestBody.content["application/json"].example), 201);
  for (const path of ["/v1/events"]) {
    const response = await fetch(`${server.url}${path}`, { method: "POST", body: "{}",
      headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key } });
    assert.equal(response.headers.get("retry-after"), "1");
    await assertResponse(path, "post", response, 429);
  }
});


for (const separateReads of [false, true]) {
  test(`timeseries returns bounded UTC start cohorts with summary parity (separate reads: ${separateReads})`, async t => {
    const server = await collector(t, { "experimental.read_worker": String(separateReads), "duckdb.threads": "1" });
    const sample = contract.paths["/v1/events"].post.requestBody.content["application/json"].example;
    const start = (at, viewer, video = "video-a", browser = "Chrome") => ({
      ...sample, event_id: randomUUID(), view_id: randomUUID(), sequence: 1,
      occurred_at: at, video_id: video, viewer_id: viewer,
      browser_context: { schema_version: 1, browser_name: browser, os_name: "Linux", device_type: "desktop" },
    });
    const measurement = (event, sequence, kind, playing, at = event.occurred_at) => ({
      ...event, event_id: randomUUID(), sequence, event_name: `chunkify.video.${kind}`,
      occurred_at: at, playing_time_ms: playing, reason: kind === "view_ended" ? "destroyed" : null,
    });
    const a = start("2026-10-19T10:30:00Z", "returning-viewer");
    const b = start("2026-10-19T11:00:00Z", "returning-viewer", "video-b", "Firefox");
    const anonymous = start("2026-10-19T11:59:59.999Z", null);
    const before = start("2026-10-19T10:29:59.999Z", "outside");
    const after = start("2026-10-19T14:00:00Z", "outside");
    const orphan = start("2026-10-19T12:00:00Z", "orphan");
    const old = start("1969-12-31T23:30:00Z", "old");
    const boundary = start("1970-01-01T00:00:00Z", "boundary");
    const nextDay = start("2026-10-20T10:00:00Z", "returning-viewer");
    const rows = [
      // Receive a later, inconsistent start first. The earliest sequence remains authoritative.
      { ...a, event_id: randomUUID(), sequence: 2, occurred_at: "2026-10-19T12:00:00Z", video_id: "wrong-video", browser_context: { schema_version: 1, browser_name: "Safari" } },
      a, measurement(a, 3, "view_ended", 500, "2026-10-21T00:00:00Z"),
      measurement(a, 90, "view_snapshot", 99999),
      { ...measurement(a, 100, "view_ended", 99999), video_id: "wrong-video" },
      b, measurement(b, 3, "view_snapshot", 1000), measurement(b, 2, "view_snapshot", 200),
      anonymous, before, measurement(before, 2, "view_ended", 8000), after,
      measurement(orphan, 2, "view_ended", 99999), old, boundary,
      nextDay, measurement(nextDay, 2, "view_ended", 700),
    ];
    for (const event of rows) await server.post("/v1/events", JSON.stringify(event), 201);
    await server.post("/v1/events", JSON.stringify(a), 200);
    const params = { from: "2026-10-19T10:30:00Z", to: "2026-10-19T14:00:00Z", interval: "hour" };
    const series = (overrides = {}) => server.get(`/v1/timeseries?${new URLSearchParams({ ...params, ...overrides })}`);
    const bucket = (hour, views, viewers, playing) => ({ bucket_start: `2026-10-19T${hour}:00:00.000Z`,
      views, unique_viewers: viewers, total_playing_time_ms: playing });
    const result = await series();
    assert.deepEqual(result, { from: "2026-10-19T10:30:00.000Z", to: "2026-10-19T14:00:00.000Z",
      interval: "hour", timezone: "UTC", buckets: [bucket("10", 1, 1, 500), bucket("11", 2, 1, 1000), bucket("12", 0, 0, 0), bucket("13", 0, 0, 0)] });
    const summary = await server.get(`/v1/summary?${new URLSearchParams({ from: params.from, to: params.to })}`);
    assert.equal(result.buckets.reduce((sum, row) => sum + row.views, 0), summary.views);
    assert.equal(result.buckets.reduce((sum, row) => sum + row.total_playing_time_ms, 0), summary.total_playing_time_ms);
    assert.equal(summary.unique_viewers, 1);
    assert.equal(result.buckets.reduce((sum, row) => sum + row.unique_viewers, 0), 2, "unique viewers are not additive");
    assert.deepEqual(await series({ from: "2026-10-19T12:30:00+02:00", to: "2026-10-19T16:00:00+02:00" }), result);
    assert.deepEqual((await series({ browser_name: "Chrome", os_name: "Linux", device_type: "desktop", viewer_id: "returning-viewer", video_id: "video-a" })).buckets,
      [bucket("10", 1, 1, 500), bucket("11", 0, 0, 0), bucket("12", 0, 0, 0), bucket("13", 0, 0, 0)]);
    for (const filter of [{ browser_name: "Safari" }, { video_id: "wrong-video" }, { device_type: "mobile" }, { viewer_id: "missing" }]) {
      assert.ok((await series(filter)).buckets.every(row => row.views === 0 && row.unique_viewers === 0 && row.total_playing_time_ms === 0));
    }
    assert.deepEqual((await series({ from: "2026-10-19T10:30:00Z", to: "2026-10-19T11:00:00Z" })).buckets, [bucket("10", 1, 1, 500)]);
    assert.deepEqual((await series({ interval: "day", from: "2026-10-19T10:30:00Z", to: "2026-10-21T00:00:00Z" })).buckets, [
      { bucket_start: "2026-10-19T00:00:00.000Z", views: 4, unique_viewers: 2, total_playing_time_ms: 1500 },
      { bucket_start: "2026-10-20T00:00:00.000Z", views: 1, unique_viewers: 1, total_playing_time_ms: 700 },
    ]);
    assert.deepEqual((await series({ from: "1969-12-31T23:30:00Z", to: "1970-01-01T00:00:00Z" })).buckets,
      [{ bucket_start: "1969-12-31T23:00:00.000Z", views: 1, unique_viewers: 1, total_playing_time_ms: 0 }]);
    const maxStart = Date.parse("2020-01-01T00:00:00Z");
    for (const [interval, width] of [["hour", 3600000], ["day", 86400000]]) {
      assert.equal((await series({ interval, from: new Date(maxStart).toISOString(), to: new Date(maxStart + 1000 * width).toISOString() })).buckets.length, 1000);
      await server.get(`/v1/timeseries?${new URLSearchParams({ interval, from: new Date(maxStart).toISOString(), to: new Date(maxStart + 1000 * width + 1).toISOString() })}`, 400);
    }
    for (const invalid of [{}, { from: params.from }, { ...params, interval: "week" }, { ...params, from: params.to },
      { ...params, to: params.from }, { ...params, from: "invalid" }, { ...params, from: "2026-10-19" },
      { ...params, limit: "1" }, { ...params, cursor: "x" }, { ...params, project_id: "other" },
      { ...params, event_name: "chunkify.video.view_started" }, { ...params, timezone: "Europe/Paris" },
      { ...params, browser_name: "" }, { ...params, os_name: "x".repeat(65) }, { ...params, device_type: "TV" },
      { ...params, from: "2026-10-19T10:00:00.0001Z", to: "2026-10-19T10:00:00.0009Z" }]) {
      await server.get(`/v1/timeseries?${new URLSearchParams(invalid)}`, 400);
    }
    await server.get(`/v1/timeseries?${new URLSearchParams(params)}&interval=day`, 400);
    for (const token of [null, "invalid", server.keys.public_key]) {
      await server.get(`/v1/timeseries?${new URLSearchParams(params)}`, 401, token ? { Authorization: `Bearer ${token}` } : {});
    }
    for (const path of ["/v1/summary", "/v1/videos", "/v1/events"]) await server.get(`${path}?interval=day`, 400);
  });
}


for (const separateReads of [false, true]) {
  test(`quality measures each view once with explicit coverage (separate reads: ${separateReads})`, async t => {
    const settings = { "experimental.read_worker": String(separateReads), "duckdb.threads": "1" };
    const server = await collector(t, settings);
    const sample = contract.paths["/v1/events"].post.requestBody.content["application/json"].example;
    const start = (video = "a", browser = "Chrome", at = "2026-10-19T12:00:00Z", viewer = "viewer") => ({
      ...sample, event_id: randomUUID(), view_id: randomUUID(), sequence: 1,
      occurred_at: at, video_id: video, viewer_id: viewer,
      browser_context: { schema_version: 1, browser_name: browser, os_name: "Linux", device_type: "desktop" },
    });
    const event = (view, sequence, kind, startup = null, playing = 0, buffering = 0, extra = {}) => ({
      ...view, event_id: randomUUID(), sequence, event_name: `chunkify.video.${kind}`,
      startup_time_ms: startup, playing_time_ms: playing, rebuffer_duration_ms: buffering,
      rebuffer_ratio: 0.99, // Deliberately not the ratio used by the aggregate.
      reason: kind === "view_ended" ? "destroyed" : null, ...extra,
    });
    const a = start();
    const b = start("b", "Firefox");
    const c = start("c", "Chrome", "2026-10-19T12:00:00Z", null);
    const d = start("no-snapshot");
    const e = start("zero-startup");
    const f = start("failed");
    const before = start("outside", "Chrome", "2026-10-18T23:59:59.999Z");
    const after = start("outside", "Chrome", "2026-10-20T00:00:00Z");
    const fatal = (view, sequence, extra = {}) => event(view, sequence, "playback_error", null, 0, 0, { error_fatal: true, ...extra });
    const aEnd = event(a, 5, "view_ended", 100, 900, 100, { occurred_at: "2026-10-21T12:00:00Z", browser_context: { schema_version: 1, browser_name: "Safari" } });
    const rows = [
      { ...a, event_id: randomUUID(), sequence: 2, video_id: "wrong-video", browser_context: { schema_version: 1, browser_name: "Safari" } },
      a, event(a, 99, "view_snapshot", 9999, 99999, 99999), aEnd,
      fatal(a, 3), fatal(a, 4),
      event(a, 100, "view_ended", 9999, 99999, 99999, { viewer_id: "wrong-viewer" }),
      b, event(b, 3, "view_snapshot", 300, 100, 100), event(b, 2, "view_snapshot", 50, 50, 50),
      fatal(b, 4, { viewer_id: "wrong-viewer" }), fatal(b, 5, { video_id: "wrong-video" }),
      event(b, 6, "playback_state_changed", null, 99999, 99999, { from_state: "playing", to_state: "paused", previous_state_duration_ms: 10, playhead_seconds: 10, error_fatal: true }),
      c, event(c, 2, "view_snapshot", 9999, 9000, 9000), event(c, 3, "view_ended", null, 1000, 0),
      fatal(c, 4, { error_fatal: false }),
      d, event(d, 2, "playback_error", 9999, 99999, 99999, { error_fatal: null }),
      e, event(e, 2, "view_ended", 0),
      f, fatal(f, 2), event(f, 3, "view_ended", null, 0, 0, { startup_outcome: "failed_before_start" }),
      before, fatal(before, 2), event(before, 3, "view_ended", 9999, 99999, 99999),
      after, fatal(after, 2), event(after, 3, "view_ended", 9999, 99999, 99999),
      fatal(start("orphan"), 2), event(start("orphan"), 3, "view_ended", 9999, 99999, 99999),
    ];
    for (const row of rows) {
      assertRequest("/v1/events", "post", row);
      // Exercise integral JSON floats, which lossless storage retains in JSON.
      const body = JSON.stringify(row).replace('"startup_time_ms":100,', '"startup_time_ms":100.0,')
        .replace('"rebuffer_duration_ms":100,', '"rebuffer_duration_ms":100.0,');
      await server.post("/v1/events", body, 201);
    }
    await server.post("/v1/events", JSON.stringify(aEnd), 200);
    const filters = { from: "2026-10-19T00:00:00Z", to: "2026-10-20T00:00:00Z" };
    const quality = (extra = {}) => server.get(`/v1/quality?${new URLSearchParams({ ...filters, ...extra })}`);
    const expected = {
      views: 6, startup_time_ms: { sample_views: 3, median: 100, p95: 280 },
      buffering: { sample_views: 3, total_playing_time_ms: 2000, total_rebuffer_duration_ms: 200, ratio: 200 / 2200 },
      fatal_errors: { affected_views: 2, ratio: 2 / 6 },
    };
    assert.deepEqual(await quality(), expected);
    assert.equal((await server.get(`/v1/summary?${new URLSearchParams(filters)}`)).views, expected.views);
    const empty = { views: 0, startup_time_ms: { sample_views: 0, median: null, p95: null },
      buffering: { sample_views: 0, total_playing_time_ms: 0, total_rebuffer_duration_ms: 0, ratio: null },
      fatal_errors: { affected_views: 0, ratio: null } };
    for (const extra of [{ video_id: "wrong-video" }, { viewer_id: "wrong-viewer" }, { browser_name: "Safari" },
      { os_name: "macOS" }, { device_type: "mobile" }, { from: "2026-10-22T00:00:00Z", to: "2026-10-23T00:00:00Z" }]) {
      assert.deepEqual(await quality(extra), empty);
    }
    assert.deepEqual(await quality({ video_id: "b", viewer_id: "viewer", browser_name: "Firefox", os_name: "Linux", device_type: "desktop" }), {
      views: 1, startup_time_ms: { sample_views: 1, median: 300, p95: 300 },
      buffering: { sample_views: 1, total_playing_time_ms: 100, total_rebuffer_duration_ms: 100, ratio: 0.5 },
      fatal_errors: { affected_views: 0, ratio: 0 },
    });
    const even = await quality({ viewer_id: "viewer", browser_name: "Chrome" });
    assert.deepEqual(even.startup_time_ms, { sample_views: 2, median: 50, p95: 95 });
    assert.deepEqual((await quality({ video_id: "no-snapshot" })).startup_time_ms, empty.startup_time_ms);
    assert.deepEqual((await quality({ video_id: "zero-startup" })).startup_time_ms, { sample_views: 1, median: 0, p95: 0 });
    assert.deepEqual((await quality({ video_id: "failed" })).fatal_errors, { affected_views: 1, ratio: 1 });
    assert.deepEqual(await quality({ from: "2026-10-19T02:00:00+02:00", to: "2026-10-20T02:00:00+02:00" }), expected);
    for (const invalid of [{ limit: "1" }, { cursor: "x" }, { event_name: "chunkify.video.view_started" },
      { project_id: "other" }, { interval: "day" }, { from: "invalid" }, { from: filters.to },
      { to: filters.from }, { browser_name: "" }, { os_name: "x".repeat(65) }, { device_type: "TV" }]) {
      await server.get(`/v1/quality?${new URLSearchParams({ ...filters, ...invalid })}`, 400);
    }
    await server.get("/v1/quality?video_id=a&video_id=b", 400);
    for (const token of [null, "invalid", server.keys.public_key]) {
      await server.get("/v1/quality", 401, token ? { Authorization: `Bearer ${token}` } : {});
    }
    // Large JSON integers also live in the storage remainder. This independent
    // cohort verifies extraction without affecting the realistic ratios above.
    const large = start("large-number");
    await server.post("/v1/events", JSON.stringify(large), 201);
    await server.post("/v1/events", JSON.stringify(event(large, 2, "view_ended", 9007199254740992, 1, 9007199254740992)), 201);
    const result = await quality({ video_id: "large-number" });
    assert.equal(result.startup_time_ms.median, 9007199254740992);
    assert.equal(result.buffering.total_rebuffer_duration_ms, 9007199254740992);
    const extreme = start("large-durations");
    await server.post("/v1/events", JSON.stringify(extreme), 201);
    await server.post("/v1/events", JSON.stringify(event(extreme, 2, "view_ended", 1e308, 1e308, 1e308)), 201);
    assert.equal((await quality({ video_id: "large-durations" })).buffering.ratio, 0.5);
    const overflow = start("large-durations");
    await server.post("/v1/events", JSON.stringify(overflow), 201);
    await server.post("/v1/events", JSON.stringify(event(overflow, 2, "view_ended", null, 1e308, 1e308)), 201);
    await server.get("/v1/quality?video_id=large-durations", 500);


  });
}


for (const separateReads of [false, true]) {
  test(`engagement uses all attempts and the selected final event (separate reads: ${separateReads})`, async t => {
    const settings = { "experimental.read_worker": String(separateReads), "duckdb.threads": "1" };
    const server = await collector(t, settings);
    const sample = contract.paths["/v1/events"].post.requestBody.content["application/json"].example;
    const start = (video, browser = "Chrome", at = "2026-10-19T12:00:00Z", viewer = "viewer") => ({
      ...sample, event_id: randomUUID(), view_id: randomUUID(), sequence: 1,
      occurred_at: at, video_id: video, viewer_id: viewer,
      browser_context: { schema_version: 1, browser_name: browser, os_name: "Linux", device_type: "desktop" },
    });
    const event = (view, sequence, kind, playing, reason = null, extra = {}) => ({
      ...view, event_id: randomUUID(), sequence, event_name: `chunkify.video.${kind}`,
      playing_time_ms: playing, reason, ...extra,
    });
    const a = start("ended"), b = start("ongoing", "Firefox"), c = start("start-only");
    const d = start("page-exit"), e = start("failed"), f = start("conflicting-ends");
    const g = start("end-order", "Chrome", "2026-10-19T12:00:00Z", null), h = start("unknown-end");
    const before = start("outside", "Chrome", "2026-10-18T23:59:59.999Z");
    const after = start("outside", "Chrome", "2026-10-20T00:00:00Z");
    const orphan = start("orphan");
    const aEnd = event(a, 3, "view_ended", 8000, "ended", {
      occurred_at: "2026-10-21T12:00:00Z", browser_context: { schema_version: 1, browser_name: "Safari" },
    });
    const rows = [
      { ...a, event_id: randomUUID(), sequence: 2, video_id: "wrong-video", browser_context: { schema_version: 1, browser_name: "Safari" } },
      a, aEnd, event(a, 90, "view_snapshot", 99999),
      event(a, 100, "view_ended", 99999, "destroyed", { viewer_id: "wrong-viewer" }),
      event(a, 100, "view_ended", 99999, "destroyed", { video_id: "wrong-video" }),
      b, event(b, 3, "view_snapshot", 4000, "ended"), event(b, 2, "view_snapshot", 1000),
      c, event(c, 2, "playback_error", 99999, "ended", { error_fatal: true }),
      d, event(d, 2, "view_ended", 2000, "page_exit"),
      e, event(e, 2, "view_ended", 0, "destroyed", { startup_outcome: "failed_before_start" }),
      f, event(f, 3, "view_ended", 6000, "ended"), event(f, 3, "view_ended", 6000, "destroyed"),
      g, event(g, 4, "view_ended", 1000, "ended", { occurred_at: "2026-10-19T13:00:00Z" }),
      event(g, 3, "view_ended", 99999, "destroyed", { occurred_at: "2026-10-19T14:00:00Z" }),
      event(g, 4, "view_ended", 99999, "destroyed", { occurred_at: "2026-10-19T12:30:00Z" }),
      h, event(h, 2, "view_ended", 0),
      before, event(before, 2, "view_ended", 99999, "ended"),
      after, event(after, 2, "view_ended", 99999, "ended"),
      event(orphan, 2, "view_ended", 99999, "ended"),
    ];
    for (const row of rows) {
      assertRequest("/v1/events", "post", row);
      await server.post("/v1/events", JSON.stringify(row), 201);
    }
    await server.post("/v1/events", JSON.stringify(aEnd), 200);
    const filters = { from: "2026-10-19T00:00:00Z", to: "2026-10-20T00:00:00Z" };
    const engagement = (extra = {}) => server.get(`/v1/engagement?${new URLSearchParams({ ...filters, ...extra })}`);
    const metrics = (views, playing, reached) => ({ views, total_playing_time_ms: playing,
      average_playing_time_ms: views ? playing / views : null, reached_end_views: reached,
      reached_end_ratio: views ? reached / views : null });
    const expected = metrics(8, 21000, 2);
    assert.deepEqual(await engagement(), expected);
    const summary = await server.get(`/v1/summary?${new URLSearchParams(filters)}`);
    assert.equal(expected.views, summary.views);
    assert.equal(expected.total_playing_time_ms, summary.total_playing_time_ms);
    assert.deepEqual(await engagement({ video_id: "ended", viewer_id: "viewer", browser_name: "Chrome", os_name: "Linux", device_type: "desktop" }), metrics(1, 8000, 1));
    assert.deepEqual(await engagement({ browser_name: "Firefox" }), metrics(1, 4000, 0));
    assert.deepEqual(await engagement({ viewer_id: "viewer" }), metrics(7, 20000, 1));
    for (const video_id of ["start-only", "failed", "unknown-end"]) {
      assert.deepEqual(await engagement({ video_id }), metrics(1, 0, 0));
    }
    assert.deepEqual(await engagement({ video_id: "conflicting-ends" }), metrics(1, 6000, 0));
    assert.deepEqual(await engagement({ video_id: "end-order" }), metrics(1, 1000, 1));
    for (const extra of [{ video_id: "wrong-video" }, { viewer_id: "wrong-viewer" }, { browser_name: "Safari" },
      { os_name: "macOS" }, { device_type: "mobile" }, { video_id: "orphan" },
      { from: "2026-10-22T00:00:00Z", to: "2026-10-23T00:00:00Z" }]) {
      assert.deepEqual(await engagement(extra), metrics(0, 0, 0));
    }
    assert.deepEqual(await engagement({ from: "2026-10-19T02:00:00+02:00", to: "2026-10-20T02:00:00+02:00" }), expected);
    assert.deepEqual(await engagement({ from: "2026-10-19T12:00:00Z", to: "2026-10-19T12:00:00.001Z" }), expected);
    assert.deepEqual(await engagement({ to: "2026-10-19T12:00:00Z" }), metrics(0, 0, 0));
    for (const invalid of [{ limit: "1" }, { cursor: "x" }, { event_name: "chunkify.video.view_started" },
      { project_id: "other" }, { interval: "day" }, { from: "invalid" }, { from: filters.to },
      { to: filters.from }, { browser_name: "" }, { os_name: "x".repeat(65) }, { device_type: "TV" }]) {
      await server.get(`/v1/engagement?${new URLSearchParams({ ...filters, ...invalid })}`, 400);
    }
    await server.get("/v1/engagement?video_id=a&video_id=b", 400);
    for (const token of [null, "invalid", server.keys.public_key]) {
      await server.get("/v1/engagement", 401, token ? { Authorization: `Bearer ${token}` } : {});
    }
    for (let i = 0; i < 2; i++) {
      const huge = start("overflow");
      await server.post("/v1/events", JSON.stringify(huge), 201);
      await server.post("/v1/events", JSON.stringify(event(huge, 2, "view_ended", 1e308, "ended")), 201);
    }
    await server.get("/v1/engagement?video_id=overflow", 500);
  });
}

for (const separateReads of [false, true]) test(`video sorting ranks the full cohort and paginates ties (separate reads: ${separateReads})`, async t => {
  const server = await collector(t, { "experimental.read_worker": String(separateReads) });
  const example = contract.paths["/v1/events"].post.requestBody.content["application/json"].example;
  function attempt(videoId, playingTime, browser = "Chrome") {
    const start = { ...structuredClone(example), event_id: randomUUID(), view_id: randomUUID(),
      video_id: videoId, sequence: 1, occurred_at: "2026-10-19T12:00:00Z",
      browser_context: { schema_version: 1, browser_name: browser, os_name: "Linux", device_type: "desktop" } };
    const end = { ...start, event_id: randomUUID(), event_name: "chunkify.video.view_ended", sequence: 2,
      occurred_at: "2026-10-19T12:00:01Z", playing_time_ms: playingTime, reason: "destroyed" };
    return [start, end];
  }
  const fractional = attempt("g", 4000.5);
  const records = [...attempt("a", 2000), ...attempt("a", 2000),
    ...attempt("b", 300), ...attempt("b", 300), ...attempt("b", 400),
    ...attempt("c", 2000), ...attempt("c", 2000), attempt("d", 0)[0], attempt("e", 0)[0],
    ...attempt("f", 5000, "Firefox"), ...fractional];
  for (const event of records) await server.post("/v1/events", JSON.stringify(event), 201);
  const get = params => server.get(`/v1/videos?${new URLSearchParams(params)}`);
  const ids = page => page.videos.map(video => video.video_id);
  async function remaining(first, params) {
    const result = [...first.videos];
    let page = first;
    while (page.next_cursor) {
      page = await get({ ...params, cursor: page.next_cursor, limit: "1" });
      result.push(...page.videos);
    }
    return result;
  }
  for (const [sort_by, order, expected] of [
    ["video_id", "asc", ["a", "b", "c", "d", "e", "f", "g"]],
    ["video_id", "desc", ["g", "f", "e", "d", "c", "b", "a"]],
    ["views", "asc", ["d", "e", "f", "g", "a", "c", "b"]],
    ["views", "desc", ["b", "a", "c", "d", "e", "f", "g"]],
    ["total_playing_time_ms", "asc", ["d", "e", "b", "a", "c", "g", "f"]],
    ["total_playing_time_ms", "desc", ["f", "g", "a", "c", "b", "d", "e"]],
  ]) {
    const params = { sort_by, order, limit: "2" };
    const all = await get({ sort_by, order });
    assert.deepEqual(ids(all), expected);
    assert.deepEqual(await remaining(await get(params), params), all.videos, `${sort_by} ${order}`);
  }
  const filtered = { sort_by: "total_playing_time_ms", order: "desc", browser_name: "Chrome", limit: "1" };
  assert.deepEqual(ids(await get(filtered)), ["g"], "filter before ranking the full cohort");
  assert.deepEqual(ids(await get({ sort_by: "views", order: "desc", video_id: "missing" })), []);
  assert.deepEqual(ids(await get({})), ["a", "b", "c", "d", "e", "f", "g"]);
  const defaults = await get({ limit: "1" });
  assert.deepEqual(ids(await get({ sort_by: "video_id", order: "asc", limit: "1", cursor: defaults.next_cursor })), ["b"]);
  const views = { sort_by: "views", order: "desc", limit: "1" };
  const watch = { sort_by: "total_playing_time_ms", order: "desc", limit: "1" };
  const firstViews = await get(views), firstWatch = await get(watch);
  for (const changed of [{ ...views, order: "asc" }, watch, {}, { ...views, browser_name: "Chrome" }]) {
    await server.get(`/v1/videos?${new URLSearchParams({ ...changed, cursor: firstViews.next_cursor })}`, 400);
  }
  await server.get(`/v1/events?cursor=${encodeURIComponent(firstViews.next_cursor)}`, 400);
  const invalidPosition = JSON.parse(Buffer.from(firstWatch.next_cursor, "base64url"));
  invalidPosition.after.total_playing_time_ms = -1;
  await server.get(`/v1/videos?${new URLSearchParams({ ...watch, cursor: Buffer.from(JSON.stringify(invalidPosition)).toString("base64url") })}`, 400);
  for (const query of ["sort_by=watch_time", "sort_by=", "sort_by=views%3BDROP%20TABLE%20events", "order=sideways",
    "sort_by=views&sort_by=video_id", "order=asc&order=desc"]) await server.get(`/v1/videos?${query}`, 400);
  for (const route of ["events", "summary", "quality", "engagement", "timeseries"]) {
    await server.get(`/v1/${route}?sort_by=views`, 400);
  }
  for (const event of [...attempt("d", 10), ...attempt("d", 10),
    { ...fractional[1], event_id: randomUUID(), sequence: 3, playing_time_ms: 100000 }]) {
    await server.post("/v1/events", JSON.stringify(event), 201);
  }
  assert.deepEqual((await remaining(firstViews, views)).map(video => video.video_id), ["b", "a", "c", "d", "e", "f", "g"]);
  const frozenWatch = await remaining(firstWatch, watch);
  assert.deepEqual(frozenWatch.map(video => video.video_id), ["f", "g", "a", "c", "b", "d", "e"]);
  assert.equal(frozenWatch.find(video => video.video_id === "g").total_playing_time_ms, 4000.5);
  assert.equal((await get(watch)).videos[0].video_id, "g", "a fresh sequence sees the later watch time");
});
