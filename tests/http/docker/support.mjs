import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { promisify } from "node:util";

const execute = promisify(execFile);
export const image = process.env.COLLECTOR_IMAGE ?? "chunkify-analytics-collector:test";

export async function docker(args) {
  const { stdout } = await execute("docker", args, { timeout: 60000, maxBuffer: 1024 * 1024 });
  return stdout.trim();
}

// Docker CLI and public HTTP interfaces only. Never open the database directly.
export async function containerCollector({ settings = {}, resources = [] } = {}) {
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
        ...Object.entries(settings).flatMap(([key, value]) => ["-e", `${key}=${value}`]),
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
    async stop() {
      await docker(["stop", "--time", "20", name]);
      const state = JSON.parse(await docker(["inspect", "--format", "{{json .State}}", name]));
      await docker(["rm", name]);
      name = undefined;
      return state;
    },
    async get(path) {
      const response = await fetch(`${server.url}${path}`, {
        headers: { Authorization: `Bearer ${server.keys.read_token}` }, signal: AbortSignal.timeout(10000),
      });
      if (!response.ok) throw new Error(`Read API returned ${response.status}: ${await response.text()}`);
      return response.json();
    },
    async close() {
      if (name) await docker(["rm", "-f", name]);
      await docker(["volume", "rm", volume]);
    },
  };
  await docker(["volume", "create", volume]);
  try {
    await docker(["run", "--rm", "--mount", `type=volume,src=${volume},dst=/data`, image, "init"]);
    // Local administrator access to the documented key file, never printed.
    server.keys = JSON.parse(await docker(["run", "--rm", "--mount", `type=volume,src=${volume},dst=/data,readonly`,
      "--entrypoint", "cat", image, "/data/keys.json"]));
    await server.start();
    return server;
  } catch (error) {
    await server.close();
    throw error;
  }
}
