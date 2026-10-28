import assert from "node:assert/strict";
import { statfs } from "node:fs/promises";
import { monitorEventLoopDelay } from "node:perf_hooks";
import { setTimeout as delay } from "node:timers/promises";
import { containerCollector, docker, image } from "./support.mjs";
import { captureWorkload } from "./workload.mjs";
import { containerMemoryGuardMiB, parseMemoryStats, resources } from "./resources.mjs";

export { captureWorkload };
const mib = 1024 * 1024;
export const thresholds = { ingestion_p95_ms: 250, query_p95_ms: 500,
  successful_request_fraction: 0.99, scheduled_request_fraction: 0.99, schedule_lag_p95_ms: 20 };
function limit(name, fallback, min, max) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`${name} must be ${min}..${max}`);
  return value;
}
export const safety = { max_data_mib: limit("CHUNKIFY_LOAD_MAX_DATA_MIB", 512, 512, 4096), min_free_disk_mib: 4096,
  max_container_memory_mib: containerMemoryGuardMiB, max_generator_rss_mib: 1024,
  max_unique_events: limit("CHUNKIFY_LOAD_MAX_EVENTS", 200000, 1, 1000000), request_timeout_ms: 10000, max_schedule_lag_ms: 50,
  max_generator_pause_ms: 5000 };
export { resources };

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
    "cat /sys/fs/cgroup/memory.current /sys/fs/cgroup/cpu.stat /proc/1/status /sys/fs/cgroup/memory.stat /sys/fs/cgroup/memory.events /sys/fs/cgroup/memory.pressure; du -sb /data; df -Pk /data; find /data -name '*.wal' -printf 'wal_bytes %s\\n'"]);
  const disk = raw.split("\n").find((line) => /^\S+\s+\d+\s+\d+\s+\d+\s+\d+%\s/.test(line))?.trim().split(/\s+/);
  assert.ok(disk, "container disk measurement must be available");
  const memory = process.memoryUsage();
  const point = { container_memory_bytes: Number(raw.split("\n")[0]),
    cpu_usage_usec: Number(raw.match(/^usage_usec (\d+)/m)?.[1]),
    process_rss_bytes: Number(raw.match(/^VmRSS:\s+(\d+) kB$/m)?.[1]) * 1024,
    data_bytes: Number(raw.match(/^(\d+)\s+\/data$/m)?.[1]),
    wal_bytes: [...raw.matchAll(/^wal_bytes (\d+)/gm)].reduce((sum, match) => sum + Number(match[1]), 0),
    volume_free_bytes: Number(disk[3]) * 1024, host_free_bytes: await hostFreeBytes(),
    generator_rss_bytes: memory.rss, generator_heap_used_bytes: memory.heapUsed,
    generator_heap_total_bytes: memory.heapTotal, generator_external_bytes: memory.external,
    generator_array_buffer_bytes: memory.arrayBuffers, ...parseMemoryStats(raw) };
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

// Keep one start and one cumulative measurement per view while verifying pages.
// Do not retain another complete event ledger alongside the submitted ledger.
function summaryAccumulator() {
  const views = new Map();
  const measured = new Map();
  return {
    add({ event_name, view_id, video_id, viewer_id, sequence, playing_time_ms }) {
      if (event_name === "chunkify.video.view_started") views.set(view_id, { video_id, viewer_id });
      if (!["chunkify.video.view_snapshot", "chunkify.video.view_ended"].includes(event_name)) return;
      const previous = measured.get(view_id);
      if (!previous || (previous.event_name !== "chunkify.video.view_ended" && event_name === "chunkify.video.view_ended") ||
        (previous.event_name === event_name && sequence > previous.sequence)) measured.set(view_id, { event_name, sequence, playing_time_ms });
    },
    videos() { return new Set([...views.values()].map((event) => event.video_id)); },
    summary(videoId) {
      let count = 0;
      let time = 0;
      const viewers = new Set();
      for (const [id, event] of views) if (videoId === undefined || event.video_id === videoId) {
        count++;
        if (event.viewer_id !== null) viewers.add(event.viewer_id);
        time += measured.get(id)?.playing_time_ms ?? 0;
      }
      return { views: count, unique_viewers: viewers.size, total_playing_time_ms: time };
    },
  };
}

// Encode the replay fields once, omitting the event ID already used as Map key.
// Strings avoid retaining several small objects for each of a million events.
export function encodeSubmission(record) {
  const { event_id, ...fields } = record.replay.fields;
  return JSON.stringify([record.replay.position, record.replay.large, fields]);
}

function assertSummary(actual, expected) {
  assert.equal(actual.views, expected.views);
  assert.equal(actual.unique_viewers, expected.unique_viewers);
  assert.ok(Math.abs(actual.total_playing_time_ms - expected.total_playing_time_ms) < 0.01, "cumulative playtime must match retained view measurements");
}

export async function verify(server, sent, acknowledged, preloadIds, workload, onProgress = async () => {}) {
  const totals = summaryAccumulator();
  let retained = 0;
  let unacknowledged = 0;
  let lastProgress = 0;
  let cursor;
  do {
    const page = await server.get(`/v1/events?limit=1000${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`);
    for (const record of page.events) {
      const id = record.event.event_id;
      const encoded = sent.get(id);
      assert.notEqual(encoded, null, "pagination must not repeat events");
      assert.ok(encoded, "stored event must have been submitted");
      const [position, large, fields] = JSON.parse(encoded);
      const expected = workload.replay({ position, large, fields: { ...fields, event_id: id } });
      assert.deepEqual(record.event, expected.event, "full canonical payload must survive restart");
      if (expected.event.browser_context) {
        for (const [key, value] of Object.entries(expected.event.browser_context)) {
          assert.deepEqual(record.event_context[key], value, "flat browser context must survive restart");
        }
        assert.ok(record.event_context.client_ip, "request IP must remain alongside browser context");
      }
      assert.equal(record.otlp, undefined);
      assert.ok(Number.isFinite(Date.parse(record.received_at)));
      totals.add(expected.event);
      // Mark the ID as verified and release its encoded replay data immediately.
      sent.set(id, null);
      retained++;
      if (!acknowledged.has(id)) unacknowledged++;
    }
    cursor = page.next_cursor;
    if (retained - lastProgress >= 10000 || !cursor) {
      await onProgress(retained);
      lastProgress = retained;
    }
  } while (cursor);
  for (const id of acknowledged) assert.equal(sent.get(id), null, "every acknowledged event must survive restart");
  const summary = await server.get("/v1/summary");
  assertSummary(summary, totals.summary());
  const videos = await server.get("/v1/videos?limit=1000");
  const videoIds = totals.videos();
  assert.equal(videos.videos.length, videoIds.size);
  for (const video of videos.videos) {
    assertSummary(video, totals.summary(video.video_id));
    assertSummary(await server.get(`/v1/summary?video_id=${encodeURIComponent(video.video_id)}`), totals.summary(video.video_id));
  }
  return { summary, persisted_new: retained - preloadIds.size,
    persisted_without_ack: unacknowledged,
    verification: { payloads: true, summary: true, per_video: true, acknowledged_events: acknowledged.size } };
}

export async function measure(input, workload, { signal, fixture, onProgress = async () => {} } = {}) {
  const config = { duplicate_every: 0, large_every: 0,
    preload: 0, concurrency: 128, shutdown: "stop", rate_limit: 10000, queries: true, profile: false, ...input };
  if (config.preload > safety.max_unique_events) throw new Error("Preload exceeds the unique-event safety limit");
  if (fixture?.volume) {
    assert.equal(fixture.count, config.preload, "fixture must match preload count");
    assert.ok(fixture.sent.length <= safety.max_unique_events, "fixture exceeds the unique-event safety limit");
  }
  if (await hostFreeBytes() < (safety.min_free_disk_mib + safety.max_data_mib) * mib) throw new Error("Benchmark requires its data budget plus the free-disk floor before starting");
  const settings = { "server.ingest_per_second": String(config.rate_limit), "server.max_body_bytes": "1048576",
    "rate_limits.read_per_minute": "1000000", "rate_limits.read_burst": "1000000",
    // The monitor issues three concurrent reads for the same project.
    "rate_limits.read_concurrent_per_project": "3",
    "retention.seconds": "2592000", "retention.interval_seconds": "5",
    "profile": String(config.profile), ...config.write_settings };
  const server = await containerCollector({ resources, settings, seed: fixture?.volume });
  const sent = new Map();
  const acknowledged = new Set();
  const preloadIds = new Set();
  let makeRecord = workload.generator();
  const duplicatePool = [];
  let generation = 0;
  let abortReason = null;
  const interrupt = () => { abortReason = "interrupted"; };
  signal?.addEventListener("abort", interrupt, { once: true });
  if (signal?.aborted) interrupt();
  let monitoring;
  let running = false;
  const loopDelay = monitorEventLoopDelay({ resolution: 10 });
  const samples = [];
  const verificationSamples = [];
  const windows = [];
  let trafficStart = null;
  function windowAt(time) {
    const index = Math.max(0, Math.floor((time - trafficStart) / 60000));
    return windows[index] ??= { start_seconds: index * 60, scheduled: 0, sent: 0, successful: 0, failed: 0,
      skipped_late: 0, skipped_concurrency: 0, latency: [],
      queries: Object.fromEntries(["summary", "videos", "events"].map((name) => [name, []])) };
  }
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
  async function send(record, measured = true) {
    const prepareStart = performance.now();
    const body = JSON.stringify(record.event);
    if (measured) {
      preparation.push(performance.now() - prepareStart);
      wireBytes += Buffer.byteLength(body);
      decodedBytes += Buffer.byteLength(body);
      requests.sent++;
      records.submitted++;
    }
    if (!sent.has(record.event.event_id)) {
      sent.set(record.event.event_id, encodeSubmission(record));
    }
    const start = performance.now();
    const window = measured ? windowAt(start) : null;
    if (window) window.sent++;
    try {
      const response = await fetch(`${server.url}/v1/events`, {
        method: "POST", body, signal: AbortSignal.timeout(safety.request_timeout_ms),
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": server.keys.public_key },
      });
      await response.text();
      if (measured) requests.statuses[response.status] = (requests.statuses[response.status] ?? 0) + 1;
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      acknowledged.add(record.event.event_id);
      if (duplicatePool.length < 1000) duplicatePool.push(record);
      if (measured) {
        requests.successful++;
        records.acknowledged++;
        const latency = performance.now() - start;
        successLatencies.push(latency);
        window.successful++;
        window.latency.push(latency);
      }
    } catch (error) {
      if (!measured) throw error;
      window.failed++;
      const reason = error.message.match(/^HTTP (\d+)$/)?.[1] ?? (error.name === "TimeoutError" ? "timeout" : "transport");
      requests.failures[reason] = (requests.failures[reason] ?? 0) + 1;
      if (/^4\d\d$/.test(reason) || reason === "503") records.rejected++;
      else records.uncertain++;
    } finally { if (measured) latencies.push(performance.now() - start); }
  }

  try {
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
      await Promise.all(fresh(Math.min(100, config.preload - count)).map(record => send(record, false)));
      if (count % 1000 === 0) {
        const reason = checkSafety(await sample(server));
        if (reason) throw new Error(`Safety stop during preload: ${reason}`);
      }
      if ((count + 100) % 10000 === 0) console.error(`Preloaded ${Math.min(count + 100, config.preload)} / ${config.preload} events...`);
    }
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
        const latency = performance.now() - beforeQuery;
        queryResults[name].latency.push(latency);
        windowAt(beforeQuery).queries[name].push(latency);
      } catch (error) {
        const reason = error.message.match(/returned (\d+)/)?.[1] ?? error.name;
        const failures = queryResults[name].failures;
        failures[reason] = (failures[reason] ?? 0) + 1;
      }
    };
    monitoring = (async () => {
      let lastProgress = 0;
      while (running) {
        if (config.queries) await Promise.all([
          query("summary", "/v1/summary"), query("videos", "/v1/videos?limit=10"),
          query("events", `/v1/events?limit=100${eventCursor ? `&cursor=${encodeURIComponent(eventCursor)}` : ""}`),
        ]);
        const point = { at_seconds: +((performance.now() - start) / 1000).toFixed(3), ...await sample(server) };
        samples.push(point);
        if (point.at_seconds - lastProgress >= 30) {
          await onProgress({ phase: "traffic", at_seconds: point.at_seconds, requests: structuredClone(requests), sample: point });
          console.error(`Traffic ${point.at_seconds}s: ${requests.successful} successful; ${requests.skipped_late + requests.skipped_concurrency} omitted; container ${(point.container_memory_bytes / mib).toFixed(0)} MiB; generator ${(point.generator_rss_bytes / mib).toFixed(0)} MiB`);
          lastProgress = point.at_seconds;
        }
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
      records.scheduled += scheduled;
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
        const window = windowAt(due);
        window.scheduled++;
        if (lag > safety.max_schedule_lag_ms) { requests.skipped_late++; window.skipped_late++; continue; }
        if (inflight.size >= config.concurrency) { requests.skipped_concurrency++; window.skipped_concurrency++; continue; }
        if (sent.size + 1 > safety.max_unique_events) { abortReason = "unique event safety limit"; break; }
        scheduleLag.push(lag);
        const requestIndex = requests.sent + inflight.size;
        const duplicate = config.duplicate_every > 0 && (ticket + 1) % config.duplicate_every === 0 && duplicatePool.length > 0;
        const record = duplicate ? duplicatePool[requestIndex % duplicatePool.length] : fresh(1)[0];
        const promise = send(record).catch((error) => { abortReason = `sender failed: ${error.message}`; });
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
    if (config.profile && config.shutdown === "stop" && settings["experimental.read_worker"] === "true") assert.ok(readerProfile, "separate reader profile must be available after graceful shutdown");
    const shutdownMs = performance.now() - stopStart;
    assert.equal(stopped.OOMKilled, false, "benchmark must stay within memory limits");
    assert.equal(stopped.ExitCode, config.shutdown === "kill" ? 137 : 0);
    const restartStart = performance.now();
    await server.start();
    const restartMs = performance.now() - restartStart;
    console.error(`Verifying ${acknowledged.size} acknowledged events after restart...`);
    const checked = await verify(server, sent, acknowledged, preloadIds, workload, async (count) => {
      const point = { verified_events: count, ...await sample(server) };
      verificationSamples.push(point);
      await onProgress({ phase: "verification", verified_events: count, sample: point });
      const reason = checkSafety(point);
      if (reason) throw new Error(`Verification safety stop: ${reason}`);
      if (count % 100000 === 0) console.error(`Verified ${count} events...`);
    });
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
      time_windows: windows.filter(Boolean).map(({ latency, queries, ...counts }) => ({ ...counts,
        successful_request_latency: distribution(latency), queries: Object.fromEntries(Object.entries(queries).map(([key, values]) => [key, distribution(values)])) })),
      verification_resources: verificationSamples,
      preload: { reused_fixture: reusedFixture, events: preloadIds.size, seconds: +preloadSeconds.toFixed(3), data_bytes: before.data_bytes },
      requests: { ...requests, sent_per_second: +(requests.sent / elapsed).toFixed(2),
        successful_per_second: +(requests.successful / elapsed).toFixed(2) },
      records: { ...records, newly_acknowledged: acknowledged.size - preloadIds.size,
        duplicate_acknowledgements: records.acknowledged - (acknowledged.size - preloadIds.size),
        newly_acknowledged_per_second: +((acknowledged.size - preloadIds.size) / elapsed).toFixed(2),
        persisted_new: checked.persisted_new, persisted_without_ack: checked.persisted_without_ack },
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
    await server.close();
  }
}
