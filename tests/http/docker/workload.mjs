import { sdkDist } from "../sdk-path.mjs";
import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { attachAnalytics } from "@chunkify/player-analytics";
import { createHttpAdapter } from "@chunkify/player-analytics/http";

// Record a real SDK playback and the HTTP adapter's wire output before timing.
// Load generation replays these templates with new identities and timestamps.
export async function captureWorkload({ browserContext = false } = {}) {
  const events = [];
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const session = attachAnalytics(media, { videoId: "fixture", viewerId: "fixture", snapshotIntervalMs: 1000,
    destinations: [{ send(event) { events.push(event); } }] });
  let rendition;
  try {
    session.setConnector({ name: "benchmark-fixture", version: "1", playerName: null, playerVersion: null,
      snapshot: () => null, subscribeToErrors: () => () => {},
      subscribeToRenditions(listener) { rendition = listener; return () => {}; } });
    rendition({ width: 640, height: 360, codec: "avc1", bitrate_bps: 500000, frame_rate: 30 });
    session.startAttempt();
    media.dispatchEvent(new Event("playing"));
    await delay(1050);
    rendition({ width: 1280, height: 720, codec: "avc1", bitrate_bps: 1000000, frame_rate: 30 });
    media.dispatchEvent(new Event("seeking"));
    media.currentTime = 30;
    media.dispatchEvent(new Event("seeked"));
    media.dispatchEvent(new Event("playing"));
    await delay(50);
    media.error = { code: 2 };
    media.dispatchEvent(new Event("error"));
  } finally { session.destroy(); }
  assert.equal(new Set(events.map((event) => event.event_name)).size, 7);
  assert.ok(events.at(-1).playing_time_ms > 0);

  const captured = [];
  const receiver = createServer(async (request, response) => {
    if (request.method === "GET") {
      if (request.url === "/") response.writeHead(200, { "Content-Type": "text/html" }).end("<!doctype html><video></video>");
      else if (/^\/[a-z-]+\.js$/.test(request.url ?? "")) {
        try {
          const source = await readFile(resolve(sdkDist, request.url.slice(1)));
          response.writeHead(200, { "Content-Type": "text/javascript" }).end(source);
        } catch { response.writeHead(404).end(); }
      } else response.writeHead(404).end();
      return;
    }
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    captured.push(JSON.parse(Buffer.concat(chunks)));
    response.writeHead(200, { "Content-Type": "application/json" }).end("{}");
  });
  receiver.listen(0, "127.0.0.1");
  await once(receiver, "listening");
  // A repeatable 16 KiB mapper property tests larger payloads. It is synthetic,
  // moderately compressible text, not a claim about future browser metadata.
  const padding = Array.from({ length: 256 }, (_, i) => createHash("sha256").update(String(i)).digest("hex")).join("");
  const variants = {};
  let context;
  try {
    const url = `http://127.0.0.1:${receiver.address().port}`;
    if (browserContext) {
      const { chromium } = await import("@playwright/test");
      const browser = await chromium.launch();
      try {
        const page = await browser.newPage({ locale: "fr-FR", timezoneId: "Europe/Paris", viewport: { width: 960, height: 540 } });
        await page.goto(url);
        await page.evaluate(async (event) => {
          const { createChunkifyAdapter } = await import("/chunkify.js");
          await createChunkifyAdapter(`${location.origin}/native`, { ingestKey: "benchmark-fixture" }).send(event);
        }, events[0]);
        context = captured.shift().browser_context;
        assert.equal(context.browser_name, "Chrome");
        assert.equal(context.viewport_width_px, 960);
      } finally { await browser.close(); }
    }
    for (const size of ["small", "large"]) {
      const map = (event) => ({ ...event, ...(context ? { browser_context: context } : {}),
        ...(size === "large" ? { benchmark_payload: padding } : {}) });
      const native = createHttpAdapter(`${url}/native`, { mapEvent: map });
      variants[size] = [];
      for (const event of events) {
        await native.send(event);
        variants[size].push({ event: captured.shift() });
      }
    }
  } finally { await new Promise((resolve) => receiver.close(resolve)); }

  // Keep captured templates once. A retained replay description contains only
  // the chosen template and the identity/timestamp fields that changed.
  const replayRecord = ({ position, large, fields }) => {
    const template = variants[large ? "large" : "small"][position];
    const event = { ...template.event, ...fields };
    return { event };
  };

  return {
    description: { event_names: [...new Set(events.map((event) => event.event_name))],
      events_per_view: events.length, recorded_playing_time_ms: events.at(-1).playing_time_ms,
      large_property_bytes: padding.length, videos: 10, viewers: 1000,
      browser_context: context ?? null },
    replay: replayRecord,
    generator() {
      let index = 0;
      let view;
      let offset;
      return (large = false) => {
        const position = index % events.length;
        const viewIndex = Math.floor(index++ / events.length);
        if (position === 0) {
          view = compactUuid();
          offset = Date.now() - Date.parse(events[0].occurred_at);
        }
        const template = variants[large ? "large" : "small"][position];
        const fields = { event_id: compactUuid(), view_id: view, video_id: `video-${viewIndex % 10}`,
          viewer_id: `viewer-${viewIndex % 1000}`,
          occurred_at: new Date(Date.parse(template.event.occurred_at) + offset).toISOString() };
        const replay = { position, large, fields };
        return { ...replayRecord(replay), replay };
      };
    },
  };
}

// Copy crypto's UUID into a compact string before it is retained in large Maps
// and Sets. This preserves the UUID bytes without retaining its component strings.
function compactUuid() {
  return Buffer.from(randomUUID(), "ascii").toString("ascii");
}
