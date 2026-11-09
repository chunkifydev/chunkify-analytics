// Native ingestion and read capacity of analytics servers sharing one ClickHouse
// database. See docs/development.md#ingestion-and-read-capacity.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { Agent, request as httpRequest } from "node:http";
import { cpus, tmpdir, totalmem } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { clickhouseQuery, clickhouseUrl, collectorEnvironment, writeConfig } from "../configuration.mjs";
import { captureWorkload } from "../docker/workload.mjs";

function integer(name, fallback, min, max) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`${name} must be a whole number from ${min} to ${max}`);
  return value;
}

const servers = integer("CHUNKIFY_CAPACITY_SERVERS", 1, 1, 16);
const seconds = integer("CHUNKIFY_CAPACITY_SECONDS", 30, 5, 600);
const writers = integer("CHUNKIFY_CAPACITY_WRITERS", 512, 1, 4096);
const readers = integer("CHUNKIFY_CAPACITY_READERS", 64, 1, 1024);
const writeRate = integer("CHUNKIFY_CAPACITY_WRITE_RATE", 200, 1, 100000);
const views = integer("CHUNKIFY_CAPACITY_VIEWS", 1000, 1, 100000);
// Seeded views start this many hours back at most, spread evenly per view.
const seedHours = integer("CHUNKIFY_CAPACITY_SEED_HOURS", 1, 1, 24 * 6);
const projects = 10;
const maxOutstandingWrites = 256;
const binary = resolve(process.env.CHUNKIFY_BINARY ?? "target/release/chunkify-analytics");
const output = resolve(process.env.CHUNKIFY_CAPACITY_REPORT ?? "target/benchmarks/capacity.json");
const database = `chunkify_capacity_${randomUUID().replaceAll("-", "").slice(0, 12)}`;
const admin = randomUUID();
const agent = new Agent({ keepAlive: true, maxSockets: Math.max(writers, readers, maxOutstandingWrites) * 2 });
const directory = await mkdtemp(join(tmpdir(), "chunkify-capacity-"));

const baseSettings = {
  "logging.level": "warn", "server.bind": "127.0.0.1:0", "retention.seconds": "0",
  "server.ingest_per_second": "1000000",
  "rate_limits.read_per_minute": "1000000", "rate_limits.read_burst": "1000000",
  "rate_limits.read_concurrent": "32", "rate_limits.read_concurrent_per_project": "32",
  // Extra config.toml fields as JSON, for example {"clickhouse.async_insert": "false"}.
  ...JSON.parse(process.env.CHUNKIFY_CAPACITY_SETTINGS ?? "{}"),
  "clickhouse.database": database,
};

function run(args) {
  const child = spawn(binary, args, { cwd: directory, env: { ...collectorEnvironment(), CHUNKIFY_ADMIN_KEY: admin }, stdio: "pipe" });
  let stdout = "", stderr = "";
  child.stdout.on("data", chunk => { stdout += chunk; });
  child.stderr.on("data", chunk => { stderr += chunk; });
  return { child, output: () => ({ stdout, stderr }) };
}

// Servers are stateless: each phase starts its own set with that phase's cache setting.
async function startServers(settings) {
  await writeConfig(directory, { ...baseSettings, ...settings });
  const started = [];
  for (let index = 0; index < servers; index++) {
    const { child, output } = run(["serve"]);
    const url = await new Promise((ready, reject) => {
      const timeout = setTimeout(() => reject(new Error(`Server startup timed out: ${output().stderr}`)), 30000);
      child.once("exit", () => { clearTimeout(timeout); reject(new Error(`Server exited: ${output().stderr}`)); });
      child.stdout.on("data", () => {
        const match = output().stdout.match(/Listening on (http:\/\/\S+)/);
        if (match) { clearTimeout(timeout); ready(match[1]); }
      });
    });
    started.push({ child, url, closed: once(child, "close") });
  }
  return {
    urls: started.map(server => server.url),
    async stop() {
      for (const server of started) server.child.kill("SIGTERM");
      await Promise.all(started.map(server => server.closed));
    },
  };
}

function send(url, path, { body, headers = {} } = {}) {
  return new Promise((resolve) => {
    const began = performance.now();
    const encoded = body === undefined ? undefined : JSON.stringify(body);
    const request = httpRequest(url + path, { agent, method: encoded ? "POST" : "GET", headers: {
      ...headers, ...(encoded && { "content-type": "application/json", "content-length": Buffer.byteLength(encoded) }),
    } }, response => {
      response.on("data", () => {});
      response.on("end", () => { clearTimeout(timer); resolve({ status: response.statusCode, ms: performance.now() - began }); });
      response.on("error", error => { clearTimeout(timer); resolve({ status: error.message, ms: performance.now() - began }); });
    });
    const timer = setTimeout(() => request.destroy(new Error("timeout")), 30000);
    request.on("error", error => { clearTimeout(timer); resolve({ status: error.message, ms: performance.now() - began }); });
    request.end(encoded);
  });
}

class Recorder {
  constructor(success) { this.success = success; this.latencies = []; this.failures = {}; this.offered = 0; }
  record({ status, ms }) {
    if (status === this.success) this.latencies.push(ms);
    else this.failures[status] = (this.failures[status] ?? 0) + 1;
  }
  summary(elapsed, scheduled = elapsed) {
    const sorted = this.latencies.sort((a, b) => a - b);
    const at = q => sorted.length ? +sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))].toFixed(1) : null;
    const failed = Object.values(this.failures).reduce((sum, count) => sum + count, 0);
    return { offered: this.offered, completed: sorted.length, failed,
      offered_per_second: +(this.offered / scheduled).toFixed(1), completed_per_second: +(sorted.length / elapsed).toFixed(1),
      failures: this.failures, latency_ms: { p50: at(0.5), p95: at(0.95), p99: at(0.99), max: at(1) } };
  }
}

const reportPaths = bounds => [`/v1/summary?${bounds}`, `/v1/engagement?${bounds}`, `/v1/quality?${bounds}`,
  `/v1/timeseries?${bounds}&interval=day`, `/v1/videos?${bounds}&sort_by=views&order=desc&limit=10`,
  `/v1/summary?${bounds}&group_by=browser_name`, `/v1/summary?${bounds}&group_by=country`];

// Dashboard reads over the last seven days. `distinct` shifts the start by a
// random number of seconds so no two requests share a query or cache entry.
function readPath(distinct) {
  const to = Math.floor(Date.now() / 60000) * 60000;
  const from = to - 7 * 86400000 - (distinct ? Math.floor(Math.random() * 86400) * 1000 : 0);
  const paths = reportPaths(new URLSearchParams({ from: new Date(from).toISOString(), to: new Date(to).toISOString() }));
  return paths[Math.floor(Math.random() * paths.length)];
}

// Closed loop: each client sends its next request when the previous one ends.
async function saturate(clients, deadline, recorder, next) {
  let counter = 0;
  await Promise.all(Array.from({ length: clients }, async () => {
    while (performance.now() < deadline) {
      recorder.offered++;
      recorder.record(await next(counter++));
    }
  }));
}

// Open loop: requests are offered on schedule; a full outstanding budget counts as a failure.
async function offer(rate, deadline, recorder, next) {
  const start = performance.now();
  const pending = new Set();
  for (let index = 0; ; index++) {
    const due = start + index * 1000 / rate;
    if (due >= deadline) break;
    const wait = due - performance.now();
    if (wait > 1) await delay(wait);
    recorder.offered++;
    if (pending.size >= maxOutstandingWrites) { recorder.record({ status: "client_backlog", ms: 0 }); continue; }
    const job = next(index).then(result => recorder.record(result)).finally(() => pending.delete(job));
    pending.add(job);
  }
  await Promise.all(pending);
}

const report = {
  started_at: new Date().toISOString(), storage: "clickhouse", clickhouse_url: clickhouseUrl,
  machine: { cpus: cpus().length, cpu_model: cpus()[0]?.model, memory_gib: +(totalmem() / 2 ** 30).toFixed(1),
    note: "Load generator, analytics servers and ClickHouse share this machine unless the ClickHouse URL is remote." },
  settings: { servers, overrides: JSON.parse(process.env.CHUNKIFY_CAPACITY_SETTINGS ?? "{}"), seconds_per_phase: seconds, writers, readers, offered_write_rate: writeRate,
    projects, views_per_project: views, seed_hours: seedHours, max_outstanding_writes: maxOutstandingWrites, one_event_per_request: true },
  phases: {},
};

let active;
try {
  report.clickhouse_version = (await clickhouseQuery("SELECT version()")).trim();
  await writeConfig(directory, baseSettings);
  const init = run(["init"]);
  const [code] = await once(init.child, "close");
  assert.equal(code, 0, init.output().stderr);
  const tenants = [JSON.parse(init.output().stdout)];

  active = await startServers({ "cache.report_ttl_seconds": "0" });
  for (let index = 1; index < projects; index++) {
    const response = await fetch(`${active.urls[0]}/v1/projects`, { method: "POST",
      headers: { authorization: `Bearer ${admin}`, "content-type": "application/json" },
      body: JSON.stringify({ name: `Capacity tenant ${index}` }) });
    assert.equal(response.status, 201, await response.clone().text());
    tenants.push(await response.json());
  }
  const workload = await captureWorkload();
  report.workload = workload.description;
  const generators = tenants.map(() => workload.generator());
  // Every event of one seeded view moves back by the same whole number of hours.
  const seedShift = view => 3600000 * (1 + createHash("sha256").update(view).digest().readUInt32BE(0) % seedHours);
  const write = (urls, index, seeding = false) => {
    const tenant = index % projects;
    const event = generators[tenant]().event;
    if (seeding) event.occurred_at = new Date(Date.parse(event.occurred_at) - seedShift(event.view_id)).toISOString();
    return send(urls[index % urls.length], "/v1/events", { body: event, headers: { "x-chunkify-ingest-key": tenants[tenant].public_key } });
  };
  const read = (urls, index, distinct) => {
    const tenant = Math.floor(Math.random() * projects);
    return send(urls[index % urls.length], readPath(distinct), { headers: { authorization: `Bearer ${tenants[tenant].secret_key}` } });
  };

  // Seed in the past hours: a fixed dataset for the read phases.
  const seedEvents = views * workload.description.events_per_view * projects;
  console.error(`Seeding ${seedEvents} events...`);
  const seed = new Recorder(201);
  let seeded = 0;
  const seedStart = performance.now();
  await Promise.all(Array.from({ length: writers }, async () => {
    while (seeded < seedEvents) {
      const index = seeded++;
      seed.offered++;
      seed.record(await write(active.urls, index, true));
    }
  }));
  report.phases.seed = { events: seedEvents, ...seed.summary((performance.now() - seedStart) / 1000) };
  assert.equal(report.phases.seed.failed, 0, `Seed failures: ${JSON.stringify(seed.failures)}`);

  async function phase(name, description, body) {
    console.error(`${name}: ${description} for ${seconds}s...`);
    // Untimed warm-up opens client connections, so connection setup is not measured.
    await body(performance.now() + 2000);
    const start = performance.now();
    const deadline = start + seconds * 1000;
    const result = await body(deadline);
    const elapsed = (performance.now() - start) / 1000;
    report.phases[name] = { description, seconds: +elapsed.toFixed(1),
      ...Object.fromEntries(Object.entries(result).map(([key, recorder]) => [key, recorder.summary(elapsed, seconds)])) };
    console.error(JSON.stringify(report.phases[name]));
  }

  await phase("ingestion", `${writers} closed-loop writers, one event per request`, async deadline => {
    const writes = new Recorder(201);
    await saturate(writers, deadline, writes, index => write(active.urls, index));
    return { writes };
  });

  await phase("cold_reads_with_ingestion", `${readers} readers, distinct uncached reports, ${writeRate} events/s offered`, async deadline => {
    const writes = new Recorder(201), reads = new Recorder(200);
    await Promise.all([offer(writeRate, deadline, writes, index => write(active.urls, index)),
      saturate(readers, deadline, reads, index => read(active.urls, index, true))]);
    return { reads, writes };
  });
  await active.stop();

  active = await startServers({ "cache.report_ttl_seconds": "60" });
  await phase("cached_reads_with_ingestion", `${readers} readers, 60s report cache, ${writeRate} events/s offered`, async deadline => {
    const writes = new Recorder(201), reads = new Recorder(200);
    await Promise.all([offer(writeRate, deadline, writes, index => write(active.urls, index)),
      saturate(readers, deadline, reads, index => read(active.urls, index, false))]);
    return { reads, writes };
  });
  await active.stop();

  active = await startServers({ "cache.report_ttl_seconds": "0" });
  await phase("mixed_saturation", `${writers} closed-loop writers and ${readers} readers of distinct uncached reports`, async deadline => {
    const writes = new Recorder(201), reads = new Recorder(200);
    await Promise.all([saturate(writers, deadline, writes, index => write(active.urls, index)),
      saturate(readers, deadline, reads, index => read(active.urls, index, true))]);
    return { reads, writes };
  });
  report.stored_events = Number((await clickhouseQuery(`SELECT count() FROM ${database}.events`)).trim());
  report.finished_at = new Date().toISOString();
} finally {
  await active?.stop();
  agent.destroy();
  await clickhouseQuery(`DROP DATABASE IF EXISTS ${database}`).catch(() => {});
  await rm(directory, { recursive: true, force: true });
}

await mkdir(resolve(output, ".."), { recursive: true });
await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
console.error(`Report written to ${output}`);
const row = (name, key) => {
  const value = report.phases[name]?.[key];
  return value ? `${name.padEnd(28)} ${key.padEnd(6)} ${String(value.completed_per_second).padStart(9)}/s  offered ${String(value.offered_per_second).padStart(9)}/s  failed ${String(value.failed).padStart(6)}  p50 ${value.latency_ms.p50}ms  p95 ${value.latency_ms.p95}ms  p99 ${value.latency_ms.p99}ms` : null;
};
console.log([row("seed", "writes") ?? `${"seed".padEnd(28)} writes ${report.phases.seed.completed_per_second}/s`,
  ...["ingestion", "cold_reads_with_ingestion", "cached_reads_with_ingestion", "mixed_saturation"]
    .flatMap(name => [row(name, "writes"), row(name, "reads")]).filter(Boolean)].join("\n"));
