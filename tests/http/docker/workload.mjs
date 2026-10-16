import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import { setTimeout as delay } from "node:timers/promises";
import { attachAnalytics } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";

// Record a real SDK playback and the public adapters' wire output before timing.
// Load generation replays these templates with new identities and timestamps.
export async function captureWorkload() {
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
  try {
    const url = `http://127.0.0.1:${receiver.address().port}`;
    for (const size of ["small", "large"]) {
      const map = size === "large" ? (event) => ({ ...event, benchmark_payload: padding }) : (event) => event;
      const native = createHttpAdapter(`${url}/native`, { mapEvent: map });
      const otlp = createOtlpAdapter(`${url}/otlp`, { mapAttributes: map });
      variants[size] = [];
      for (const event of events) {
        await native.send(event);
        const canonical = captured.shift();
        await otlp.send(event);
        const request = captured.shift();
        const scope = request.resourceLogs[0].scopeLogs[0];
        variants[size].push({ event: canonical, log: scope.logRecords[0], scope: scope.scope });
      }
    }
  } finally { await new Promise((resolve) => receiver.close(resolve)); }

  return {
    description: { event_names: [...new Set(events.map((event) => event.event_name))],
      events_per_view: events.length, recorded_playing_time_ms: events.at(-1).playing_time_ms,
      large_property_bytes: padding.length, videos: 10, viewers: 1000 },
    generator() {
      let index = 0;
      let view;
      let offset;
      return (large = false) => {
        const position = index % events.length;
        const viewIndex = Math.floor(index++ / events.length);
        if (position === 0) {
          view = randomUUID();
          offset = Date.now() - Date.parse(events[0].occurred_at);
        }
        const template = variants[large ? "large" : "small"][position];
        const fields = { event_id: randomUUID(), view_id: view, video_id: `video-${viewIndex % 10}`,
          viewer_id: `viewer-${viewIndex % 1000}`,
          occurred_at: new Date(Date.parse(template.event.occurred_at) + offset).toISOString() };
        const event = { ...template.event, ...fields };
        const log = { ...template.log, timeUnixNano: String(BigInt(Date.parse(event.occurred_at)) * 1000000n),
          attributes: template.log.attributes.map((attribute) => Object.hasOwn(fields, attribute.key)
            ? { key: attribute.key, value: { stringValue: fields[attribute.key] } } : attribute) };
        return { event, log, scope: template.scope };
      };
    },
  };
}

export function otlpBody(records) {
  return { resourceLogs: [{ scopeLogs: [{ scope: records[0].scope, logRecords: records.map(({ log }) => log) }] }] };
}

export function originalOtlp(record) {
  return { resource_logs: {}, scope_logs: { scope: record.scope }, log_record: record.log };
}
