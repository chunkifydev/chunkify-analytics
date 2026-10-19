// Bounded writer-batch comparison through the collector HTTP API, not a capacity claim.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { mkdtemp, readFile, writeFile, cp, rm } from "node:fs/promises";
import { join, resolve } from "node:path";
import { tmpdir, cpus, platform, arch } from "node:os";
import { createHash } from "node:crypto";
import { captureWorkload, otlpBody, originalOtlp } from "./docker/workload.mjs";

const binary = resolve(process.env.COLLECTOR_BINARY ?? "target/debug/chunkify-analytics-collector");
const preload = Number(process.env.COLLECTOR_BATCH_PROBE_PRELOAD ?? 10000);
const count = Number(process.env.COLLECTOR_BATCH_PROBE_EVENTS ?? 10000);
const reportPath = resolve(process.env.COLLECTOR_BATCH_PROBE_REPORT ?? join(tmpdir(), "chunkify-writer-batching.json"));
let root;
const inheritedEnv = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("COLLECTOR_")));
const baseEnv = { ...inheritedEnv, COLLECTOR_BIND: "127.0.0.1:0", COLLECTOR_PROFILE: "true",
  COLLECTOR_MAX_BODY_BYTES: "16777216", COLLECTOR_MAX_OTLP_RECORDS: "1000",
  COLLECTOR_BATCH_MAX_BYTES: "16777216", COLLECTOR_QUEUE_MAX_BYTES: "67108864",
  COLLECTOR_INSERT_MODE: "bulk", COLLECTOR_BATCH_DELAY_MS: "10", COLLECTOR_INGEST_PER_SECOND: "10000",
  COLLECTOR_RETENTION_SECONDS: "0", COLLECTOR_CHECKPOINT_THRESHOLD_MIB: "64",
  COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE: "false", COLLECTOR_EXPERIMENTAL_READ_WORKER: "false",
  COLLECTOR_STORE_IP: "true", COLLECTOR_TRUSTED_PROXIES: "" };
let live;
let interrupted = false;
function interrupt() {
  interrupted = true;
  live?.child.kill("SIGKILL");
}
process.once("SIGINT", interrupt);
process.once("SIGTERM", interrupt);
const report = { status: "running", measured_at: new Date().toISOString(), binary_sha256: null,
  host: { os: platform(), architecture: arch(), cpu: cpus()[0]?.model, logical_cpus: cpus().length, node: process.version },
  build_profile: process.env.COLLECTOR_BATCH_PROBE_BUILD_PROFILE ?? "unspecified",
  preload, events: count, request_events: 100, concurrent_requests: 10, results: [],
  limitations: ["Native binary on the local host; no CPU/RAM container limits. Record the build profile separately.",
    "OTLP batches in direct mode, no concurrent reads. One enriched SDK view-start template with unique identities; not the full playback mix or production requests/s.",
    "Same preloaded database per run. Every measured event, envelope and browser context is verified after graceful restart; no crash, overload or retention-expiry test."] };

async function start(directory, batch) {
  assert.ok(!interrupted, "Probe interrupted");
  const env = { ...baseEnv, COLLECTOR_DATA_DIR: directory, COLLECTOR_BATCH_MAX_EVENTS: String(batch) };
  const keys = JSON.parse(await readFile(join(directory, "keys.json")));
  const child = spawn(binary, ["serve"], { env, stdio: ["ignore", "pipe", "pipe"] });
  const closed = new Promise((resolve) => {
    child.once("error", (error) => resolve({ error }));
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  live = { child, closed };
  let logs = "", output = "";
  child.stderr.on("data", p => { logs += p; });
  const url = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { child.kill("SIGKILL"); reject(Error("Startup timeout")); }, 15000);
    child.once("error", (error) => { clearTimeout(timer); reject(error); });
    child.once("exit", () => { clearTimeout(timer); reject(Error(logs)); });
    child.stdout.on("data", p => { output += p; const match = output.match(/Listening on (http:\/\/\S+)/);
      if (match) { clearTimeout(timer); resolve(match[1]); } });
  });
  return { async post(body) {
      assert.ok(!interrupted, "Probe interrupted");
      const response = await fetch(url + "/v1/logs", { method: "POST", body,
        headers: { "Content-Type": "application/json", "X-Chunkify-Ingest-Key": keys.ingest_key }, signal: AbortSignal.timeout(30000) });
      assert.equal(response.status, 200, await response.clone().text());
      assert.deepEqual(await response.json(), {});
    }, async get(path) {
      const response = await fetch(url + path, { headers: { Authorization: "Bearer " + keys.read_token }, signal: AbortSignal.timeout(30000) });
      assert.equal(response.status, 200); return response.json();
    }, async stop() {
      child.kill("SIGTERM");
      const timeout = setTimeout(() => child.kill("SIGKILL"), 30000);
      const { code, error } = await closed;
      clearTimeout(timeout);
      live = undefined;
      assert.ifError(error);
      assert.equal(code, 0, logs);
      const profile = logs.split("\n").filter(x => x.startsWith('{"type":"collector_profile"'))
        .map(JSON.parse).find(x => x.worker === "writer");
      assert.ok(profile, "Missing writer profile");
      return profile;
    } };
}

try {
  await writeFile(reportPath, JSON.stringify(report, null, 2) + "\n");
  assert.ok(Number.isSafeInteger(preload) && preload >= 1000 && preload <= 200000 && preload % 1000 === 0,
    "Preload must be a multiple of 1000 between 1000 and 200000");
  assert.ok(Number.isSafeInteger(count) && count >= 1000 && count <= 20000 && count % 1000 === 0,
    "Events must be a multiple of 1000 between 1000 and 20000");
  report.binary_sha256 = createHash("sha256").update(await readFile(binary)).digest("hex");
  root = await mkdtemp(join(tmpdir(), "chunkify-batch-probe-"));
  const workload = await captureWorkload({ browserContext: true });
  const template = workload.generator()();
  // Replay one SDK view-start event with deterministic unique identities.
  function record(i, phase) {
    const id = `${phase === "seed" ? "11111111" : "22222222"}-0000-4000-8000-${String(i).padStart(12, "0")}`;
    const fields = { event_id: id, view_id: id, video_id: phase, viewer_id: `viewer-${i % 1000}`,
      occurred_at: report.measured_at };
    return workload.replay({ ...template.replay, fields });
  }
  const seed = join(root, "seed");
  execFileSync(binary, ["init"], { env: { ...baseEnv, COLLECTOR_DATA_DIR: seed }, stdio: "ignore", timeout: 10000 });
  let server = await start(seed, 1000);
  console.log("Preloading", preload, "enriched SDK events");
  for (let start = 0; start < preload; start += 1000) {
    const started = performance.now();
    await server.post(JSON.stringify(otlpBody(Array.from({ length: 1000 }, (_, j) => record(start + j, "seed")))));
    if ((start + 1000) % Math.min(preload, 50000) === 0)
      console.log("Preloaded", start + 1000, "last request", Math.round(performance.now() - started), "ms");
  }
  await server.stop();
  const bodies = Array.from({ length: count / 100 }, (_, k) => JSON.stringify(otlpBody(Array.from({ length: 100 }, (_, j) => record(k * 100 + j, "probe")))));
  for (const [index, batch] of [100, 1000, 1000, 100].entries()) {
    const directory = join(root, `case-${index}`);
    await cp(seed, directory, { recursive: true });
    server = await start(directory, batch);
    let next = 0;
    const timingStarted = performance.now();
    await Promise.all(Array.from({ length: 10 }, async () => {
      while (next < bodies.length) { const current = next++; await server.post(bodies[current]); }
    }));
    const elapsedMs = performance.now() - timingStarted;
    const profile = await server.stop();
    assert.equal(profile.ingest.received_records, count);
    assert.equal(profile.ingest.inserted_records, count);
    server = await start(directory, batch);
    let cursor, verified = 0;
    const seen = new Set();
    do {
      const page = await server.get("/v1/events?video_id=probe&limit=1000" + (cursor ? "&cursor=" + encodeURIComponent(cursor) : ""));
      for (const row of page.events) {
        const i = Number(row.event.event_id.slice(-12));
        assert.ok(Number.isSafeInteger(i) && i >= 0 && i < count && !seen.has(i));
        // OTLP restores schema nulls; the native SDK event already contains them.
        const expected = record(i, "probe");
        assert.deepEqual(row.event, expected.event);
        assert.deepEqual(row.otlp, originalOtlp(expected));
        for (const [key, value] of Object.entries(expected.event.browser_context))
          assert.deepEqual(row.event_context[key], value);
        assert.equal(row.event_context.client_ip, "127.0.0.1");
        seen.add(i); verified++;
      }
      cursor = page.next_cursor;
    } while (cursor);
    assert.equal(verified, count);
    await server.stop();
    const result = { run: index + 1, batch_max_events: batch, elapsed_ms: elapsedMs,
      events_per_second: count * 1000 / elapsedMs, verified_events: verified, profile };
    report.results.push(result);
    await writeFile(reportPath, JSON.stringify(report, null, 2) + "\n");
    console.log(JSON.stringify({ run: result.run, batch, elapsed_ms: elapsedMs,
      transactions: profile.ingest.transactions, writer_ms: profile.jobs.ingest.execution.total_ms,
      insert_ms: profile.ingest_stages.insert_event.total_ms, commit_ms: profile.ingest_stages.commit.total_ms, verified }));
    await rm(directory, { recursive: true });
  }
  report.status = "completed";
  await writeFile(reportPath, JSON.stringify(report, null, 2) + "\n");
  console.log("Report:", reportPath);
} catch (error) {
  report.status = "failed";
  report.error = error.message;
  await writeFile(reportPath, JSON.stringify(report, null, 2) + "\n");
  throw error;
} finally {
  if (live) { live.child.kill("SIGKILL"); await live.closed; }
  process.off("SIGINT", interrupt);
  process.off("SIGTERM", interrupt);
  if (root) await rm(root, { recursive: true, force: true });
}
