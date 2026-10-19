// Isolated storage experiment. Run from the repository root after building the SDK
// and `cargo build --locked --example storage_comparison` in the collector crate.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { createWriteStream } from "node:fs";
import { mkdtemp, readFile, writeFile, rm, statfs } from "node:fs/promises";
import { tmpdir, cpus, platform, arch } from "node:os";
import { join, resolve } from "node:path";
import { captureWorkload } from "./docker/workload.mjs";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";

const count = Number(process.env.STORAGE_EVENTS ?? 200000);
const repeats = Number(process.env.STORAGE_REPEATS ?? 2);
assert.ok(Number.isSafeInteger(count) && count >= 100 && count <= 500000);
assert.ok(Number.isSafeInteger(repeats) && repeats >= 1 && repeats <= 3);
const binary = resolve(process.env.STORAGE_BINARY ?? "target/debug/examples/storage_comparison");
const selectedVariants = (process.env.STORAGE_VARIANTS ?? "current,typed,typed_context").split(",");
assert.ok(selectedVariants.every((v) => ["current", "typed", "typed_context", "production"].includes(v)));
const output = resolve(process.env.STORAGE_REPORT ?? "storage-comparison-2026-10-19.json");
const directory = await mkdtemp(join(tmpdir(), "chunkify-storage-comparison-"));
const disk = await statfs(directory);
assert.ok(disk.bavail * disk.bsize > 4 * 1024 ** 3, "Need at least 4 GiB free temporary disk");

function execute(args) {
  return new Promise((resolveRun, reject) => {
    const child = spawn(binary, args, { stdio: ["ignore", "pipe", "inherit"], timeout: 600000 });
    let stdout = "";
    child.stdout.on("data", (part) => { stdout += part; });
    child.once("error", reject);
    child.once("close", (code) => {
      if (code !== 0) return reject(new Error(`Benchmark exited ${code}`));
      try { resolveRun(JSON.parse(stdout)); } catch (error) { reject(error); }
    });
  });
}

const sourcePaths = ["tests/http/storage-comparison.mjs", "tests/http/docker/workload.mjs",
  "examples/storage_comparison.rs", "src/context.rs",
  "src/event.rs", "src/store.rs",
  "src/writer.rs", "src/storage.rs", "Cargo.lock"];
const report = { measured_at: new Date().toISOString(), host: { platform: platform(), architecture: arch(),
  cpu: cpus()[0]?.model, logical_cpus: cpus().length }, events: count, repeats,
  batch_size: 1000, source_sha256: {}, results: [], limitations: [
    "Local isolated storage benchmark, not HTTP ingestion capacity or production sizing.",
    "Shared host; fixed two DuckDB threads and 512 MiB buffer memory limit, not a process memory limit.",
    "SDK event templates replayed with synthetic context, identifiers and varying measurements; not production traffic.",
    "Same records, order, batches, primary/unique indexes and original OTLP retention in all variants.",
    "Experimental typed schemas are discovered from fixture scalar types; production uses the actual versioned storage module. Fresh database comparison, not migration disk reclamation.",
    "Timing includes file parsing and preparation; reads are sequential warm queries, not concurrent or cold-cache measurements.",
    "Every event, derived context, receipt time and OTLP object is compared after reopening; JSON numeric values compare by value.",
    "No retention churn, crash recovery, long-running views or archival behavior is tested.",
  ] };
try {
  for (const path of sourcePaths) report.source_sha256[path] = createHash("sha256").update(await readFile(path)).digest("hex");
  const workload = await captureWorkload();
  const next = workload.generator();
  report.workload = workload.description;
  report.workload.templates = [];
  const fixture = join(directory, "events.jsonl");
  const stream = createWriteStream(fixture);
  const hash = createHash("sha256");
  let sourceBytes = 0;
  // Capture real OTLP adapter serialization without an HTTP server per record.
  const originalFetch = globalThis.fetch;
  let wire;
  globalThis.fetch = async (_url, options) => { wire = JSON.parse(options.body); return new Response("{}", { status: 200 }); };
  const otlp = createOtlpAdapter("http://127.0.0.1/benchmark");
  try {
    for (let i = 0; i < count; i++) {
      const { event } = next();
      const view = Math.floor(i / workload.description.events_per_view);
      const position = i % workload.description.events_per_view;
      if (i < workload.description.events_per_view) report.workload.templates.push(structuredClone(event));
      const uuid = (key) => { const h = createHash("sha256").update(key).digest("hex"); return `${h.slice(0,8)}-${h.slice(8,12)}-${h.slice(12,16)}-${h.slice(16,20)}-${h.slice(20,32)}`; };
      event.event_id = uuid(`storage-event-${i}`);
      event.view_id = uuid(`storage-view-${view}`);
      const mobile = view % 3 === 0;
      event.occurred_at = new Date(Date.UTC(2026, 9, 1) + view * 37000 + position * 1100).toISOString();
      event.video_id = `video-${view % 100}`;
      event.viewer_id = view % 11 === 0 ? null : `viewer-${view % 10000}`;
      event.browser_context = { schema_version: 1,
        user_agent: mobile ? "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 Chrome/130.0.0.0 Mobile Safari/537.36" :
          "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36",
        browser_name: "Chrome", browser_version: `130.0.${view % 5}.0`, os_name: mobile ? "Android" : "macOS",
        os_version: mobile ? "14" : "14.6", device_type: mobile ? "mobile" : "desktop", device_family: mobile ? "Android" : null,
        language: view % 4 === 0 ? "en-US" : "fr-FR", language_prefix: view % 4 === 0 ? "en" : "fr",
        timezone: "Europe/Paris", timezone_offset_minutes: -120, screen_width_px: mobile ? 390 : 1920,
        screen_height_px: mobile ? 844 : 1080, viewport_width_px: mobile ? 390 : (position > 3 && view % 5 === 0 ? 1280 : 960),
        viewport_height_px: mobile ? 720 : 540, pixel_ratio: mobile ? 3 : 2 };
      // Vary recorded cumulative measurements so identical fixture timings do not
      // artificially make every metric constant across thousands of views.
      const factor = 1 + (view % 997) / 100;
      for (const key of ["playing_time_ms", "startup_time_ms", "rebuffer_duration_ms", "seek_duration_ms", "previous_state_duration_ms"])
        if (typeof event[key] === "number") event[key] *= factor;
      if (typeof event.rebuffer_duration_ms === "number" && position >= 2) {
        event.rebuffer_count = view % 4;
        event.rebuffer_duration_ms = event.rebuffer_count * (125 + view % 251);
        event.rebuffer_ratio = event.rebuffer_duration_ms / (event.playing_time_ms + event.rebuffer_duration_ms);
      }
      if (Array.isArray(event.rendition_playing_time)) event.rendition_playing_time = event.rendition_playing_time.map((r) => ({ ...r, playing_time_ms: r.playing_time_ms * factor }));
      // Valid optional/custom payloads exercise lossless fallback, explicit null,
      // empty/nested objects, Unicode, and mixed string/numeric error codes.
      if (view % 101 === 0) event.custom = { label: "écran", nested: [null, {}, { ok: true }], large_integer: 9007199254740991 };
      if (view % 37 === 0) event.browser_context.custom = { enabled: true };
      if (event.event_name === "chunkify.video.playback_error" && view % 2 === 0) event.error_code = "NETWORK_FAILURE";
      let original = null;
      if (i % 7 === 0) {
        await otlp.send(event);
        const scope = wire.resourceLogs[0].scopeLogs[0];
        original = { resource_logs: {}, scope_logs: { scope: scope.scope }, log_record: scope.logRecords[0] };
      }
      const octet = view % 250 + 1;
      const request_context = { peer_ip: "127.0.0.1", client_ip: `192.0.2.${position > 4 && view % 9 === 0 ? (octet % 250 + 1) : octet}`, client_ip_source: "x_forwarded_for" };
      const record = { event, otlp: original, request_context, received_at_ms: Date.parse(event.occurred_at) + 50 };
      const line = JSON.stringify(record) + "\n";
      hash.update(line); sourceBytes += Buffer.byteLength(line);
      if (!stream.write(line)) await once(stream, "drain");
    }
  } finally { globalThis.fetch = originalFetch; }
  stream.end(); await once(stream, "finish");
  report.fixture_sha256 = hash.digest("hex"); report.fixture_bytes = sourceBytes;
  for (const format of ["native", "mixed"]) {
    for (let repeat = 0; repeat < repeats; repeat++) {
      const variants = repeat % 2 === 0 ? selectedVariants : [...selectedVariants].reverse();
      for (const variant of variants) {
        const path = join(directory, `${format}-${repeat}-${variant}.duckdb`);
        console.error(`Storage comparison: ${format}, ${variant}, repeat ${repeat + 1}, ${count} events`);
        const result = await execute([fixture, path, variant, format, "1000"]);
        assert.equal(result.verified_events, count);
        assert.equal(result.otlp_records, format === "mixed" ? Math.ceil(count / 7) : 0);
        const reference = report.results[0];
        if (reference) {
          for (const query of ["summary", "qoe"]) for (const [key, expected] of Object.entries(reference.queries[query].result)) {
            const actual = result.queries[query].result[key];
            assert.ok(Math.abs(actual - expected) <= Math.max(1e-6, Math.abs(expected) * 1e-12), `${query}.${key} differs between variants`);
          }
        }
        report.results.push({ format, variant, repeat: repeat + 1, ...result });
        await writeFile(output, JSON.stringify(report, null, 2) + "\n");
        await rm(path);
        await rm(`${path}.wal`, { force: true });
      }
    }
  }
  console.log(JSON.stringify({ report: output, results: report.results.map(({ format, variant, repeat, bytes_per_event, insert_seconds, queries }) => ({ format, variant, repeat, bytes_per_event, insert_seconds, queries })) }, null, 2));
} finally { await rm(directory, { recursive: true, force: true }); }
