import assert from "node:assert/strict";
import test from "node:test";
import { captureWorkload, encodeSubmission, measure, safety, verify } from "./benchmark.mjs";

test("event limits reject oversized preloads and fixtures before starting Docker", async () => {
  await assert.rejects(measure({ preload: safety.max_unique_events + 1 }, null), /Preload exceeds/);
  await assert.rejects(measure({ preload: 1 }, null, { fixture: { volume: "must-not-be-used", count: 1,
    sent: { length: safety.max_unique_events + 1 } } }), /fixture exceeds/);
});

test("streamed verification preserves every payload check and rejects missing or repeated acknowledgements", async (t) => {
  const workload = await captureWorkload();
  const generate = workload.generator();
  const records = Array.from({ length: workload.description.events_per_view * 3 }, () => generate());
  const ids = records.map((record) => record.event.event_id);
  const summary = (views) => ({ views, unique_viewers: views, total_playing_time_ms: workload.description.recorded_playing_time_ms * views });
  const pages = records.map((record) => ({ event: structuredClone(record.event),
    received_at: new Date().toISOString() }));
  async function check(change = () => {}) {
    const sent = new Map(records.map((record) => [record.event.event_id, encodeSubmission(record)]));
    const stored = structuredClone(pages);
    change(stored);
    let offset = 0;
    const server = { async get(path) {
      if (path.startsWith("/v1/events")) {
        const events = stored.slice(offset, offset += 4);
        return { events, next_cursor: offset < stored.length ? String(offset) : null };
      }
      if (path === "/v1/videos?limit=1000") return { videos: [0, 1, 2].map((id) => ({ video_id: `video-${id}`, ...summary(1) })) };
      return summary(path.includes("video_id") ? 1 : 3);
    } };
    return verify(server, sent, new Set(ids), new Set(), workload);
  }
  await t.test("complete records and cumulative summaries", async () => {
    const result = await check();
    assert.equal(result.persisted_new, records.length);
    assert.equal(result.persisted_without_ack, 0);
    assert.equal(result.verification.acknowledged_events, records.length);
  });
  await t.test("missing acknowledged event", () => assert.rejects(check((rows) => rows.pop()), /every acknowledged event/));
  await t.test("duplicate across pages", () => assert.rejects(check((rows) => rows.push(rows[0])), /pagination must not repeat/));
  await t.test("changed payload", () => assert.rejects(check((rows) => { rows[0].event.video_id = "wrong"; }), /full canonical payload/));
});
