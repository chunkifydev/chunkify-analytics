import assert from "node:assert/strict";
import test from "node:test";
import { captureWorkload, measure } from "./benchmark.mjs";

test("benchmark distinguishes duplicate records from new events and verifies playback after a crash", { timeout: 120000 }, async () => {
  const workload = await captureWorkload({ browserContext: true });
  const result = await measure({ name: "accounting-check", format: "mixed", batch_size: 5,
    gzip: true, duplicate_every: 2, large_every: 3, rate: 10, seconds: 2, preload: 20, shutdown: "kill" }, workload);
  assert.equal(result.completed, true);
  assert.equal(result.requests.scheduled, 20);
  assert.equal(result.requests.sent + result.requests.skipped_late + result.requests.skipped_concurrency, 20);
  assert.equal(result.requests.successful, result.requests.sent);
  assert.deepEqual(result.requests.failures, {});
  assert.ok(result.records.acknowledged > result.records.newly_acknowledged);
  assert.ok(result.records.duplicate_acknowledgements > 0);
  assert.equal(result.records.persisted_new, result.records.newly_acknowledged);
  assert.ok(result.summary.total_playing_time_ms > 0);
  assert.equal(result.verification.payloads, true);
  assert.equal(result.verification.otlp_context, true);
  assert.equal(result.verification.summary, true);
  assert.equal(result.verification.shutdown, "kill");
  assert.ok(result.queries.summary.latency.count > 0);
  assert.ok(result.queries.videos.latency.count > 0);
  assert.ok(result.queries.events.latency.count > 0);
});

test("benchmark reports rate-limited OTLP records separately from accepted records", { timeout: 120000 }, async () => {
  const result = await measure({ name: "rejection-check", format: "otlp", batch_size: 5,
    rate: 40, rate_limit: 10, seconds: 2, profile: true,
    write_settings: { "experimental.read_worker": "true" } }, await captureWorkload());
  assert.equal(result.requests.scheduled, 80);
  assert.equal(result.requests.sent + result.requests.skipped_late + result.requests.skipped_concurrency, 80);
  assert.ok(result.requests.failures[429] > 0);
  assert.equal(result.records.rejected, result.requests.failures[429] * 5);
  assert.equal(result.records.acknowledged, result.requests.successful * 5);
  assert.equal(result.records.persisted_new, result.records.acknowledged);
  assert.equal(result.records.duplicate_acknowledgements, 0);
  assert.equal(result.records.uncertain, 0);
  assert.equal(result.collector_profile.ingest.requests, result.requests.successful);
  assert.ok(result.collector_profile.ingest.transactions <= result.requests.successful);
  assert.equal(result.collector_profile.ingest.received_records, result.records.acknowledged);
  assert.equal(result.collector_profile.ingest.inserted_records, result.records.newly_acknowledged);
  assert.equal(result.collector_profile.ingest.max_records, result.collector_profile.ingest.max_requests * 5);
  assert.equal(result.reader_profile.worker, "reader");
  assert.equal(result.collector_profile.worker, "writer");
  for (const [name, query] of Object.entries(result.queries)) {
    assert.ok(query.latency.count > 0);
    assert.deepEqual(query.failures, {});
    assert.equal(result.reader_profile.jobs[name].execution.count, query.latency.count);
    assert.equal(result.collector_profile.jobs[name], undefined);
  }
  assert.equal(result.meets_probe_thresholds, false);
  assert.equal(result.meets_full_delivery_thresholds, false);
});
