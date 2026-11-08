import { execFile, execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import { configSource } from "../configuration.mjs";

const execute = promisify(execFile);
export const image = process.env.CHUNKIFY_IMAGE ?? "chunkify-analytics:test";
export const clickhouseImage = process.env.CHUNKIFY_CLICKHOUSE_IMAGE ?? "clickhouse/clickhouse-server:25.8";

export async function docker(args) {
  const { stdout } = await execute("docker", args, { timeout: 60000, maxBuffer: 1024 * 1024 });
  return stdout.trim();
}

async function containerLogs(name) {
  const { stdout, stderr } = await execute("docker", ["logs", name], { timeout: 60000, maxBuffer: 1024 * 1024 });
  return `${stdout}\n${stderr}`;
}

// One ClickHouse container and private network per test process. Each analytics
// container uses its own database, so tests stay isolated.
let shared;
export function clickhouse() {
  shared ??= (async () => {
    const id = randomUUID();
    const network = `chunkify-test-${id}`;
    const name = `chunkify-clickhouse-test-${id}`;
    await docker(["network", "create", network]);
    await docker(["run", "-d", "--name", name, "--network", network, "--network-alias", "clickhouse",
      "--ulimit", "nofile=262144:262144", "--env", "CLICKHOUSE_SKIP_USER_SETUP=1", clickhouseImage]);
    process.once("exit", () => {
      try {
        execFileSync("docker", ["rm", "-f", name], { stdio: "ignore" });
        execFileSync("docker", ["network", "rm", network], { stdio: "ignore" });
      } catch {}
    });
    const deadline = Date.now() + 60000;
    while (true) {
      try {
        await docker(["exec", name, "clickhouse-client", "--query", "SELECT 1"]);
        break;
      } catch (error) {
        if (Date.now() > deadline) throw new Error(`ClickHouse did not start: ${await containerLogs(name)}`);
        await delay(500);
      }
    }
    return { name, network, url: "http://clickhouse:8123" };
  })();
  return shared;
}

// Run SQL inside the shared ClickHouse container.
export async function clickhouseQuery(query) {
  const server = await clickhouse();
  return docker(["exec", server.name, "clickhouse-client", "--query", query]);
}

async function copyDatabase(source, destination) {
  await clickhouseQuery(`CREATE DATABASE ${destination}`);
  for (const table of ["events", "registry"]) {
    await clickhouseQuery(`CREATE TABLE ${destination}.${table} AS ${source}.${table}`);
    await clickhouseQuery(`INSERT INTO ${destination}.${table} SELECT * FROM ${source}.${table}`);
  }
}

// Docker CLI and public HTTP interfaces only, except ClickHouse snapshots.
export async function containerCollector({ settings = {}, resources = [], environment = {}, seed } = {}) {
  const store = await clickhouse();
  const volume = `chunkify-collector-test-${randomUUID()}`;
  const database = `chunkify_${randomUUID().replaceAll("-", "_")}`;
  let name;
  const server = {
    volume,
    database,
    clickhouse: store,
    async start() {
      name = `${volume}-${randomUUID()}`;
      server.name = name;
      await docker(["run", "-d", "--name", name, "--read-only", "--tmpfs", "/tmp:rw,nosuid,noexec,size=64m",
        "--cap-drop=ALL", "--security-opt=no-new-privileges", "-p", "127.0.0.1::4174", "--network", store.network,
        "--mount", `type=volume,src=${volume},dst=/data,readonly`,
        "--env", `CHUNKIFY_CLICKHOUSE_URL=${store.url}`, "--env", `CHUNKIFY_CLICKHOUSE_DATABASE=${database}`,
        ...Object.entries(environment).flatMap(([key, value]) => ["--env", `${key}=${value}`]),
        ...resources, image]);
      const address = await docker(["port", name, "4174/tcp"]);
      server.url = `http://${address}`;
      const deadline = Date.now() + 45000;
      while (Date.now() < deadline) {
        const state = JSON.parse(await docker(["inspect", "--format", "{{json .State}}", name]));
        if (!state.Running) throw new Error(`Container exited: ${await docker(["logs", name])}`);
        if (state.Health?.Status === "healthy") return;
        await delay(250);
      }
      throw new Error(`Container did not become healthy: ${await docker(["logs", name])}`);
    },
    async stop(signal = "stop") {
      await docker(signal === "kill" ? ["kill", "--signal", "KILL", name] : ["stop", "--time", "20", name]);
      const state = JSON.parse(await docker(["inspect", "--format", "{{json .State}}", name]));
      server.lastLogs = await containerLogs(name);
      await docker(["rm", name]);
      name = undefined;
      return state;
    },
    // Copy this server's database, to start another server from the same data.
    async snapshot() {
      if (name) throw new Error("Stop collector before snapshot");
      const snapshot = `chunkify_${randomUUID().replaceAll("-", "_")}`;
      await copyDatabase(database, snapshot);
      return snapshot;
    },
    async command(...args) {
      return docker(["exec", name, "chunkify-analytics", ...args]);
    },
    async get(path) {
      const response = await fetch(`${server.url}${path}`, {
        headers: { Authorization: `Bearer ${server.keys.secret_key}` }, signal: AbortSignal.timeout(10000),
      });
      if (!response.ok) throw new Error(`Read API returned ${response.status}: ${await response.text()}`);
      return response.json();
    },
    async close({ preserveDatabase = false } = {}) {
      if (name) await docker(["rm", "-f", name]);
      await docker(["volume", "rm", volume]);
      if (!preserveDatabase) await clickhouseQuery(`DROP DATABASE IF EXISTS ${database}`);
    },
  };
  await docker(["volume", "create", volume]);
  try {
    if (seed) await copyDatabase(seed, database);
    // Servers poll the shared registry; keep CLI changes quick to apply in tests.
    const config = configSource({ "clickhouse.registry_refresh_seconds": "1", ...settings });
    await docker(["run", "--rm", "--user=0:0", "--network=none", "--mount", `type=volume,src=${volume},dst=/data`,
      "--entrypoint", "sh", image, "-c", 'umask 077; printf %s "$1" > /data/config.toml; chown 10001:10001 /data/config.toml',
      "sh", config]);
    const run = args => docker(["run", "--rm", "--network", store.network, "--mount", `type=volume,src=${volume},dst=/data,readonly`,
      "--env", `CHUNKIFY_CLICKHOUSE_URL=${store.url}`, "--env", `CHUNKIFY_CLICKHOUSE_DATABASE=${database}`, image, ...args]);
    if (!seed) server.keys = JSON.parse(await run(["init"]));
    else {
      server.keys = {};
      for (const kind of ["public", "secret"]) {
        server.keys[`${kind}_key`] = JSON.parse(await run(["key", "rotate-primary", kind])).key;
      }
    }
    await server.start();
    return server;
  } catch (error) {
    await server.close();
    throw error;
  }
}

// Wait for running servers to adopt a registry change made through the CLI.
export async function eventually(check, message) {
  const deadline = Date.now() + 15000;
  while (!(await check())) {
    if (Date.now() > deadline) throw new Error(message);
    await delay(200);
  }
}
