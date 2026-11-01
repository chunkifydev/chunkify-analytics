import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { Agent, request as httpRequest } from "node:http";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { containerCollector, docker } from "./support.mjs";
import { captureWorkload } from "./workload.mjs";

const users = 100, projects = 10;
const concurrent = Number(process.env.CHUNKIFY_READ_LOAD_CONCURRENT ?? 32);
const perProject = Number(process.env.CHUNKIFY_READ_LOAD_PER_PROJECT ?? 16);
const distinct = process.env.CHUNKIFY_READ_LOAD_DISTINCT === "1";
assert.ok(Number.isInteger(concurrent) && concurrent >= 1 && concurrent <= 256);
assert.ok(Number.isInteger(perProject) && perProject >= 1 && perProject <= concurrent);
const cpus = Number(process.env.CHUNKIFY_READ_LOAD_CPUS ?? 1);
const memory = Number(process.env.CHUNKIFY_READ_LOAD_MEMORY_MIB ?? 2048);
assert.ok(Number.isInteger(cpus) && cpus >= 1 && cpus <= 8);
assert.ok(Number.isInteger(memory) && memory >= 2048 && memory <= 16384);
const agent = new Agent({ keepAlive: true, maxSockets: users });
const writeAgent = new Agent({ keepAlive: true, maxSockets: 128 });
const duration = Number(process.env.CHUNKIFY_READ_LOAD_SECONDS ?? 70);
assert.ok(Number.isInteger(duration) && duration >= 10 && duration <= 300);
const views = Number(process.env.CHUNKIFY_READ_LOAD_VIEWS ?? 100);
assert.ok(Number.isInteger(views) && views >= 1 && views <= 10000);
assert.ok(!distinct || views >= 10, "Distinct-query comparison needs ten seeded views per tenant");
const endpoint = process.env.CHUNKIFY_S3_TEST_ENDPOINT;
assert.ok(endpoint, "Set CHUNKIFY_S3_TEST_ENDPOINT to an endpoint reachable from Docker");
const admin = randomUUID();
const server = await containerCollector({
  resources: ["--platform=linux/arm64", `--cpus=${cpus}`, `--memory=${memory}m`, `--memory-swap=${memory}m`],
  environment: { CHUNKIFY_ADMIN_KEY: admin },
  settings: { profile: true, "logging.level": "warn", "duckdb.threads": 1,
    "server.ingest_per_second": 10000, "retention.seconds": 0,
    "storage.backend": "s3", "storage.url": `s3://${process.env.CHUNKIFY_S3_TEST_BUCKET ?? "chunkify-analytics-test"}/read-load-${randomUUID()}/`,
    "storage.endpoint": endpoint, "storage.region": "us-east-1", "storage.access_key_id": "test",
    "storage.secret_access_key": "test", "storage.use_ssl": false, "storage.flush_interval_seconds": 0,
    "cache.report_ttl_seconds": 60, "rate_limits.read_concurrent": concurrent, "rate_limits.read_concurrent_per_project": perProject },
});
let phase = "seed", stopped = false, stopWrites = false, writer;
const tenants = [server.keys];
const report = { users, projects, write_target_rps: 100, duration_seconds: duration, flush_interval_seconds: 0, views_per_project: views, seed_events: null, cpu: cpus, memory_mib: memory, architecture: "arm64", max_http_connections: users, max_write_connections: 128, max_outstanding_writes: 256, concurrent, per_project: perProject, rounds: [] };
function read(path, tenant, body, ingest = false) {
  return new Promise((resolve, reject) => {
    const encoded = body && JSON.stringify(body);
    const request = httpRequest(server.url + path, { agent: ingest ? writeAgent : agent, method: body ? "POST" : "GET", headers: {
      ...(ingest ? { "x-chunkify-ingest-key": tenant.public_key } : { authorization: `Bearer ${tenant.secret_key}` }), accept: "application/json, text/event-stream",
      ...(body && { "content-type": "application/json", "content-length": Buffer.byteLength(encoded), "mcp-protocol-version": "2025-11-25" }),
    } }, response => {
      const chunks = [];
      response.on("data", chunk => chunks.push(chunk));
      response.on("end", () => {
        clearTimeout(timer);
        if (response.statusCode !== (ingest ? 201 : 200)) { reject(new Error(`Request returned ${response.statusCode}`)); return; }
        try { resolve(JSON.parse(Buffer.concat(chunks))); } catch (error) { reject(error); }
      });
      response.on("error", reject);
    });
    const timer = setTimeout(() => request.destroy(new Error("Request timed out")), 15000);
    request.on("error", error => { clearTimeout(timer); reject(error); });
    request.end(encoded);
  });
}
const get = (path, tenant) => read(path, tenant);
try {
  for (let index = 1; index < projects; index++) {
    const response = await fetch(server.url + "/v1/projects", { method: "POST",
      headers: { authorization: `Bearer ${admin}`, "content-type": "application/json" }, body: JSON.stringify({ name: `Load tenant ${index}` }) });
    assert.equal(response.status, 201);
    tenants.push(await response.json());
  }
  const workload = await captureWorkload();
  const next = workload.generator();
  const records = views * workload.description.events_per_view;
  report.seed_events = records * projects;
  for (const tenant of tenants) {
    let submitted = 0;
    await Promise.all(Array.from({ length: 16 }, async () => {
      while (submitted++ < records) {
        const event = next().event;
        event.occurred_at = new Date(Date.parse(event.occurred_at) - 3600000).toISOString();
        const body = JSON.stringify(event);
        for (let attempt = 0; ; attempt++) {
          const response = await fetch(server.url + "/v1/events", { method: "POST",
            headers: { "content-type": "application/json", "x-chunkify-ingest-key": tenant.public_key }, body });
          await response.arrayBuffer();
          if (response.status === 503 && attempt < 20) { await delay(200); continue; }
          assert.equal(response.status, 201);
          break;
        }
      }
    }));
  }
  // Raw event reads bypass the report cache. Wait for all buffered files to publish.
  phase = "seed-publication";
  console.log(JSON.stringify({ phase, projects, views_per_project: views }));
  const deadline = Date.now() + 15000;
  for (const tenant of tenants) {
    while (true) {
      const result = await get("/v1/events?limit=1&order=desc", tenant);
      if (result.events.length && Date.parse(result.events[0].received_at) > 0) break;
      assert.ok(Date.now() < deadline, "Seed data did not publish");
      await delay(100);
    }
  }
  await delay(1500);
  function cohort() {
    const to = new Date(Math.floor(Date.now() / 60000) * 60000).toISOString();
    const from = new Date(Date.parse(to) - 7 * 86400000).toISOString();
    const bounds = new URLSearchParams({ from, to });
    return { from, to, bounds, paths: [`/v1/summary`, `/v1/engagement?${bounds}`, `/v1/quality?${bounds}`,
      `/v1/timeseries?${bounds}&interval=day`, `/v1/videos?${bounds}&sort_by=views&order=desc&limit=10`,
      `/v1/summary?${bounds}&group_by=browser_name`, `/v1/summary?${bounds}&group_by=country`] };
  }
  const mcp = async (tenant, id, { from, to }) => {
    const body = await read("/mcp", tenant, { jsonrpc: "2.0", id, method: "tools/call", params: { name: "get_metrics", arguments: { kind: "summary", from, to } } });
    assert.equal(body.error, undefined);
    assert.equal(body.result.isError ?? false, false);
  };
  const writeLatencies = [], writeErrors = [], startedViews = [];
  let offered = 0, skipped = 0, late = 0;
  const pending = new Set();
  const generators = tenants.map(() => workload.generator());
  const mixedStart = performance.now();
  const mixedDeadline = mixedStart + duration * 1000;
  writer = (async () => {
    while (!stopWrites && performance.now() < mixedDeadline) {
      const due = mixedStart + offered * 10;
      await delay(Math.max(0, due - performance.now()));
      if (stopWrites) break;
      if (performance.now() - due > 50) late++;
      const tenant = offered++ % tenants.length;
      if (pending.size >= 256) { skipped++; continue; }
      const event = generators[tenant]().event;
      const began = performance.now();
      let job;
      job = read("/v1/events", tenants[tenant], event, true).then(() => {
        writeLatencies.push(performance.now() - began);
        if (event.event_name === "chunkify.video.view_started") startedViews.push({ tenant, at: performance.now() });
      }, error => { writeErrors.push(error.message); }).finally(() => pending.delete(job));
      pending.add(job);
    }
    await Promise.all(pending);
  })();
  let iteration = 0;
  const cohorts = new Set();
  do {
    const round = iteration++ === 0 ? "cold" : "warm";
    phase = round;
    console.log(JSON.stringify({ phase, users }));
    const current = cohort();
    cohorts.add(current.to);
    const latencies = [];
    const started = performance.now();
    await Promise.all(Array.from({ length: users }, async (_, index) => {
      const tenant = tenants[index % tenants.length];
      const timed = async work => { const began = performance.now(); await work; latencies.push(performance.now() - began); };
      await Promise.all([...current.paths.map(path => timed(get(path, tenant))), timed(mcp(tenant, index + 1, current))]);
    }));
    latencies.sort((a, b) => a - b);
    const point = { round, requests: latencies.length, failures: 0, elapsed_ms: Math.round(performance.now() - started),
      cohort_to: current.to, p99_ms: Math.round(latencies[Math.ceil(latencies.length * .99) - 1]), max_ms: Math.round(latencies.at(-1)),
      p50_ms: Math.round(latencies[Math.ceil(latencies.length * .5) - 1]), p95_ms: Math.round(latencies[Math.ceil(latencies.length * .95) - 1]) };
    report.rounds.push(point);
    if (round === "cold") console.log(JSON.stringify(point));
    await delay(Math.min(1000, Math.max(0, mixedDeadline - performance.now())));
  } while (performance.now() < mixedDeadline);
  await writer;
  const warm = report.rounds.filter(r => r.round === "warm");
  console.log(JSON.stringify({ phase: "warm-summary", rounds: warm.length, max_p95_ms: Math.max(...warm.map(r => r.p95_ms)) }));
  writeLatencies.sort((a,b) => a-b);
  report.writes = { offered, completed: writeLatencies.length, failures: writeErrors.length, skipped, late_over_50ms: late,
    errors: [...new Set(writeErrors)], completed_rps: +(writeLatencies.length / duration).toFixed(1),
    p50_ms: Math.round(writeLatencies[Math.ceil(writeLatencies.length * .5) - 1]),
    p95_ms: Math.round(writeLatencies[Math.ceil(writeLatencies.length * .95) - 1]),
    p99_ms: Math.round(writeLatencies[Math.ceil(writeLatencies.length * .99) - 1]), max_ms: Math.round(writeLatencies.at(-1)) };
  console.log(JSON.stringify({ phase: "writes", ...report.writes }));
  assert.equal(writeErrors.length + skipped, 0, "mixed writes must succeed without retries or dropped schedules");
  assert.ok(writeLatencies.length >= duration * 95, "must sustain at least 95% of the 100 events/second offered load");
  for (let index = 0; index < tenants.length; index++) {
    const expected = views + startedViews.filter(v => v.tenant === index && v.at <= performance.now() - 60000).length;
    for (const path of ["/v1/summary", `/v1/summary?${cohort().bounds}`]) {
      const summary = await get(path, tenants[index]);
      assert.ok(summary.views >= expected, "cached and advancing dashboard cohorts must include writes published over a minute ago");
    }
  }
  report.dashboard_cohorts = cohorts.size;
  if (duration >= 60) assert.ok(cohorts.size >= 2, "must exercise advancing dashboard bounds");
  if (distinct) {
    const { bounds } = cohort();
    phase = "unique-queries";
    const started = performance.now();
    const results = await Promise.all(Array.from({ length: users }, async (_, index) => {
      const began = performance.now();
      try {
        const tenantIndex = index % projects;
        const viewer = (tenantIndex * views + Math.floor(index / projects)) % 1000;
        const value = await get(`/v1/quality?${bounds}&viewer_id=viewer-${viewer}`, tenants[tenantIndex]);
        assert.ok(value.views > 0, "Distinct queries must match seeded views");
        return { elapsed: performance.now() - began, failed: false };
      } catch (error) { return { elapsed: performance.now() - began, failed: true, error: error.message }; }
    }));
    const latencies = results.map(r => r.elapsed).sort((a,b) => a-b);
    const point = { round: phase, requests: users, failures: results.filter(r => r.failed).length,
      errors: [...new Set(results.filter(r => r.failed).map(r => r.error))], elapsed_ms: Math.round(performance.now() - started),
      p95_ms: Math.round(latencies[Math.ceil(users * .95) - 1]), max_ms: Math.round(latencies.at(-1)) };
    report.rounds.push(point);
    console.log(JSON.stringify(point));
    assert.equal(point.failures, 0, "distinct uncached queries must succeed");
  }
  const memory = await docker(["exec", server.name, "sh", "-c", "cat /sys/fs/cgroup/memory.current /sys/fs/cgroup/memory.peak /sys/fs/cgroup/memory.events"]);
  report.memory_current_bytes = Number(memory.split("\n")[0]);
  report.memory_peak_bytes = Number(memory.split("\n")[1]);
  assert.match(memory, /oom_kill 0/);
  assert.equal((await server.stop()).ExitCode, 0);
  stopped = true;
  const profiles = server.lastLogs.split("\n").filter(line => line.startsWith('{"type":"collector_profile"')).map(line => JSON.parse(line));
  report.report_queries = profiles.filter(p => p.worker === "reader").reduce((sum, p) => sum + Object.values(p.jobs).reduce((n, job) => n + job.execution.count, 0), 0);
  // Eight cached keys per tenant at most, plus the uncached seed-publication checks.
  assert.ok(report.report_queries <= projects * 10 * (Math.ceil(duration / 60) + 1) + (distinct ? users : 0), `Reports were not coalesced: ${report.report_queries}`);
  assert.ok(report.report_queries > 0);
  const destination = process.env.CHUNKIFY_LOAD_REPORT ?? "target/benchmarks/read-load.json";
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, JSON.stringify(report, null, 2) + "\n");
  console.log(JSON.stringify({ report: destination, report_queries: report.report_queries, memory_peak_mib: Math.ceil(report.memory_peak_bytes / 1048576) }));
} catch (error) {
  report.failure = { phase, error: error.message };
  const destination = process.env.CHUNKIFY_LOAD_REPORT ?? "target/benchmarks/read-load.json";
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, JSON.stringify(report, null, 2) + "\n");
  console.error(JSON.stringify(report.failure));
  throw error;
} finally {
  stopWrites = true;
  if (writer) await writer;
  if (!stopped) { await server.stop(); }
  await mkdir("target/benchmarks", { recursive: true });
  await writeFile("target/benchmarks/read-load-server.log", server.lastLogs ?? "");
  agent.destroy();
  writeAgent.destroy();
  await server.close();
}
