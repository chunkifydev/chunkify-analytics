import assert from "node:assert/strict";
import { statfs } from "node:fs/promises";
import { monitorEventLoopDelay } from "node:perf_hooks";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import { gzip } from "node:zlib";
import { containerCollector, docker, image } from "./support.mjs";
import { captureWorkload, originalOtlp, otlpBody } from "./workload.mjs";

export { captureWorkload };
const compress = promisify(gzip);
const mib = 1024 * 1024;
export const thresholds = { ingestion_p95_ms: 250, query_p95_ms: 500,
  successful_request_fraction: 0.99, scheduled_request_fraction: 0.99, schedule_lag_p95_ms: 20 };
export const safety = { max_data_mib: 512, min_free_disk_mib: 4096,
  max_container_memory_mib: 1800, max_generator_rss_mib: 1024,
  max_unique_events: 200000, request_timeout_ms: 10000, max_schedule_lag_ms: 50,
  max_generator_pause_ms: 5000 };
export const resources = ["--cpus=2", "--memory=2g", "--memory-swap=2g"];

export function distribution(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const percentile = (p) => sorted.length ? +sorted[Math.ceil(sorted.length * p) - 1].toFixed(2) : null;
  return { count: sorted.length, p50_ms: percentile(0.5), p95_ms: percentile(0.95), p99_ms: percentile(0.99) };
}

export async function hostFreeBytes() {
  const fs = await statfs(process.cwd());
  return fs.bavail * fs.bsize;
}

// One Docker exec per sample. cgroup v2 memory includes filesystem cache; RSS
// does not. These are sampled values, not continuous profiling or exact peaks.
async function sample(server) {
  const raw = await docker(["exec", server.name, "sh", "-c",
    "cat /sys/fs/cgroup/memory.current /sys/fs/cgroup/cpu.stat /proc/1/status; du -sb /data; df -Pk /data; find /data -name '*.wal' -printf 'wal_bytes %s\\n'"]);
  const disk = raw.split("\n").find((line) => /^\S+\s+\d+\s+\d+\s+\d+\s+\d+%\s/.test(line))?.trim().split(/\s+/);
  assert.ok(disk, "container disk measurement must be available");
  const point = { container_memory_bytes: Number(raw.split("\n")[0]),
    cpu_usage_usec: Number(raw.match(/^usage_usec (\d+)/m)?.[1]),
    process_rss_bytes: Number(raw.match(/^VmRSS:\s+(\d+) kB$/m)?.[1]) * 1024,
    data_bytes: Number(raw.match(/^(\d+)\s+\/data$/m)?.[1]),
    wal_bytes: [...raw.matchAll(/^wal_bytes (\d+)/gm)].reduce((sum, match) => sum + Number(match[1]), 0),
    volume_free_bytes: Number(disk[3]) * 1024, host_free_bytes: await hostFreeBytes(),
    generator_rss_bytes: process.memoryUsage().rss };
  for (const [key, value] of Object.entries(point)) {
    assert.ok(Number.isFinite(value) && value >= 0, `Resource sample unavailable: ${key}. The benchmark requires Linux cgroup v2.`);
  }
  return point;
}

function checkSafety(point) {
  if (point.data_bytes > safety.max_data_mib * mib) return "data directory size limit";
  if (Math.min(point.volume_free_bytes, point.host_free_bytes) < safety.min_free_disk_mib * mib) return "free disk floor";
  if (point.container_memory_bytes > safety.max_container_memory_mib * mib) return "container memory threshold";
  if (point.generator_rss_bytes > safety.max_generator_rss_mib * mib) return "load generator memory threshold";
  return null;
}

function expectedSummary(records) {
  const views = new Map();
  const measured = new Map();
  for (const { event } of records.values()) {
    if (event.event_name === "chunkify.video.view_started") views.set(event.view_id, event);
    if (!["chunkify.video.view_snapshot", "chunkify.video.view_ended"].includes(event.event_name)) continue;
    const previous = measured.get(event.view_id);
    if (!previous || (previous.event_name !== "chunkify.video.view_ended" && event.event_name === "chunkify.video.view_ended") ||
      (previous.event_name === event.event_name && event.sequence > previous.sequence)) measured.set(event.view_id, event);
  }
  return { views: views.size,
    unique_viewers: new Set([...views.values()].map((event) => event.viewer_id).filter((id) => id !== null)).size,
    total_playing_time_ms: [...views.keys()].reduce((sum, id) => sum + (measured.get(id)?.playing_time_ms ?? 0), 0) };
}

function assertSummary(actual, expected) {
  assert.equal(actual.views, expected.views);
  assert.equal(actual.unique_viewers, expected.unique_viewers);
  assert.ok(Math.abs(actual.total_playing_time_ms - expected.total_playing_time_ms) < 0.01, "cumulative playtime must match retained view measurements");
}

async function verify(server, sent, acknowledged, preloadIds, workload) {
  const retained = new Map();
  let cursor;
  do {
    const page = await server.get(`/v1/events?limit=1000${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`);
    for (const record of page.events) {
      const id = record.event.event_id;
      assert.ok(!retained.has(id), "pagination must not repeat events");
      const submitted = sent.get(id);
      assert.ok(submitted, "stored event must have been submitted");
      const expected = workload.replay(submitted.replay);
      assert.deepEqual(record.event, expected.event, "full canonical payload must survive restart");
      if (expected.event.browser_context) {
        for (const [key, value] of Object.entries(expected.event.browser_context)) {
          assert.deepEqual(record.event_context[key], value, "flat browser context must survive restart");
        }
        assert.ok(record.event_context.client_ip, "request IP must remain alongside browser context");
      }
      if (submitted.format === "otlp") assert.deepEqual(record.otlp, originalOtlp(expected), "original OTLP context must survive restart");
      else assert.equal(record.otlp, undefined);
      assert.ok(Number.isFinite(Date.parse(record.received_at)));
      // Full payloads were checked above. Only these fields are needed for the
      // independent summary calculations after pagination finishes.
      const { event_name, view_id, video_id, viewer_id, sequence, playing_time_ms } = expected.event;
      retained.set(id, { event: { event_name, view_id, video_id, viewer_id, sequence, playing_time_ms } });
    }
    cursor = page.next_cursor;
  } while (cursor);
  for (const id of acknowledged) assert.ok(retained.has(id), "every acknowledged event must survive restart");
  const summary = await server.get("/v1/summary");
  assertSummary(summary, expectedSummary(retained));
  const videos = await server.get("/v1/videos?limit=1000");
  const videoIds = new Set([...retained.values()].filter(({ event }) => event.event_name === "chunkify.video.view_started").map(({ event }) => event.video_id));
  assert.equal(videos.videos.length, videoIds.size);
  for (const video of videos.videos) {
    const selected = new Map([...retained].filter(([, { event }]) => event.video_id === video.video_id));
    assertSummary(video, expectedSummary(selected));
    assertSummary(await server.get(`/v1/summary?video_id=${encodeURIComponent(video.video_id)}`), expectedSummary(selected));
  }
  return { summary, persisted_new: retained.size - preloadIds.size,
    persisted_without_ack: [...retained.keys()].filter((id) => !acknowledged.has(id)).length,
    verification: { payloads: true, otlp_context: true, summary: true, per_video: true, acknowledged_events: acknowledged.size } };
}

export async function measure(input, workload, { signal, fixture } = {}) {
  const config = { batch_size: 1, gzip: false, duplicate_every: 0, large_every: 0,
    preload: 0, concurrency: 128, shutdown: "stop", rate_limit: 10000, queries: true, profile: false, ...input };
  if (await hostFreeBytes() < safety.min_free_disk_mib * mib) throw new Error("Benchmark requires at least 4 GiB free host disk");
  const settings = { COLLECTOR_INGEST_PER_SECOND: String(config.rate_limit), COLLECTOR_MAX_BODY_BYTES: "1048576",
    COLLECTOR_MAX_OTLP_RECORDS: "100", COLLECTOR_RETENTION_SECONDS: "2592000", COLLECTOR_RETENTION_INTERVAL_SECONDS: "5",
    COLLECTOR_PROFILE: String(config.profile), ...config.write_settings };
  const server = await containerCollector({ resources, settings, seed: fixture?.volume });
  const sent = new Map();
  const acknowledged = new Set();
  const preloadIds = new Set();
  let makeRecord = workload.generator();
  const duplicatePool = [];
  let generation = 0;
  let abortReason = null;
  let verified = false;
  const interrupt = () => { abortReason = "interrupted"; };
  signal?.addEventListener("abort", interrupt, { once: true });
  if (signal?.aborted) interrupt();
  let monitoring;
  let running = false;
  const loopDelay = monitorEventLoopDelay({ resolution: 10 });
  const samples = [];
  const queryResults = Object.fromEntries(["summary", "videos", "events"].map((name) => [name, { latency: [], failures: {} }]));
  const latencies = [];
  const successLatencies = [];
  const scheduleLag = [];
  const preparation = [];
  const requests = { scheduled: 0, sent: 0, successful: 0, skipped_concurrency: 0, skipped_late: 0, statuses: {}, failures: {} };
  const records = { scheduled: 0, submitted: 0, acknowledged: 0, rejected: 0, uncertain: 0 };
  let wireBytes = 0;
  let decodedBytes = 0;
  let maxInflight = 0;

  function fresh(count) {
    return Array.from({ length: count }, () => makeRecord(config.large_every > 0 && ++generation % config.large_every === 0));
  }
  async function send(batch, format, measured = true) {
    const prepareStart = performance.now();
    const json = JSON.stringify(format === "native" ? batch[0].event : otlpBody(batch));
    const body = config.gzip && format === "otlp" ? await compress(json) : json;
    if (measured) {
      preparation.push(performance.now() - prepareStart);
      wireBytes += Buffer.byteLength(body);
      decodedBytes += Buffer.byteLength(json);
      requests.sent++;
      records.submitted += batch.length;
    }
    for (const record of batch) if (!sent.has(record.event.event_id)) {
      sent.set(record.event.event_id, { replay: record.replay, format });
    }
    const start = performance.now();
    try {
      const response = await fetch(`${server.url}${format === "native" ? "/v1/events" : "/v1/logs"}`, {
        method: "POST", body, signal: AbortSignal.timeout(safety.request_timeout_ms),
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.ingest_key,
          ...(config.gzip && format === "otlp" ? { "Content-Encoding": "gzip" } : {}) },
      });
      await response.text();
      if (measured) requests.statuses[response.status] = (requests.statuses[response.status] ?? 0) + 1;
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      for (const record of batch) acknowledged.add(record.event.event_id);
      if (duplicatePool.length < 1000) duplicatePool.push(...batch.slice(0, 1000 - duplicatePool.length));
      if (measured) {
        requests.successful++;
        records.acknowledged += batch.length;
        successLatencies.push(performance.now() - start);
      }
    } catch (error) {
      if (!measured) throw error;
      const reason = error.message.match(/^HTTP (\d+)$/)?.[1] ?? (error.name === "TimeoutError" ? "timeout" : "transport");
      requests.failures[reason] = (requests.failures[reason] ?? 0) + 1;
      if (/^4\d\d$/.test(reason) || reason === "503") records.rejected += batch.length;
      else records.uncertain += batch.length;
    } finally { if (measured) latencies.push(performance.now() - start); }
  }

  try {
    const queueSamples = [];
    let trafficStart = null;
    const queueEnabled = settings.COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE === "true";
    const queueSample = async () => {
      if (!queueEnabled) return null;
      const result = await server.get("/v1/ingestion");
      const value = { at: Date.now(), ...result.queue };
      queueSamples.push(value);
      if (value.failed) throw new Error("Durable queue worker failed");
      return value;
    };
    const waitForQueue = async (started = performance.now()) => {
      while (true) {
        const point = await queueSample();
        if (!point || point.pending_requests === 0) return performance.now() - started;
        if (performance.now() - started > 20000) { abortReason ??= "Queue did not drain within 20 seconds"; return performance.now() - started; }
        const resources = await sample(server);
        samples.push({ phase: trafficStart === null ? "preload_drain" : "queue_drain", at_seconds: trafficStart === null ? 0 : +((performance.now() - trafficStart) / 1000).toFixed(3), ...resources });
        const reason = checkSafety(resources);
        if (reason) { abortReason ??= `Queue drain safety stop: ${reason}`; return performance.now() - started; }
        await delay(100);
      }
    };
    const initial = await sample(server);
    abortReason ??= checkSafety(initial);
    if (abortReason) throw new Error(`Safety stop before preload: ${abortReason}`);
    const preloadStart = performance.now();
    const reusedFixture = Boolean(fixture?.volume);
    if (reusedFixture) {
      assert.equal(config.preload, fixture.count, "fixture must match preload count");
      for (const [id, replay] of fixture.sent) { sent.set(id, replay); acknowledged.add(id); }
    }
    for (let count = reusedFixture ? config.preload : 0; count < config.preload; count += 100) {
      if (signal?.aborted) throw new Error("Benchmark interrupted during preload");
      await send(fresh(Math.min(100, config.preload - count)), "otlp", false);
      if (count % 1000 === 0) {
        const reason = checkSafety(await sample(server));
        if (reason) throw new Error(`Safety stop during preload: ${reason}`);
      }
      if ((count + 100) % 10000 === 0) console.error(`Preloaded ${Math.min(count + 100, config.preload)} / ${config.preload} events...`);
    }
    await waitForQueue();
    for (const id of acknowledged) preloadIds.add(id);
    const preloadSeconds = (performance.now() - preloadStart) / 1000;
    if (fixture && !fixture.volume) {
      await server.stop();
      fixture.volume = await server.snapshot();
      fixture.count = config.preload;
      fixture.sent = [...sent];
      await server.start();
    }
    if (fixture) { makeRecord = workload.generator(); duplicatePool.length = 0; generation = 0; }
    const before = await sample(server);
    const start = performance.now();
    trafficStart = start;
    const cpuBefore = process.cpuUsage();
    loopDelay.enable();
    running = true;
    let eventCursor;
    const query = async (name, path) => {
      const beforeQuery = performance.now();
      try {
        const page = await server.get(path);
        if (name === "events") eventCursor = page.next_cursor;
        queryResults[name].latency.push(performance.now() - beforeQuery);
      } catch (error) {
        const reason = error.message.match(/returned (\d+)/)?.[1] ?? error.name;
        const failures = queryResults[name].failures;
        failures[reason] = (failures[reason] ?? 0) + 1;
      }
    };
    monitoring = (async () => {
      while (running) {
        if (config.queries) await Promise.all([
          query("summary", "/v1/summary"), query("videos", "/v1/videos?limit=10"),
          query("events", `/v1/events?limit=100${eventCursor ? `&cursor=${encodeURIComponent(eventCursor)}` : ""}`),
        ]);
        const point = { at_seconds: +((performance.now() - start) / 1000).toFixed(3), ...await sample(server) };
        samples.push(point);
        await queueSample();
        abortReason ??= checkSafety(point);
        if (abortReason) break;
        await delay(1000);
      }
    })().catch((error) => { abortReason = `monitor failed: ${error.message}`; });

    // An open schedule offers work independently of response completion. When
    // the bounded generator is full or late, record the omitted request rather
    // than hiding it in achieved throughput or accumulating an unbounded queue.
    const phases = config.phases ?? [{ name: "steady", rate: config.rate, seconds: config.seconds }];
    const phaseResults = [];
    const inflight = new Set();
    for (const phase of phases) {
      if (abortReason) break;
      const phaseStart = performance.now();
      const phaseLatencies = latencies.length;
      const countsBefore = structuredClone(requests);
      const scheduled = Math.ceil(phase.rate * phase.seconds);
      let lastProgress = performance.now();
      requests.scheduled += scheduled;
      records.scheduled += config.format === "mixed" ? Math.ceil(scheduled / 2) + Math.floor(scheduled / 2) * config.batch_size
        : scheduled * (config.format === "native" ? 1 : config.batch_size);
      for (let ticket = 0; ticket < scheduled && !abortReason; ticket++) {
        const due = phaseStart + ticket * 1000 / phase.rate;
        if (due > performance.now()) await delay(Math.max(1, due - performance.now()));
        const now = performance.now();
        if (now - lastProgress > safety.max_generator_pause_ms) {
          abortReason = "load generator paused for more than 5 seconds; capacity measurement invalid";
          break;
        }
        lastProgress = now;
        const lag = now - due;
        if (lag > safety.max_schedule_lag_ms) { requests.skipped_late++; continue; }
        if (inflight.size >= config.concurrency) { requests.skipped_concurrency++; continue; }
        if (sent.size + config.batch_size > safety.max_unique_events) { abortReason = "unique event safety limit"; break; }
        scheduleLag.push(lag);
        const requestIndex = requests.sent + inflight.size;
        const format = config.format === "mixed" ? (ticket % 2 ? "otlp" : "native") : config.format;
        const count = format === "native" ? 1 : config.batch_size;
        const duplicate = config.duplicate_every > 0 && (ticket + 1) % config.duplicate_every === 0 && duplicatePool.length > 0;
        const batch = duplicate ? Array.from({ length: count }, (_, i) => duplicatePool[(requestIndex + i) % duplicatePool.length]) : fresh(count);
        const promise = send(batch, format).catch((error) => { abortReason = `sender failed: ${error.message}`; });
        inflight.add(promise);
        maxInflight = Math.max(maxInflight, inflight.size);
        void promise.finally(() => inflight.delete(promise));
      }
      const remaining = phase.seconds * 1000 - (performance.now() - phaseStart);
      if (remaining > 0 && !abortReason) await delay(remaining);
      // Drain between phases so the recovery result is attributable. Report the
      // drain separately; it is not part of the offered-traffic window.
      const drainStart = performance.now();
      await Promise.all(inflight);
      phaseResults.push({ ...phase, drain_ms: +(performance.now() - drainStart).toFixed(2),
        sent: requests.sent - countsBefore.sent, successful: requests.successful - countsBefore.successful,
        skipped_concurrency: requests.skipped_concurrency - countsBefore.skipped_concurrency,
        skipped_late: requests.skipped_late - countsBefore.skipped_late,
        request_latency: distribution(latencies.slice(phaseLatencies)) });
    }
    const elapsed = (performance.now() - start) / 1000;
    const generatorCpu = process.cpuUsage(cpuBefore);
    loopDelay.disable();
    running = false;
    const queueDrainStarted = performance.now();
    const queueAtTrafficEnd = await queueSample();
    const queueDrainMs = await waitForQueue(queueDrainStarted);
    await monitoring;
    const after = await sample(server);
    samples.push({ at_seconds: +((performance.now() - start) / 1000).toFixed(3), ...after });
    abortReason ??= checkSafety(after);
    const stopStart = performance.now();
    const stopped = await server.stop(config.shutdown);
    const profiles = server.lastLogs.split("\n").filter((line) => line.startsWith('{"type":"collector_profile"')).map((line) => JSON.parse(line));
    const collectorProfile = profiles.find((profile) => !profile.worker || profile.worker === "writer") ?? null;
    const readerProfile = profiles.find((profile) => profile.worker === "reader") ?? null;
    if (config.profile && config.shutdown === "stop") assert.ok(collectorProfile, "collector profile must be available after graceful shutdown");
    if (config.profile && config.shutdown === "stop" && settings.COLLECTOR_EXPERIMENTAL_READ_WORKER === "true") assert.ok(readerProfile, "separate reader profile must be available after graceful shutdown");
    const shutdownMs = performance.now() - stopStart;
    assert.equal(stopped.OOMKilled, false, "benchmark must stay within memory limits");
    assert.equal(stopped.ExitCode, config.shutdown === "kill" ? 137 : 0);
    const restartStart = performance.now();
    await server.start();
    const restartMs = performance.now() - restartStart;
    const checked = await verify(server, sent, acknowledged, preloadIds, workload);
    verified = true;
    await server.stop();
    const dataBytes = Number((await docker(["run", "--rm", "--mount", `type=volume,src=${server.volume},dst=/data,readonly`,
      "--entrypoint", "du", image, "-sb", "/data"])).split(/\s/)[0]);
    const queries = Object.fromEntries(Object.entries(queryResults).map(([key, value]) => [key,
      { latency: distribution(value.latency), failures: value.failures }]));
    const latency = distribution(latencies);
    const successfulLatency = distribution(successLatencies);
    const lag = distribution(scheduleLag);
    const queryFailures = Object.values(queries).some((value) => Object.keys(value.failures).length > 0);
    const offeredSeconds = phases.reduce((sum, phase) => sum + phase.seconds, 0);
    const pass = !abortReason && requests.sent / requests.scheduled >= thresholds.scheduled_request_fraction &&
      requests.successful / requests.sent >= thresholds.successful_request_fraction && successfulLatency.p95_ms !== null &&
      successfulLatency.p95_ms <= thresholds.ingestion_p95_ms && lag.p95_ms <= thresholds.schedule_lag_p95_ms &&
      !queryFailures && (!config.queries || Object.values(queries).every((value) => value.latency.count > 0 && value.latency.p95_ms <= thresholds.query_p95_ms));
    const peak = (field) => Math.max(before[field], ...samples.map((point) => point[field]));
    return { ...config, collector_settings: settings, completed: !abortReason, abort_reason: abortReason,
      elapsed_seconds: +elapsed.toFixed(3), offered_seconds: offeredSeconds, phases: phaseResults,
      preload: { reused_fixture: reusedFixture, events: preloadIds.size, seconds: +preloadSeconds.toFixed(3), data_bytes: before.data_bytes },
      requests: { ...requests, sent_per_second: +(requests.sent / elapsed).toFixed(2),
        successful_per_second: +(requests.successful / elapsed).toFixed(2) },
      records: { ...records, newly_acknowledged: acknowledged.size - preloadIds.size,
        duplicate_acknowledgements: records.acknowledged - (acknowledged.size - preloadIds.size),
        newly_acknowledged_per_second: +((acknowledged.size - preloadIds.size) / elapsed).toFixed(2),
        persisted_new: checked.persisted_new, persisted_without_ack: checked.persisted_without_ack },
      durable_queue: queueEnabled ? { samples: queueSamples, after_http_completion: queueAtTrafficEnd, drain_ms: +queueDrainMs.toFixed(2) } : null,
      request_latency: latency, successful_request_latency: successfulLatency, schedule_lag: lag,
      preparation_latency: distribution(preparation), queries, collector_profile: collectorProfile, reader_profile: readerProfile, max_inflight: maxInflight,
      payload: { total_wire_bytes: wireBytes, total_decoded_bytes: decodedBytes,
        mean_wire_bytes_per_request: requests.sent ? Math.round(wireBytes / requests.sent) : null },
      resources: { samples, peak_process_rss_mib: +(peak("process_rss_bytes") / mib).toFixed(2),
        peak_container_memory_mib: +(peak("container_memory_bytes") / mib).toFixed(2),
        peak_generator_rss_mib: +(peak("generator_rss_bytes") / mib).toFixed(2),
        peak_wal_bytes: peak("wal_bytes"), checkpointed_data_bytes: dataBytes,
        mean_container_cpu_cores: +((after.cpu_usage_usec - before.cpu_usage_usec) / 1e6 / (samples.at(-1).at_seconds)).toFixed(2),
        generator_cpu_cores: +((generatorCpu.user + generatorCpu.system) / 1e6 / elapsed).toFixed(2),
        generator_event_loop_p99_ms: +(loopDelay.percentile(99) / 1e6).toFixed(2) },
      restart: { shutdown_ms: +shutdownMs.toFixed(2), startup_ms: +restartMs.toFixed(2) },
      summary: checked.summary, verification: { ...checked.verification, shutdown: config.shutdown },
      meets_probe_thresholds: pass,
      meets_full_delivery_thresholds: pass && requests.sent === requests.scheduled && requests.successful === requests.sent,
      sustainable_capacity_established: false };
  } finally {
    running = false;
    signal?.removeEventListener("abort", interrupt);
    loopDelay.disable();
    await monitoring;
    const preserveVolume = Boolean(config.write_settings?.COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE === "true" && acknowledged.size && !verified);
    if (preserveVolume) console.error(`Unverified acknowledged events retained in Docker volume ${server.volume}`);
    await server.close({ preserveVolume });
  }
}
