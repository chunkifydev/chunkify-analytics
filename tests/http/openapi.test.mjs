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
  const configuration = { ...inherited, "rate_limits.read_per_minute": "1000000", "rate_limits.read_burst": "1000000",
    "rate_limits.read_concurrent_per_project": "32", "rate_limits.read_concurrent": "32",
    "rate_limits.export_downloads_per_minute": "1000", "data_dir": directory, "server.bind": "127.0.0.1:0",
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
    async admin(args) {
      const process = spawn(binary, args, { cwd: directory, env, stdio: "pipe" });
      let stdout = "", stderr = "";
      process.stdout.on("data", chunk => { stdout += chunk; });
      process.stderr.on("data", chunk => { stderr += chunk; });
      assert.equal((await once(process, "close"))[0], 0, stderr);
      return JSON.parse(stdout);
    },
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
    assert.deepEqual(result, { kind: "summary", from: "2026-10-19T10:30:00.000Z", to: "2026-10-19T14:00:00.000Z",
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

for (const separateReads of [false, true]) test(`metric breakdowns preserve start context and page complete groups (separate reads: ${separateReads})`, async t => {
  const server = await collector(t, { "experimental.read_worker": String(separateReads) });
  const example = contract.paths["/v1/events"].post.requestBody.content["application/json"].example;
  const context = (browser_name, os_name, device_type) => ({ schema_version: 1, browser_name, os_name, device_type });
  function attempt(video, browser, viewer, playing, reason = "destroyed") {
    const start = { ...structuredClone(example), event_id: randomUUID(), view_id: randomUUID(), sequence: 1,
      video_id: video, viewer_id: viewer, occurred_at: "2026-10-19T12:00:00Z",
      ...(browser !== undefined && { browser_context: browser }) };
    const end = { ...start, event_id: randomUUID(), event_name: "chunkify.video.view_ended", sequence: 4,
      occurred_at: "2026-10-19T12:00:01Z", playing_time_ms: playing, reason,
      startup_time_ms: video === "a" ? 200 : null, rebuffer_duration_ms: video === "a" ? 100 : 0 };
    return [start, end];
  }
  const a = attempt("a", context("Chrome", "Linux", "desktop"), "shared", 1000, "ended");
  const b = attempt("b", context("Chrome", "Windows", "mobile"), "shared", 3000);
  const c = attempt("c", context("Firefox", "Windows", "desktop"), "other", 2000, "ended");
  const d = attempt("d", undefined, null, 0);
  const e = attempt("e", context(123, true, null), "shared", 500);
  const f = attempt("f", context("unknown", "unknown", "unknown"), "shared", 500);
  const error = { ...a[1], event_id: randomUUID(), sequence: 3, event_name: "chunkify.video.playback_error",
    reason: null, error_fatal: true, error_category: "network", error_code: 2 };
  const records = [{ ...a[0], event_id: randomUUID(), sequence: 2, browser_context: context("Firefox", "Changed", "tablet") },
    a[0], error, { ...a[1], browser_context: context("Safari", "Changed", "tablet") },
    ...b, ...c, d[0], ...e, ...f,
    { ...a[1], event_id: randomUUID(), view_id: randomUUID(), playing_time_ms: 999999 }];
  for (const event of records) await server.post("/v1/events", JSON.stringify(event), 201);
  const grouped = (route, params) => server.get(`/v1/${route}?${new URLSearchParams(params)}`);
  for (const route of ["summary", "quality", "engagement"]) {
    for (const group_by of ["browser_name", "os_name", "device_type"]) {
      const all = await grouped(route, { group_by });
      assert.equal(all.group_by, group_by);
      assert.equal(all.next_cursor, null);
      assert.equal(all.groups[0].value, null);
      assert.equal(all.groups.reduce((sum, row) => sum + row.data.views, 0), 6);
      for (const group of all.groups.filter(group => group.value !== null)) {
        assert.deepEqual(group.data, await grouped(route, { [group_by]: group.value }), `${route} ${group_by} ${group.value}`);
      }
      let page = await grouped(route, { group_by, limit: "1" });
      const groups = [...page.groups];
      while (page.next_cursor) {
        page = await grouped(route, { group_by, cursor: page.next_cursor, limit: "2" });
        groups.push(...page.groups);
      }
      assert.deepEqual(groups, all.groups);
      assert.deepEqual((await grouped(route, { group_by, video_id: "missing" })).groups, []);
      const filtered = await grouped(route, { group_by, browser_name: "Chrome", viewer_id: "shared",
        from: "2026-10-19T12:00:00Z", to: "2026-10-20T00:00:00Z" });
      assert.equal(filtered.groups.reduce((sum, row) => sum + row.data.views, 0), 2);
    }
  }
  const byBrowser = await grouped("summary", { group_by: "browser_name" });
  assert.deepEqual(byBrowser.groups[0].data, { views: 2, unique_viewers: 1, total_playing_time_ms: 500 });
  assert.deepEqual(byBrowser.groups.map(row => row.value), [null, "Chrome", "Firefox", "unknown"]);
  assert.equal(byBrowser.groups.find(row => row.value === "Chrome").data.unique_viewers, 1);
  const quality = await grouped("quality", { group_by: "browser_name" });
  assert.equal(quality.groups.find(row => row.value === "Chrome").data.fatal_errors.ratio, 0.5);
  assert.equal(quality.groups[0].data.startup_time_ms.median, null);
  const devices = await grouped("summary", { group_by: "device_type" });
  assert.deepEqual(devices.groups.map(row => row.value), [null, "desktop", "mobile", "unknown"]);
  const first = await grouped("summary", { group_by: "browser_name", limit: "1" });
  for (const [route, params] of [["summary", { group_by: "os_name" }], ["summary", {}], ["quality", { group_by: "browser_name" }],
    ["summary", { group_by: "browser_name", viewer_id: "shared" }]]) {
    await server.get(`/v1/${route}?${new URLSearchParams({ ...params, cursor: first.next_cursor })}`, 400);
  }
  for (const route of ["summary", "quality", "engagement"]) {
    for (const query of ["group_by=country", "group_by=", "group_by=browser_name&group_by=os_name", "group_by=browser_name&limit=0",
      "group_by=browser_name&limit=1001", "group_by=browser_name&sort_by=views", "group_by=browser_name&order=desc",
      "group_by=browser_name&event_name=chunkify.video.view_started", "limit=1", "cursor=bad"]) {
      await server.get(`/v1/${route}?${query}`, 400);
    }
    for (const headers of [{}, { Authorization: `Bearer ${server.keys.public_key}` }]) {
      await server.get(`/v1/${route}?group_by=browser_name`, 401, headers);
    }
  }
  for (const route of ["events", "videos", "timeseries"]) await server.get(`/v1/${route}?group_by=browser_name`, 400);
  for (const event of attempt("late", context("Zulu", "Other", "tablet"), "late", 9000)) {
    await server.post("/v1/events", JSON.stringify(event), 201);
  }
  let page = first;
  const frozen = [...page.groups];
  while (page.next_cursor) {
    page = await grouped("summary", { group_by: "browser_name", limit: "1", cursor: page.next_cursor });
    frozen.push(...page.groups);
  }
  assert.deepEqual(frozen, byBrowser.groups);
  assert.ok((await grouped("summary", { group_by: "browser_name" })).groups.some(group => group.value === "Zulu"));
});

function csvRecords(text) {
  const rows = [];
  let row = [],
    cell = "",
    quoted = false;
  for (let i = 0; i < text.length; i++) {
    const c = text[i];
    if (c === '"') {
      if (quoted && text[i + 1] === '"') {
        cell += '"';
        i++;
      } else quoted = !quoted;
    } else if (c === "," && !quoted) {
      row.push(cell);
      cell = "";
    } else if (c === "\n" && !quoted) {
      row.push(cell);
      rows.push(row);
      row = [];
      cell = "";
    } else if (c !== "\r" || quoted) cell += c;
  }
  assert.equal(quoted, false, "complete CSV");
  assert.equal(cell, "");
  assert.deepEqual(row, []);
  return rows;
}

for (const separateReads of [false, true])
  test(`analytics exploration and scoped CSV downloads (separate reads: ${separateReads})`, async (t) => {
    const server = await collector(t, {
      "experimental.read_worker": String(separateReads),
      "server.ingest_per_second": "100000",
    });
    const example =
      contract.paths["/v1/events"].post.requestBody.content["application/json"]
        .example;
    const starts = [];
    for (const [index, browser] of ["Chrome", "Firefox", null].entries()) {
      const start = {
        ...structuredClone(example),
        event_id: randomUUID(),
        view_id: randomUUID(),
        video_id: index === 0 ? '=formula,"line\n雪' : `video-${index}`,
        viewer_id: index === 2 ? null : "same-viewer",
        occurred_at: `2026-10-19T${10 + index}:00:00Z`,
        custom_payload: { marker: "nested-custom-data" },
        custom_note: "custom-scalar-data",
        browser_context: {
          schema_version: 1,
          browser_name: browser,
          os_name: "Linux",
          device_type: index === 2 ? "unknown" : "desktop",
        },
      };
      starts.push(start);
      const end = {
        ...start,
        event_id: randomUUID(),
        event_name: "chunkify.video.view_ended",
        sequence: 3,
        reason: "ended",
        occurred_at: "2026-10-21T00:00:00Z",
        playing_time_ms: 1000 * (index + 1),
        startup_time_ms: 100 * (index + 1),
        rebuffer_duration_ms: index * 100,
      };
      for (const event of [start, end])
        await server.post("/v1/events", JSON.stringify(event), 201);
      if (index === 0)
        for (const code of [2, "2", null])
          await server.post(
            "/v1/events",
            JSON.stringify({
              ...start,
              event_id: randomUUID(),
              event_name: "chunkify.video.playback_error",
              sequence: 2,
              error_code: code,
              error_fatal: code === 2,
              error_category: "network",
            }),
            201,
          );
    }
    const headers = { Authorization: `Bearer ${server.keys.secret_key}` };
    const detail = async (id, query = "", status = 200) =>
      assertResponse(
        "/v1/views/{view_id}",
        "get",
        await fetch(`${server.url}/v1/views/${id}${query}`, { headers }),
        status,
      );
    const dimensions = async (name, query = "", status = 200) =>
      assertResponse(
        "/v1/dimensions/{dimension}",
        "get",
        await fetch(`${server.url}/v1/dimensions/${name}${query}`, { headers }),
        status,
      );
    for (const kind of ["quality", "engagement"]) {
      const series = await server.get(
        `/v1/timeseries?kind=${kind}&from=2026-10-19T10:00:00Z&to=2026-10-19T14:00:00Z&interval=hour`,
      );
      assert.equal(series.kind, kind);
      assert.equal(series.buckets.length, 4);
      for (let i = 0; i < 4; i++) {
        const total = await server.get(
          `/v1/${kind}?from=2026-10-19T${10 + i}:00:00Z&to=2026-10-19T${11 + i}:00:00Z`,
        );
        const { bucket_start, ...metrics } = series.buckets[i];
        assert.deepEqual(metrics, total);
      }
    }
    const first = await server.get("/v1/views?limit=1");
    assert.equal(first.views.length, 1);
    assert.ok(first.next_cursor);
    const fatal = await server.get("/v1/views?has_fatal_error=true");
    assert.equal(fatal.views.length, 1);
    assert.equal(fatal.views[0].view_id, starts[0].view_id);
    assert.equal(
      (await server.get("/v1/views?has_fatal_error=false")).views.length,
      2,
    );
    await server.get(
      `/v1/views?has_fatal_error=true&cursor=${encodeURIComponent(first.next_cursor)}`,
      400,
    );
    assert.equal(
      (await server.get(`/v1/events?view_id=${starts[0].view_id}`)).events
        .length,
      5,
    );
    let page = await detail(starts[0].view_id, "?limit=2");
    const info = page.view,
      history = [...page.events];
    assert.equal(info.playing_time_ms, 1000);
    assert.equal(info.has_fatal_error, true);
    assert.equal(info.reached_end, true);
    while (page.next_cursor) {
      page = await detail(
        starts[0].view_id,
        `?limit=2&cursor=${encodeURIComponent(page.next_cursor)}`,
      );
      assert.deepEqual(page.view, info);
      history.push(...page.events);
    }
    assert.equal(history.length, 5);
    assert.ok(history.every((e) => e.event.view_id === starts[0].view_id));
    await detail(randomUUID(), "", 404);
    const errors = await server.get("/v1/errors");
    assert.equal(errors.errors.length, 3);
    assert.deepEqual(
      new Set(errors.errors.map((e) => JSON.stringify(e.error_code))),
      new Set(["2", '"2"', "null"]),
    );
    assert.ok(
      errors.errors.every(
        (e) => e.error_events === 1 && e.affected_views === 1,
      ),
    );
    let d = await dimensions("browser_name", "?limit=1");
    assert.equal(d.values[0].value, null);
    assert.ok(d.next_cursor);
    const next = await dimensions(
      "browser_name",
      `?cursor=${encodeURIComponent(d.next_cursor)}`,
    );
    assert.equal(next.values.length, 2);
    await dimensions(
      "os_name",
      `?cursor=${encodeURIComponent(d.next_cursor)}`,
      400,
    );
    for (const invalid of [
      "/v1/views?has_fatal_error=maybe",
      "/v1/views?unknown=x",
      "/v1/errors?group_by=browser_name",
      "/v1/timeseries?kind=errors&from=2026-10-19T00:00:00Z&to=2026-10-20T00:00:00Z&interval=day",
      "/v1/events?export=true",
    ])
      await server.get(invalid, 400);
    const key = await server.admin([
      "key",
      "create",
      "default",
      "secret",
      "Exports",
      "--read-only",
    ]);
    const create = async (body, status = 201, token = key.key) =>
      assertResponse(
        "/v1/exports",
        "post",
        await fetch(`${server.url}/v1/exports`, {
          method: "POST",
          headers: {
            Authorization: `Bearer ${token}`,
            "content-type": "application/json",
          },
          body: JSON.stringify(body),
        }),
        status,
      );
    for (const [resource, query, expectedRows] of [
      ["summary", {}, 1],
      ["quality", { group_by: "browser_name" }, 3],
      ["engagement", {}, 1],
      [
        "timeseries",
        {
          kind: "quality",
          from: "2026-10-19T10:00:00Z",
          to: "2026-10-19T14:00:00Z",
          interval: "hour",
        },
        4,
      ],
      ["videos", { sort_by: "views", order: "desc" }, 3],
      ["events", { view_id: starts[0].view_id }, 5],
      ["views", { has_fatal_error: true }, 1],
      ["errors", {}, 3],
      ["dimensions", { dimension: "browser_name" }, 3],
    ]) {
      const link = await create({ resource, query });
      assert.ok(!JSON.stringify(link).includes(key.key));
      assert.equal(new URL(link.download_url).origin, server.url);
      const response = await fetch(link.download_url);
      const text = await assertResponse(
        "/v1/exports/{token}",
        "get",
        response,
        200,
      );
      assert.equal(
        response.headers.get("content-disposition"),
        `attachment; filename="chunkify-${resource}.csv"`,
      );
      assert.equal(response.headers.get("x-content-type-options"), "nosniff");
      assert.equal(csvRecords(text).length, expectedRows + 1);
      if (resource === "videos") assert.ok(text.includes("'=formula,"));
      if (resource === "events") {
        const [columns, ...records] = csvRecords(text);
        const event = Object.fromEntries(
          columns.map((name, i) => [name, records[0][i]]),
        );
        assert.equal(event.event_id, history[0].event.event_id);
        assert.equal(event.video_id, `'${history[0].event.video_id}`);
        assert.equal(
          event.playing_time_ms,
          String(history[0].event.playing_time_ms),
        );
        assert.equal(event["context.browser_name"], "Chrome");
        for (const name of [
          "rebuffer_count",
          "rebuffer_duration_ms",
          "startup_time_ms",
          "error_code",
          "error_fatal",
          "from_state",
          "seek_outcome",
        ])
          assert.ok(columns.includes(name), name);
        assert.ok(
          !columns.some((name) =>
            [
              "event",
              "event_context",
              "otlp",
              "custom_payload",
              "custom_note",
              "rendition_playing_time",
            ].includes(name),
          ),
        );
        assert.ok(!text.includes("nested-custom-data"));
        assert.ok(!text.includes("custom-scalar-data"));
        const fatalColumn = columns.indexOf("error_fatal");
        assert.equal(
          records.filter((record) => record[fatalColumn] === "true").length,
          1,
        );
      }
      assert.equal(
        (await fetch(link.download_url, { method: "HEAD" })).status,
        200,
      );
      assert.equal(
        (await fetch(`${link.download_url}?video_id=another`)).status,
        400,
      );
    }
    for (const body of [
      { resource: "projects" },
      { resource: "videos", query: { limit: 1 } },
      { resource: "events", query: { project_id: "other" } },
      { resource: "views", query: { has_fatal_error: "true" } },
      { resource: "timeseries", query: {} },
      { resource: "dimensions", query: { dimension: "country" } },
      { resource: "summary", query: { sort_by: "views" } },
    ])
      await create(body, 400);
    await create({ resource: "events" }, 401, server.keys.public_key);
    const link = await create({ resource: "events" });
    await server.admin(["key", "rotate", "default", key.key_id]);
    assert.equal((await fetch(link.download_url)).status, 404);
    assert.equal(
      (await fetch(`${server.url}/v1/exports/${"A".repeat(43)}`)).status,
      404,
    );
  });

test("CSV exports contain every matching row beyond normal query page limits", async (t) => {
  const server = await collector(t, { "server.ingest_per_second": "100000" });
  const example =
    contract.paths["/v1/events"].post.requestBody.content["application/json"]
      .example;
  for (let base = 0; base < 1105; base += 40)
    await Promise.all(
      Array.from({ length: Math.min(40, 1105 - base) }, (_, i) =>
        server.post(
          "/v1/events",
          JSON.stringify({
            ...example,
            event_id: randomUUID(),
            view_id: randomUUID(),
            video_id: `video-${base + i}`,
          }),
          201,
        ),
      ),
    );
  assert.equal((await server.get("/v1/events?limit=1000")).events.length, 1000);
  const response = await fetch(`${server.url}/v1/exports`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${server.keys.secret_key}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ resource: "events" }),
  });
  const link = await assertResponse("/v1/exports", "post", response, 201);
  const csv = await fetch(link.download_url);
  assert.equal(csv.status, 200);
  assert.equal(csvRecords(await csv.text()).length, 1106);
  const hourly = await fetch(`${server.url}/v1/exports`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${server.keys.secret_key}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({
      resource: "timeseries",
      query: {
        from: "2026-01-01T00:00:00Z",
        to: "2026-03-01T00:00:00Z",
        interval: "hour",
      },
    }),
  });
  const hours = await assertResponse("/v1/exports", "post", hourly, 201);
  assert.equal(
    csvRecords(await (await fetch(hours.download_url)).text()).length,
    24 * 59 + 1,
  );
});

test("export links use the configured public base including an ingress prefix", async (t) => {
  const server = await collector(t, {
    "server.public_url": "https://analytics.example.com/collector/",
  });
  const response = await fetch(`${server.url}/v1/exports`, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${server.keys.secret_key}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ resource: "videos" }),
  });
  const link = await assertResponse("/v1/exports", "post", response, 201);
  assert.ok(
    link.download_url.startsWith(
      "https://analytics.example.com/collector/v1/exports/",
    ),
  );
  const token = new URL(link.download_url).pathname.split("/").at(-1);
  assert.equal((await fetch(`${server.url}/v1/exports/${token}`)).status, 200);
});

for (const separateReads of [false, true])
  test(`read rate budgets span routes and keys without blocking ingestion (separate reads: ${separateReads})`, async (t) => {
    const server = await collector(t, {
      "experimental.read_worker": String(separateReads),
      "rate_limits.read_per_minute": "1",
      "rate_limits.read_burst": "1",
    });
    const key = await server.admin([
      "key",
      "create",
      "default",
      "secret",
      "Read budget",
      "--read-only",
    ]);
    await server.get("/v1/summary?from=invalid", 400);
    await server.get("/v1/summary");
    const response = await fetch(`${server.url}/v1/events`, {
      headers: { Authorization: `Bearer ${key.key}` },
    });
    await assertResponse("/v1/events", "get", response, 429);
    assert.ok(Number(response.headers.get("retry-after")) >= 1);
    assert.ok(Number(response.headers.get("retry-after")) <= 60);
    const rotated = await server.admin([
      "key",
      "rotate",
      "default",
      key.key_id,
    ]);
    await server.get("/v1/quality", 429, {
      Authorization: `Bearer ${rotated.key}`,
    });
    const other = await server.admin([
      "project",
      "create",
      "Independent budget",
    ]);
    await server.get("/v1/summary", 200, {
      Authorization: `Bearer ${other.secret_key}`,
    });
    await server.get("/v1/engagement", 429);
    const example =
      contract.paths["/v1/events"].post.requestBody.content["application/json"]
        .example;
    await server.post("/v1/events", JSON.stringify(example), 201);
    assert.equal((await fetch(`${server.url}/healthz`)).status, 200);
  });

test("export link creation has a separate shared project rate budget", async (t) => {
  const server = await collector(t, {
    "rate_limits.export_links_per_minute": "1",
  });
  const key = await server.admin([
    "key",
    "create",
    "default",
    "secret",
    "Export budget",
    "--read-only",
  ]);
  const create = async (token, status) => {
    const response = await fetch(`${server.url}/v1/exports`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ resource: "summary" }),
    });
    const body = await assertResponse("/v1/exports", "post", response, status);
    if (status === 429)
      assert.ok(Number(response.headers.get("retry-after")) >= 1);
    return body;
  };
  const link = await create(server.keys.secret_key, 201);
  await create(key.key, 429);
  await server.get("/v1/summary");
  assert.equal((await fetch(link.download_url)).status, 200);
  const other = await server.admin([
    "project",
    "create",
    "Other export budget",
  ]);
  await create(other.secret_key, 201);
});

test("CSV download budgets cannot be bypassed with another link or rotated key", async (t) => {
  const server = await collector(t, {
    "rate_limits.export_downloads_per_minute": "1",
  });
  const key = await server.admin([
    "key",
    "create",
    "default",
    "secret",
    "Download budget",
    "--read-only",
  ]);
  const create = async (token) => {
    const response = await fetch(`${server.url}/v1/exports`, {
      method: "POST",
      headers: {
        Authorization: `Bearer ${token}`,
        "content-type": "application/json",
      },
      body: JSON.stringify({ resource: "events" }),
    });
    return assertResponse("/v1/exports", "post", response, 201);
  };
  const a = await create(server.keys.secret_key),
    b = await create(key.key);
  for (let i = 0; i < 3; i++)
    assert.equal((await fetch(a.download_url, { method: "HEAD" })).status, 200);
  assert.equal(
    (await fetch(`${server.url}/v1/exports/${"A".repeat(43)}`)).status,
    404,
  );
  const first = await fetch(a.download_url);
  assert.equal(first.status, 200);
  await first.text();
  for (const link of [a, b]) {
    const response = await fetch(link.download_url);
    await assertResponse("/v1/exports/{token}", "get", response, 429);
    assert.ok(Number(response.headers.get("retry-after")) >= 1);
  }
  const rotated = await server.admin(["key", "rotate", "default", key.key_id]);
  const c = await create(rotated.key);
  assert.equal((await fetch(c.download_url)).status, 429);
  const other = await server.admin([
    "project",
    "create",
    "Other download budget",
  ]);
  const independent = await create(other.secret_key);
  const download = await fetch(independent.download_url);
  assert.equal(download.status, 200);
  await download.text();
  await server.get("/v1/summary");
});
