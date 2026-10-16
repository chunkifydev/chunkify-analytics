import assert from "node:assert/strict";
import { cpus, platform, arch } from "node:os";
import { writeFile } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";
import { attachAnalytics } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";
import { containerCollector, docker, image } from "./support.mjs";

const seconds = Number(process.env.COLLECTOR_LOAD_SECONDS ?? 30);
if (!Number.isInteger(seconds) || seconds < 5 || seconds > 300) throw new Error("COLLECTOR_LOAD_SECONDS must be 5..300");
const concurrency = 8;
const resources = ["--cpus=2", "--memory=2g", "--memory-swap=2g"];

function playback(index) {
  const events = [];
  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const session = attachAnalytics(media, { videoId: `video-${index % 10}`, viewerId: `viewer-${index % 1000}`,
    destinations: [{ send(event) { events.push(event); } }] });
  session.startAttempt();
  session.destroy();
  return events;
}

function distribution(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const percentile = (p) => sorted.length ? +sorted[Math.ceil(sorted.length * p) - 1].toFixed(2) : null;
  return { count: sorted.length, p50_ms: percentile(0.5), p95_ms: percentile(0.95), p99_ms: percentile(0.99) };
}

async function readAll(server) {
  const records = new Map();
  let cursor;
  do {
    const page = await server.get(`/v1/events?limit=1000${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ""}`);
    for (const record of page.events) records.set(record.event.event_id, record.event);
    cursor = page.next_cursor;
  } while (cursor);
  return records;
}

async function measure(format, rate) {
  // Raise the rate cap to measure storage capacity, rather than the default 100/s policy.
  const server = await containerCollector({ resources, settings: { COLLECTOR_INGEST_PER_SECOND: "10000" } });
  try {
    const adapter = (format === "native" ? createHttpAdapter : createOtlpAdapter)(
      `${server.url}${format === "native" ? "/v1/events" : "/v1/logs"}`,
      { headers: { "X-Chunkify-Ingest-Key": server.keys.ingest_key } },
    );
    const accepted = new Map();
    const latency = [];
    const readLatency = [];
    const failures = {};
    let readFailures = 0;
    let peakRssKiB = 0;
    let samples = 0;
    let active = true;
    let issued = 0;
    let views = 0;
    let canonicalBytes = 0;
    let monitorError;
    const start = performance.now();
    const monitor = (async () => {
      while (active) {
        const before = performance.now();
        try {
          await server.get("/v1/summary");
          readLatency.push(performance.now() - before);
        } catch { readFailures++; }
        const status = await docker(["exec", server.name, "cat", "/proc/1/status"]);
        peakRssKiB = Math.max(peakRssKiB, Number(status.match(/^VmRSS:\s+(\d+) kB$/m)?.[1] ?? 0));
        samples++;
        await delay(1000);
      }
    })().catch((error) => { monitorError = error; active = false; });
    // Closed-loop clients, capped at eight concurrent requests. Paced runs share a
    // target schedule; overload reduces achieved rate instead of growing a queue.
    await Promise.all(Array.from({ length: concurrency }, async () => {
      let pending = [];
      while (active) {
        const ticket = issued++;
        const due = rate ? start + ticket * 1000 / rate : performance.now();
        if (due >= start + seconds * 1000) break;
        if (due > performance.now()) await delay(due - performance.now());
        if (performance.now() - start >= seconds * 1000) break;
        if (!pending.length) pending = playback(views++);
        const event = pending.shift();
        canonicalBytes += Buffer.byteLength(JSON.stringify(event));
        const before = performance.now();
        try {
          await adapter.send(event);
          accepted.set(event.event_id, event);
        } catch (error) {
          const reason = error.message.match(/HTTP (\d+)/)?.[1] ?? "transport";
          failures[reason] = (failures[reason] ?? 0) + 1;
        }
        latency.push(performance.now() - before);
      }
    }));
    const elapsed = (performance.now() - start) / 1000;
    active = false;
    await monitor;
    if (monitorError) throw monitorError;
    const stopped = await server.stop();
    assert.equal(stopped.ExitCode, 0);
    assert.equal(stopped.OOMKilled, false);
    const dataBytes = Number((await docker(["run", "--rm", "--mount", `type=volume,src=${server.volume},dst=/data,readonly`,
      "--entrypoint", "du", image, "-sb", "/data"])).split(/\s/)[0]);
    await server.start();
    const persisted = await readAll(server);
    for (const [id, event] of accepted) assert.deepEqual(persisted.get(id), event, "acknowledged event must survive replacement");
    // The response may be lost after a successful commit. Report any extra records separately.
    assert.ok(persisted.size <= latency.length);
    const summary = await server.get("/v1/summary");
    const starts = [...persisted.values()].filter((event) => event.event_name === "chunkify.video.view_started");
    assert.equal(summary.views, starts.length);
    assert.equal(summary.unique_viewers, new Set(starts.map((event) => event.viewer_id)).size);
    return {
      format, target_requests_per_second: rate ?? "unpaced", concurrency, requested_seconds: seconds,
      elapsed_seconds: +elapsed.toFixed(3), attempted: latency.length, accepted: accepted.size, failures,
      accepted_per_second: +(accepted.size / elapsed).toFixed(2), request_latency: distribution(latency),
      summary_latency: distribution(readLatency), summary_failures: readFailures,
      sampled_peak_process_rss_mib: +(peakRssKiB / 1024).toFixed(2), memory_samples: samples,
      mean_canonical_event_bytes: Math.round(canonicalBytes / latency.length), checkpointed_data_bytes: dataBytes,
      persisted_events: persisted.size, persisted_without_ack: persisted.size - accepted.size,
      summary, replacement_verified: true,
    };
  } finally { await server.close(); }
}

const report = {
  measured_at: new Date().toISOString(),
  host: { os: platform(), architecture: arch(), cpu: cpus()[0]?.model, logical_cpus: cpus().length, node: process.version },
  engine: JSON.parse(await docker(["info", "--format", '{{json .}}'])),
  image: JSON.parse(await docker(["image", "inspect", "--format", '{{json .}}', image])),
  container_limits: { cpus: 2, memory_mib: 2048, swap_mib: 0 },
  ingest_rate_limit: 10000, results: [],
};
// Keep the report free of machine paths, registry configuration, and environment values.
report.engine = { version: report.engine.ServerVersion, architecture: report.engine.Architecture,
  cpus: report.engine.NCPU, memory_bytes: report.engine.MemTotal, os: report.engine.OperatingSystem };
report.image = { id: report.image.Id, architecture: report.image.Architecture, size_bytes: report.image.Size };
for (const format of ["native", "otlp"]) {
  for (const rate of [50, 100, null]) {
    console.error(`Measuring ${format}, ${rate ?? "unpaced"} requests/s for ${seconds}s...`);
    const result = await measure(format, rate);
    report.results.push(result);
    if (process.env.COLLECTOR_LOAD_REPORT) {
      await writeFile(process.env.COLLECTOR_LOAD_REPORT, `${JSON.stringify(report, null, 2)}\n`);
    }
    console.error(`${result.accepted_per_second} accepted/s; p95 ${result.request_latency.p95_ms}ms; failures ${JSON.stringify(result.failures)}`);
  }
}
const output = `${JSON.stringify(report, null, 2)}\n`;
if (process.env.COLLECTOR_LOAD_REPORT) await writeFile(process.env.COLLECTOR_LOAD_REPORT, output);
else console.log(output);
