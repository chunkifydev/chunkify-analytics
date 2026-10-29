import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";
import { configSource } from "../configuration.mjs";

const execute = promisify(execFile);
export const image = process.env.CHUNKIFY_IMAGE ?? "chunkify-analytics:test";

export async function docker(args) {
  const { stdout } = await execute("docker", args, { timeout: 60000, maxBuffer: 1024 * 1024 });
  return stdout.trim();
}

async function containerLogs(name) {
  const { stdout, stderr } = await execute("docker", ["logs", name], { timeout: 60000, maxBuffer: 1024 * 1024 });
  return `${stdout}\n${stderr}`;
}

// Docker CLI and public HTTP interfaces only. Never open the database directly.
export async function containerCollector({ settings = {}, resources = [], environment = {}, seed } = {}) {
  const volume = `chunkify-collector-test-${randomUUID()}`;
  let name;
  const server = {
    volume,
    async start() {
      name = `${volume}-${randomUUID()}`;
      server.name = name;
      await docker(["run", "-d", "--name", name, "--read-only", "--tmpfs", "/tmp:rw,nosuid,noexec,size=64m",
        "--cap-drop=ALL", "--security-opt=no-new-privileges", "-p", "127.0.0.1::4174",
        "--mount", `type=volume,src=${volume},dst=/data`,
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
    async snapshot() {
      if (name) throw new Error("Stop collector before snapshot");
      const snapshot = `chunkify-collector-test-${randomUUID()}`;
      await docker(["volume", "create", snapshot]);
      try { await copyVolume(volume, snapshot); return snapshot; }
      catch (error) { await docker(["volume", "rm", snapshot]); throw error; }
    },
    async get(path) {
      const response = await fetch(`${server.url}${path}`, {
        headers: { Authorization: `Bearer ${server.keys.secret_key}` }, signal: AbortSignal.timeout(10000),
      });
      if (!response.ok) throw new Error(`Read API returned ${response.status}: ${await response.text()}`);
      return response.json();
    },
    async close({ preserveVolume = false } = {}) {
      if (name) await docker(["rm", "-f", name]);
      if (!preserveVolume) await docker(["volume", "rm", volume]);
    },
  };
  await docker(["volume", "create", volume]);
  try {
    if (seed) await copyVolume(seed, volume);
    await docker(["run", "--rm", "--user=0:0", "--network=none", "--mount", `type=volume,src=${volume},dst=/data`,
      "--entrypoint", "sh", image, "-c", 'umask 077; printf %s "$1" > /data/config.toml; chown 10001:10001 /data/config.toml',
      "sh", configSource(settings)]);
    const run = args => docker(["run", "--rm", "--mount", `type=volume,src=${volume},dst=/data`, image, ...args]);
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

async function copyVolume(source, destination) {
  await docker(["run", "--rm", "--user=0:0", "--network=none", "--mount", `type=volume,src=${source},dst=/source,readonly`,
    "--mount", `type=volume,src=${destination},dst=/data`, "--entrypoint", "sh", image,
    "-c", "cp -a /source/. /data/ && chown 10001:10001 /data"]);
}
