import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { cpus, platform, arch, loadavg } from "node:os";
import { readFile, readdir, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import { captureWorkload, measure, safety, thresholds, hostFreeBytes } from "./benchmark.mjs";
import { docker, image } from "./support.mjs";

const execute = promisify(execFile);
function integer(name, fallback, min, max) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`${name} must be ${min}..${max}`);
  return value;
}
const seconds = integer("COLLECTOR_LOAD_SECONDS", 30, 5, 300);
const preload = integer("COLLECTOR_LOAD_PRELOAD_EVENTS", 50000, 0, 100000);
const concurrency = integer("COLLECTOR_LOAD_CONCURRENCY", 128, 1, 4096);
const profile = integer("COLLECTOR_LOAD_PROFILE", 0, 0, 1) === 1;
const queries = integer("COLLECTOR_LOAD_QUERIES", 1, 0, 1) === 1;
const browserContext = integer("COLLECTOR_LOAD_BROWSER_CONTEXT", 0, 0, 1) === 1;
const write_settings = {};
for (const [name, fallback, min, max] of [
  ["BATCH_MAX_EVENTS", 100, 1, 10000], ["BATCH_MAX_BYTES", 1048576, 1, 67108864],
  ["BATCH_DELAY_MS", 10, 0, 1000], ["QUEUE_MAX_BYTES", 16777216, 1, 268435456],
  ["QUEUE_MAX_REQUESTS", 256, 1, 4096],
  ["CHECKPOINT_THRESHOLD_MIB", 16, 1, 1024],
]) {
  if (process.env[`COLLECTOR_LOAD_${name}`] !== undefined) {
    write_settings[`COLLECTOR_${name}`] = String(integer(`COLLECTOR_LOAD_${name}`, fallback, min, max));
  }
}
if (process.env.COLLECTOR_LOAD_DURABLE_QUEUE === "1") write_settings.COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE = "true";
const insertMode = process.env.COLLECTOR_LOAD_INSERT_MODE;
if (insertMode !== undefined) {
  if (!["row", "bulk"].includes(insertMode)) throw new Error("COLLECTOR_LOAD_INSERT_MODE must be row or bulk");
  write_settings.COLLECTOR_INSERT_MODE = insertMode;
}
const cases = [
  ...[100, 200, 400, 500, 750, 800, 1000, 1600, 2000, 3000, 4000].map((rate) => ({ name: `native-${rate}`, format: "native", rate })),
  ...[750, 1000, 2000, 3000].map((rate) => ({ name: `preloaded-native-${rate}`, format: "native", rate, preload })),
  ...[100, 200, 400, 800, 1000].map((rate) => ({ name: `otlp-${rate}`, format: "otlp", rate })),
  { name: "otlp-batch10", format: "otlp", batch_size: 10, rate: 100 },
  { name: "otlp-batch25", format: "otlp", batch_size: 25, rate: 100 },
  { name: "otlp-batch10-low", format: "otlp", batch_size: 10, rate: 25 },
  { name: "otlp-batch25-low", format: "otlp", batch_size: 25, rate: 10 },
  { name: "otlp-batch25-gzip", format: "otlp", batch_size: 25, gzip: true, rate: 100, shutdown: "kill" },
  { name: "mixed", format: "mixed", batch_size: 10, duplicate_every: 5, rate: 100 },
  { name: "mixed-large", format: "mixed", batch_size: 10, large_every: 4, gzip: true, duplicate_every: 5, rate: 50 },
  { name: "preloaded-mixed", format: "mixed", batch_size: 10, duplicate_every: 5, rate: 100, preload },
  { name: "burst-recovery", format: "native", phases: [
    { name: "warm", rate: 100, seconds: seconds * 0.2 },
    { name: "burst", rate: 1600, seconds: seconds * 0.4 },
    { name: "recovery", rate: 100, seconds: seconds * 0.4 },
  ] },
  { name: "rate-cap", format: "otlp", batch_size: 10, rate: 400, rate_limit: 100 },
];
const selected = process.env.COLLECTOR_LOAD_CASES?.split(",");
for (const arg of process.argv.slice(2)) if (arg !== "--list") throw new Error(`Unknown argument: ${arg}. Use --list or configure COLLECTOR_LOAD_* environment variables.`);
for (const name of selected ?? []) if (!cases.some((item) => item.name === name)) throw new Error(`Unknown benchmark case: ${name}`);
let plan = cases.filter((item) => !selected || selected.includes(item.name)).map((item) => ({ seconds, concurrency, profile, queries, write_settings, ...item }));
const compareQueue = integer("COLLECTOR_LOAD_COMPARE_QUEUE", 0, 0, 1) === 1;
if (compareQueue) {
  if (plan.length !== 1) throw new Error("Queue comparison requires exactly one selected workload");
  plan = [
    { ...plan[0], name: `${plan[0].name}-direct`, write_settings: { ...write_settings, COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE: "false" } },
    { ...plan[0], name: `${plan[0].name}-queued`, write_settings: { ...write_settings, COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE: "true" } },
  ];
}
if (process.argv.includes("--list")) {
  console.log(JSON.stringify({ cases: plan, thresholds, safety }, null, 2));
} else {
  const controller = new AbortController();
  const interrupt = () => { controller.abort(); console.error("Stopping traffic and cleaning up benchmark resources..."); };
  process.on("SIGINT", interrupt);
  process.on("SIGTERM", interrupt);
  const info = JSON.parse(await docker(["info", "--format", "{{json .}}"]));
  const inspected = JSON.parse(await docker(["image", "inspect", "--format", "{{json .}}", image]));
  const git = async (...args) => (await execute("git", args)).stdout.trim();
  const runnerHash = createHash("sha256");
  for (const file of ["load.mjs", "benchmark.mjs", "workload.mjs", "support.mjs"]) {
    runnerHash.update(file).update(await readFile(new URL(file, import.meta.url)));
  }
  const collector_files_sha256 = {};
  for (const file of ["Cargo.toml", "Cargo.lock", "Dockerfile",
    ...(await readdir("services/analytics-collector/src")).filter((file) => file.endsWith(".rs")).sort().map((file) => `src/${file}`)]) {
    collector_files_sha256[file] = createHash("sha256").update(await readFile(`services/analytics-collector/${file}`)).digest("hex");
  }
  const report = {
    report_version: 3, measured_at: new Date().toISOString(),
    source: { git_head: await git("rev-parse", "HEAD"),
      collector_source_commit: await git("log", "-1", "--format=%H", "--", "services/analytics-collector/src"),
      collector_source_modified: Boolean(await git("status", "--porcelain", "--", "services/analytics-collector/src", "services/analytics-collector/Cargo.lock")),
      runner_sha256: runnerHash.digest("hex"), collector_files_sha256 },
    host: { os: platform(), architecture: arch(), cpu: cpus()[0]?.model, logical_cpus: cpus().length,
      node: process.version, load_average_at_start: loadavg(), free_disk_bytes_at_start: await hostFreeBytes() },
    engine: { version: info.ServerVersion, architecture: info.Architecture, cpus: info.NCPU,
      memory_bytes: info.MemTotal, os: info.OperatingSystem, storage_driver: info.Driver,
      other_running_containers_at_start: info.ContainersRunning },
    image: { id: inspected.Id, architecture: inspected.Architecture, size_bytes: inspected.Size },
    storage: "local Docker named volume; loopback HTTP; no TLS or artificial network delay",
    container_limits: { cpus: 2, memory_mib: 2048, swap_mib: 0 }, thresholds, safety,
    limitations: [
      "Short probes identify candidate rates, not sustained or 30-day capacity.",
      "Shared host workloads may affect results; no other containers are stopped.",
      "SDK events and adapter wire output are recorded once, then replayed with new identities.",
      "Verification retains compact replay descriptions; complete expected payloads are reconstructed and compared after restart.",
      "OTLP batching and gzip model a gateway; SDK adapters still send one event per request.",
      "Request latency starts after serialization/compression; preparation and schedule lag are reported separately.",
      "Resource sampling includes Docker exec overhead and may miss peaks. Container memory includes filesystem cache.",
      "Scheduled retention scans run every five seconds with a 30-day TTL; these runs do not measure deletion of expired data.",
      "Optional aggregate profiling covers the process lifetime, including preload, health checks and startup retention, but excludes schema initialization and final checkpoint. It is unavailable after SIGKILL. Queue depth and internal timing percentiles are not measured.",
      "Image ID identifies the executable. Git source provenance is the local checkout and cannot attest to a separately supplied image.",
    ],
    plan, results: [], status: "running",
  };
  const save = async () => {
    if (process.env.COLLECTOR_LOAD_REPORT) await writeFile(process.env.COLLECTOR_LOAD_REPORT, `${JSON.stringify(report, null, 2)}\n`);
  };
  const workload = await captureWorkload({ browserContext });
  report.workload = workload.description;
  await save();
  const fixture = compareQueue ? {} : undefined;
  try {
  for (const config of plan) {
    console.error(`Measuring ${config.name} for ${config.seconds}s${config.preload ? ` after preloading ${config.preload} events` : ""}...`);
    try {
      const result = await measure(config, workload, { signal: controller.signal, fixture });
      report.results.push(result);
      if (compareQueue) global.gc?.();
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
  } finally { if (fixture?.volume) await docker(["volume", "rm", fixture.volume]); }
  if (report.status === "running") report.status = "completed";
  report.finished_at = new Date().toISOString();
  await save();
  process.off("SIGINT", interrupt);
  process.off("SIGTERM", interrupt);
  if (!process.env.COLLECTOR_LOAD_REPORT) console.log(JSON.stringify(report, null, 2));
}
