import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { promisify } from "node:util";
import test from "node:test";
import { attachAnalytics } from "@chunkify/analytics";
import { collectorEnvironment, writeConfig } from "./configuration.mjs";

const binary = process.env.CHUNKIFY_BINARY ?? resolve("target/debug/chunkify-analytics");
const command = promisify(execFile);
const chrome = { schema_version: 1, browser_name: "Chrome", os_name: "Linux", device_type: "desktop" };
const firefox = { schema_version: 1, browser_name: "Firefox", os_name: "Windows", device_type: "desktop" };
const safari = { schema_version: 1, browser_name: "Safari", os_name: "iOS", device_type: "mobile" };

async function collector(t, separateReads) {
  const directory = await mkdtemp(join(tmpdir(), "chunkify-context-filters-"));
  const env = { ...process.env, "data_dir": directory, "server.bind": "127.0.0.1:0",
    "retention.seconds": "0", "server.trusted_proxies": "",
    "experimental.read_worker": String(separateReads) };
  let child;
  let exited;
  let errors = "";
  const server = {
    async stop() {
      if (!child) return;
      if (child.exitCode !== null || child.signalCode !== null) { await exited; return; }
      child.kill("SIGTERM");
      const timeout = setTimeout(() => child.kill("SIGKILL"), 5000);
      try { assert.equal((await exited)[0], 0, errors); } finally { clearTimeout(timeout); }
    },
    async start() {
      child = spawn(binary, ["serve"], { env: collectorEnvironment(env), cwd: directory, stdio: ["ignore", "pipe", "pipe"] });
      exited = once(child, "close");
      let output = "";
      errors = "";
      child.stderr.on("data", (chunk) => { errors += chunk; });
      server.url = await new Promise((ready, reject) => {
        const timeout = setTimeout(() => reject(new Error(`Collector startup timed out: ${errors}`)), 10000);
        child.once("error", (error) => { clearTimeout(timeout); reject(error); });
        child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Collector exited: ${errors}`)); });
        child.stdout.on("data", (chunk) => {
          output += chunk;
          const match = output.match(/Listening on (http:\/\/\S+)/);
          if (match) { clearTimeout(timeout); ready(match[1]); }
        });
      });
    },
    request(path, params = {}) {
      return fetch(`${server.url}${path}?${new URLSearchParams(params)}`, {
        headers: { Authorization: `Bearer ${server.keys.secret_key}` },
      });
    },
    async get(path, params = {}) {
      const response = await server.request(path, params);
      assert.equal(response.status, 200, await response.clone().text());
      return response.json();
    },
    async send(event, status = 201) {
      const headers = { "X-Chunkify-Ingest-Key": server.keys.public_key };
      const response = await fetch(`${server.url}/v1/events`, { method: "POST",
        headers: { ...headers, "Content-Type": "application/json" }, body: JSON.stringify(event) });
      assert.equal(response.status, status, await response.text());
    },
  };
  t.after(async () => {
    try { await server.stop(); } finally { await rm(directory, { recursive: true, force: true }); }
  });
  await writeConfig(directory, env);
  const initialized = await command(binary, ["init"], { env: collectorEnvironment(env), cwd: directory, timeout: 10000 });
  server.keys = JSON.parse(initialized.stdout);
  await server.start();
  return server;
}

function view(videoId, context) {
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const events = [];
  const session = attachAnalytics(media, { videoId, viewerId: "viewer-a",
    includeEvents: ["view_started", "view_ended"], destinations: [{ send(event) { events.push(event); } }] });
  session.startAttempt();
  session.destroy();
  assert.equal(events.length, 2);
  return events.map((event) => ({ ...event, occurred_at: "2026-10-15T12:00:00Z",
    ...(context === undefined ? {} : { browser_context: context }) }));
}

const totals = (views, playingTime, uniqueViewers = views ? 1 : 0) => ({
  views, unique_viewers: uniqueViewers, total_playing_time_ms: playingTime,
});

for (const separateReads of [false, true]) {
  const reader = separateReads ? "separate reader" : "shared worker";

  test(`context filters select each event's strings without coercing missing or null values (${reader})`, async (t) => {
    const server = await collector(t, separateReads);
    const fixtures = [chrome, firefox, safari,
      { schema_version: 1, browser_name: null, os_name: null, device_type: "unknown" },
      undefined, { schema_version: 1, browser_name: 123, os_name: true, device_type: null },
      { schema_version: 1, browser_name: "null", os_name: "null", device_type: "tablet" },
    ].map((context, index) => view(`video-${index}`, context)[0]);
    for (const event of fixtures) await server.send(event);
    assert.equal((await server.get("/v1/events")).events.length, fixtures.length);
    for (const [params, expected] of [
      [{ browser_name: "Chrome" }, [fixtures[0]]],
      [{ os_name: "Windows" }, [fixtures[1]]],
      [{ device_type: "mobile" }, [fixtures[2]]],
      [{ device_type: "unknown" }, [fixtures[3]]],
      [{ device_type: "tablet" }, [fixtures[6]]],
      [{ browser_name: "null", os_name: "null" }, [fixtures[6]]],
      [{ browser_name: "123" }, []], [{ os_name: "true" }, []],
      [{ browser_name: "chrome" }, []], [{ os_name: "linux" }, []],
      [{ browser_name: "Chrome", os_name: "Windows" }, []],
      [{ browser_name: "Chrome", os_name: "Linux", device_type: "desktop", video_id: "video-0",
        viewer_id: "viewer-a", event_name: "chunkify.video.view_started",
        from: "2026-10-15T12:00:00Z", to: "2026-10-15T12:00:01Z" }, [fixtures[0]]],
    ]) {
      assert.deepEqual((await server.get("/v1/events", params)).events.map(({ event }) => event.event_id),
        expected.map((event) => event.event_id), JSON.stringify(params));
    }
    // First-delivery context stays authoritative even when a retry changes it.
    await server.send({ ...fixtures[0], browser_context: safari }, 200);
    assert.equal((await server.get("/v1/events", { browser_name: "Chrome" })).events.length, 1);
    const before = await server.get("/v1/events", { device_type: "desktop" });
    await server.stop();
    await server.start();
    assert.deepEqual(await server.get("/v1/events", { device_type: "desktop" }), before);
  });

  test(`context summary cohorts use the first retained start and final or latest playtime (${reader})`, async (t) => {
    const server = await collector(t, separateReads);
    const alpha = view("video-a", chrome);
    const beta = view("video-b", firefox);
    const gamma = view("video-c", chrome);
    const noContext = view("video-d");
    // The earliest start wins even when another start is received first.
    await server.send({ ...alpha[0], event_id: randomUUID(), sequence: 2, browser_context: firefox });
    await server.send(alpha[0]);
    await server.send({ ...alpha[1], sequence: 4, playing_time_ms: 1000, browser_context: safari,
      occurred_at: "2026-10-16T12:00:00Z" });
    await server.send({ ...alpha[0], event_id: randomUUID(), event_name: "chunkify.video.view_snapshot",
      sequence: 5, playing_time_ms: 9999 });
    await server.send(beta[0]);
    await server.send({ ...beta[1], playing_time_ms: 2000, browser_context: chrome });
    await server.send(gamma[0]);
    await server.send({ ...gamma[0], event_id: randomUUID(), event_name: "chunkify.video.view_snapshot",
      sequence: 3, playing_time_ms: 3000, browser_context: safari });
    await server.send({ ...gamma[0], event_id: randomUUID(), event_name: "chunkify.video.view_snapshot",
      sequence: 2, playing_time_ms: 500, browser_context: firefox });
    await server.send(noContext[0]);
    await server.send({ ...noContext[1], playing_time_ms: 500, browser_context: chrome });

    assert.deepEqual(await server.get("/v1/summary"), totals(4, 6500));
    const params = { browser_name: "Chrome", os_name: "Linux", device_type: "desktop" };
    assert.deepEqual(await server.get("/v1/summary", params), totals(2, 4000));
    assert.deepEqual(await server.get("/v1/summary", { browser_name: "Firefox" }), totals(1, 2000));
    assert.deepEqual(await server.get("/v1/summary", { browser_name: "Safari" }), totals(0, 0));
    assert.deepEqual(await server.get("/v1/summary", { device_type: "unknown" }), totals(0, 0));
    assert.deepEqual(await server.get("/v1/summary", { ...params, video_id: "video-a", viewer_id: "viewer-a",
      from: "2026-10-15T00:00:00Z", to: "2026-10-16T00:00:00Z" }), totals(1, 1000));
    const videos = await server.get("/v1/videos", params);
    assert.deepEqual(videos, { videos: [
      { video_id: "video-a", ...totals(1, 1000) }, { video_id: "video-c", ...totals(1, 3000) },
    ], next_cursor: null });
    await server.stop();
    await server.start();
    assert.deepEqual(await server.get("/v1/summary", params), totals(2, 4000));
    assert.deepEqual(await server.get("/v1/videos", params), videos);
  });

  test(`context filters stay fixed across event and video pages (${reader})`, async (t) => {
    const server = await collector(t, separateReads);
    const initial = [view("video-a", chrome)[0], view("video-b", chrome)[0], view("video-c", firefox)[0]];
    for (const event of initial) await server.send(event);
    const params = { browser_name: "Chrome", os_name: "Linux", device_type: "desktop", limit: "1" };
    const paths = ["/v1/events", "/v1/videos"];
    const first = await Promise.all(paths.map((path) => server.get(path, params)));
    await server.send(view("video-z", chrome)[0]);
    for (const [index, path] of paths.entries()) {
      const cursor = first[index].next_cursor;
      assert.ok(cursor);
      const page = await server.get(path, { ...params, cursor });
      const field = path.endsWith("events") ? "events" : "videos";
      assert.equal(page[field].length, 1);
      assert.equal(page.next_cursor, null, "newly ingested matching events must stay outside this page sequence");
      assert.equal(field === "events" ? page.events[0].event.video_id : page.videos[0].video_id, "video-b");
      for (const [name, value] of [["browser_name", "Firefox"], ["os_name", "Windows"], ["device_type", "mobile"]]) {
        const changed = await server.request(path, { ...params, [name]: value, cursor });
        assert.equal(changed.status, 400, `${path}: changed ${name}`);
        await changed.arrayBuffer();
        const omitted = { ...params, cursor };
        delete omitted[name];
        const response = await server.request(path, omitted);
        assert.equal(response.status, 400, `${path}: omitted ${name}`);
        await response.arrayBuffer();
      }
      const unfiltered = await server.get(path, { limit: "1" });
      const legacyCursor = JSON.parse(Buffer.from(unfiltered.next_cursor, "base64url").toString());
      for (const name of ["browser_name", "os_name", "device_type"]) delete legacyCursor.filters[name];
      const legacyPage = await server.get(path, { limit: "1",
        cursor: Buffer.from(JSON.stringify(legacyCursor)).toString("base64url") });
      assert.equal(field === "events" ? legacyPage.events[0].event.video_id : legacyPage.videos[0].video_id, "video-b");
    }
  });

  test(`context filter values are bounded and SQL values remain literal (${reader})`, async (t) => {
    const server = await collector(t, separateReads);
    const literal = "Chrome' OR TRUE --";
    const literalEvent = view("quoted", { ...chrome, browser_name: literal, os_name: "é".repeat(64) })[0];
    await server.send(literalEvent);
    await server.send(view("other", chrome)[0]);
    for (const path of ["/v1/events", "/v1/summary", "/v1/videos"]) {
      for (const params of [{ browser_name: "" }, { browser_name: "é".repeat(65) },
        { os_name: "" }, { os_name: "x".repeat(65) }, { device_type: "Desktop" },
        { device_type: "" }, { device_type: "phone" }]) {
        const response = await server.request(path, params);
        assert.equal(response.status, 400, `${path}: ${JSON.stringify(params)}`);
        await response.arrayBuffer();
      }
      const result = await server.get(path, { browser_name: literal, os_name: "é".repeat(64) });
      if (path === "/v1/events") assert.deepEqual(result.events.map(({ event }) => event.event_id), [literalEvent.event_id]);
      else if (path === "/v1/summary") assert.deepEqual(result, totals(1, 0));
      else assert.deepEqual(result.videos, [{ video_id: "quoted", ...totals(1, 0) }]);
      await server.get(path, { browser_name: "x".repeat(64) });
    }
  });
}
