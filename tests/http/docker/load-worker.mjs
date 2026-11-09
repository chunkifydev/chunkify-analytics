import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { cpus, platform, arch, loadavg } from "node:os";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import { getHeapStatistics } from "node:v8";
import { captureWorkload, measure, safety, thresholds, hostFreeBytes } from "./benchmark.mjs";
import { docker, image } from "./support.mjs";
import { checkEngineBudget, containerLimits } from "./resources.mjs";

const execute = promisify(execFile);
const generator_runtime = { exec_args: process.execArgv, v8_heap_limit_bytes: getHeapStatistics().heap_size_limit };
if (generator_runtime.v8_heap_limit_bytes > 768 * 1024 * 1024) {
  throw new Error("Generator heap exceeds the 768 MiB budget. Use load.mjs and remove conflicting heap settings from NODE_OPTIONS.");
}
function integer(name, fallback, min, max) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`${name} must be ${min}..${max}`);
  return value;
}
const seconds = integer("CHUNKIFY_LOAD_SECONDS", 30, 5, 300);
const preload = integer("CHUNKIFY_LOAD_PRELOAD_EVENTS", 50000, 0, 100000);
const concurrency = integer("CHUNKIFY_LOAD_CONCURRENCY", 128, 1, 4096);
const queries = integer("CHUNKIFY_LOAD_QUERIES", 1, 0, 1) === 1;
const browserContext = integer("CHUNKIFY_LOAD_BROWSER_CONTEXT", 0, 0, 1) === 1;
const write_settings = {};
const writeFields = { BATCH_MAX_EVENTS: "batch.max_events", BATCH_MAX_BYTES: "batch.max_bytes",
  BATCH_DELAY_MS: "batch.delay_ms", QUEUE_MAX_BYTES: "batch.queue_max_bytes", QUEUE_MAX_REQUESTS: "batch.queue_max_requests" };
for (const [name, fallback, min, max] of [
  ["BATCH_MAX_EVENTS", 10000, 1, 10000], ["BATCH_MAX_BYTES", 10485760, 1, 67108864],
  ["BATCH_DELAY_MS", 10, 0, 1000], ["QUEUE_MAX_BYTES", 33554432, 1, 268435456],
  ["QUEUE_MAX_REQUESTS", 4096, 1, 4096],
]) {
  if (process.env[`CHUNKIFY_LOAD_${name}`] !== undefined) {
    write_settings[writeFields[name]] = String(integer(`CHUNKIFY_LOAD_${name}`, fallback, min, max));
  }
}
const cases = [
  ...[100, 200, 400, 500, 750, 800, 1000, 1600, 2000, 3000, 4000].map((rate) => ({ name: `native-${rate}`, rate })),
  ...[750, 1000, 2000, 3000].map((rate) => ({ name: `preloaded-native-${rate}`, rate, preload })),
  { name: "duplicates", duplicate_every: 5, rate: 100 },
  { name: "large", large_every: 4, duplicate_every: 5, rate: 50 },
  { name: "preloaded-duplicates", duplicate_every: 5, rate: 100, preload },
  { name: "burst-recovery", phases: [
    { name: "warm", rate: 100, seconds: seconds * 0.2 },
    { name: "burst", rate: 1600, seconds: seconds * 0.4 },
    { name: "recovery", rate: 100, seconds: seconds * 0.4 },
  ] },
  { name: "rate-cap", rate: 400, rate_limit: 100 },
];
const selected = process.env.CHUNKIFY_LOAD_CASES?.split(",");
for (const arg of process.argv.slice(2)) if (arg !== "--list") throw new Error(`Unknown argument: ${arg}. Use --list or configure CHUNKIFY_LOAD_* environment variables.`);
for (const name of selected ?? []) if (!cases.some((item) => item.name === name)) throw new Error(`Unknown benchmark case: ${name}`);
const plan = cases.filter((item) => !selected || selected.includes(item.name)).map((item) => ({ seconds, concurrency, queries, write_settings, ...item }));
if (process.argv.includes("--list")) {
  console.log(JSON.stringify({ cases: plan, thresholds, safety, generator_runtime, container_limits: containerLimits }, null, 2));
} else {
  const controller = new AbortController();
  const interrupt = () => { controller.abort(); console.error("Stopping traffic and cleaning up benchmark resources..."); };
  process.on("SIGINT", interrupt);
  process.on("SIGTERM", interrupt);
  const info = JSON.parse(await docker(["info", "--format", "{{json .}}"]));
  checkEngineBudget(info);
  const inspected = JSON.parse(await docker(["image", "inspect", "--format", "{{json .}}", image]));
  const git = async (...args) => (await execute("git", args)).stdout.trim();
  const runnerHash = createHash("sha256");
  for (const file of ["load.mjs", "load-worker.mjs", "benchmark.mjs", "workload.mjs", "support.mjs", "resources.mjs"]) {
    runnerHash.update(file).update(await readFile(new URL(file, import.meta.url)));
  }
  const collector_files_sha256 = {};
  for (const file of ["Cargo.toml", "Cargo.lock", "Dockerfile", "openapi.json",
    ...(await readdir("src", { recursive: true })).filter((file) => file.endsWith(".rs")).sort().map((file) => `src/${file}`)]) {
    collector_files_sha256[file] = createHash("sha256").update(await readFile(file)).digest("hex");
  }
  const report = {
    report_version: 7, measured_at: new Date().toISOString(), generator_runtime,
    source: { git_head: await git("rev-parse", "HEAD"),
      collector_source_commit: await git("log", "-1", "--format=%H", "--", "src"),
      collector_source_modified: Boolean(await git("status", "--porcelain", "--", "src", "Cargo.lock")),
      runner_sha256: runnerHash.digest("hex"), collector_files_sha256 },
    host: { os: platform(), architecture: arch(), cpu: cpus()[0]?.model, logical_cpus: cpus().length,
      node: process.version, load_average_at_start: loadavg(), free_disk_bytes_at_start: await hostFreeBytes() },
    engine: { version: info.ServerVersion, architecture: info.Architecture, cpus: info.NCPU,
      memory_bytes: info.MemTotal, os: info.OperatingSystem, storage_driver: info.Driver,
      other_running_containers_at_start: info.ContainersRunning },
    image: { id: inspected.Id, architecture: inspected.Architecture, size_bytes: inspected.Size },
    storage: "local Docker named volume; loopback HTTP; no TLS or artificial network delay",
    container_limits: containerLimits, thresholds, safety,
    limitations: [
      "Short probes identify candidate rates, not sustained or 30-day capacity.",
      "Shared host workloads may affect results; no other containers are stopped.",
      "SDK events and adapter wire output are recorded once, then replayed with new identities.",
      "Verification retains encoded replay descriptions, releases them page by page and accumulates per-view summaries; every complete payload is reconstructed and checked after restart.",
      "Request latency starts after serialization; preparation and schedule lag are reported separately.",
      "Resource sampling includes Docker exec overhead and may miss peaks. Container memory includes filesystem cache.",
      "Scheduled retention scans run every five seconds with a 30-day TTL; these runs do not measure deletion of expired data.",
      "Optional aggregate profiling covers the process lifetime, including preload, health checks and startup retention, but excludes schema initialization and final checkpoint. It is unavailable after SIGKILL. Queue depth and internal timing percentiles are not measured.",
      "Image ID identifies the executable. Git source provenance is the local checkout and cannot attest to a separately supplied image.",
    ],
    plan, results: [], status: "running",
  };
  const save = async () => {
    if (process.env.CHUNKIFY_LOAD_REPORT) await writeFile(process.env.CHUNKIFY_LOAD_REPORT, `${JSON.stringify(report, null, 2)}\n`);
  };
  const workload = await captureWorkload({ browserContext });
  report.workload = workload.description;
  await save();
  for (const config of plan) {
    console.error(`Measuring ${config.name} for ${config.seconds}s${config.preload ? ` after preloading ${config.preload} events` : ""}...`);
    try {
      const result = await measure(config, workload, { signal: controller.signal,
        async onProgress(update) { (report.progress ??= []).push({ case: config.name, ...update }); await save(); },
      });
      report.results.push(result);
      await save();
      console.error(`${result.requests.successful_per_second} successful requests/s; ${result.records.newly_acknowledged_per_second} new events/s; successful p95 ${result.successful_request_latency.p95_ms}ms; failures ${JSON.stringify(result.requests.failures)}; probe thresholds ${result.meets_probe_thresholds ? "met" : "not met"}; full-delivery thresholds ${result.meets_full_delivery_thresholds ? "met" : "not met"}`);
      if (!result.completed) {
        report.status = controller.signal.aborted ? "interrupted" : "safety_stopped";
        process.exitCode = controller.signal.aborted ? 130 : 1;
        break;
      }
    } catch (error) {
      report.status = controller.signal.aborted ? "interrupted" : "failed";
      report.failure = { case: config.name, message: error.message };
      process.exitCode = controller.signal.aborted ? 130 : 1;
      console.error(`${config.name} failed: ${error.message}`);
      break;
    }
  }
  if (report.status === "running") report.status = "completed";
  report.finished_at = new Date().toISOString();
  await save();
  process.off("SIGINT", interrupt);
  process.off("SIGTERM", interrupt);
  if (!process.env.CHUNKIFY_LOAD_REPORT) console.log(JSON.stringify(report, null, 2));
}
