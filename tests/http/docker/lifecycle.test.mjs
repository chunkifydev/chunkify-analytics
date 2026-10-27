import assert from "node:assert/strict";
import test from "node:test";
import { randomUUID } from "node:crypto";
import { isIP } from "node:net";
import { attachAnalytics } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";
import { containerCollector, docker } from "./support.mjs";

test("the admin environment key provisions a project that survives container replacement", { timeout: 120000 }, async t => {
  const adminKey = `admin-${randomUUID()}`;
  const server = await containerCollector({ environment: { CH_ADMIN_KEY: adminKey } });
  t.after(() => server.close());
  const response = await fetch(`${server.url}/v1/projects`, { method: "POST", headers: {
    authorization: `Bearer ${adminKey}`, "content-type": "application/json" },
    body: JSON.stringify({ name: "Customer website", allowed_origins: ["https://customer.example"] }) });
  assert.equal(response.status, 201, await response.clone().text());
  const project = await response.json();
  assert.equal(project.restart_required, false);
  assert.match(project.project_id, /^[0-9a-f-]{36}$/);
  const events = () => fetch(`${server.url}/v1/events`, { headers: { authorization: `Bearer ${project.secret_key}` } });
  assert.deepEqual((await (await events()).json()).events, []);
  await server.stop();
  await server.start();
  assert.equal((await events()).status, 200);
  const logs = await docker(["logs", server.name]);
  for (const key of [adminKey, project.public_key, project.secret_key]) assert.ok(!logs.includes(key));
});

test("a non-root container keeps SDK events and keys when replaced, and stops gracefully", { timeout: 120000 }, async (t) => {
  const server = await containerCollector();
  t.after(() => server.close());
  assert.notEqual(await docker(["exec", server.name, "id", "-u"]), "0");
  assert.equal(await docker(["exec", server.name, "stat", "-c", "%a", "/data/keys.json"]), "600");
  assert.deepEqual(await (await fetch(`${server.url}/healthz`)).json(), { status: "ok" });
  assert.equal((await fetch(`${server.url}/v1/events`)).status, 401);

  const media = Object.assign(new EventTarget(), { duration: 120, currentTime: 0, src: "", currentSrc: "",
    ownerDocument: { defaultView: null } });
  const events = [];
  const session = attachAnalytics(media, { videoId: "container-video", viewerId: "container-viewer",
    destinations: [{ send(event) { events.push(event); } }] });
  session.startAttempt();
  session.destroy();
  const headers = { "X-Chunkify-Ingest-Key": server.keys.public_key };
  const native = createHttpAdapter(`${server.url}/v1/events`, { headers });
  const otlp = createOtlpAdapter(`${server.url}/v1/logs`, { headers });
  await native.send(events[0]);
  for (const event of events.slice(1)) await otlp.send(event);
  const stored = await server.get("/v1/events");
  assert.deepEqual(stored.events.map(({ event }) => event), events);
  assert.ok(stored.events.at(-1).otlp);
  for (const record of stored.events) {
    assert.ok(isIP(record.event_context.peer_ip));
    assert.equal(record.event_context.client_ip, record.event_context.peer_ip);
    assert.equal(record.event_context.client_ip_source, "connection");
  }

  const stopped = await server.stop();
  assert.equal(stopped.ExitCode, 0, "docker stop should finish without SIGKILL");
  assert.equal(stopped.OOMKilled, false);
  await server.start();
  assert.deepEqual(await server.get("/v1/events"), stored);
  await createOtlpAdapter(`${server.url}/v1/logs`, { headers }).send(events[0]);
  assert.deepEqual(await server.get("/v1/summary"), { views: 1, unique_viewers: 1, total_playing_time_ms: 0 });
  assert.deepEqual(await server.get("/v1/events"), stored);
  const logs = await docker(["logs", server.name]);
  assert.ok(!logs.includes(server.keys.public_key) && !logs.includes(server.keys.secret_key));
});


test("docker exec manages live projects and keys in a non-root read-only container", { timeout: 120000 }, async (t) => {
  const server = await containerCollector();
  t.after(() => server.close());
  const command = async (...args) => JSON.parse(await docker(["exec", server.name, "chunkify-analytics", ...args]));
  const project = await command("project", "create", "Customer website");
  assert.equal(project.restart_required, false);
  assert.match(project.project_id, /^[0-9a-f-]{36}$/);
  assert.equal(project.name, "Customer website");
  const projectRead = await fetch(`${server.url}/v1/events`, { headers: { authorization: `Bearer ${project.secret_key}` } });
  assert.equal(projectRead.status, 200);
  assert.deepEqual((await projectRead.json()).events, []);
  const token = await command("key", "create", "default", "secret", "Dashboard");
  assert.equal(token.restart_required, false);
  assert.equal(await docker(["exec", server.name, "stat", "-c", "%a", "/data/admin.sock"]), "600");
  const status = async (secret) => (await fetch(`${server.url}/v1/summary`, { headers: { authorization: `Bearer ${secret}` } })).status;
  assert.equal(await status(token.key), 200);
  const rotated = await command("key", "rotate", "default", token.key_id);
  assert.equal(rotated.key_id, token.key_id);
  assert.equal(await status(token.key), 401);
  assert.equal(await status(rotated.key), 200);
  await server.stop("kill");
  await server.start();
  assert.equal(await status(token.key), 401);
  assert.equal(await status(rotated.key), 200);
  await command("key", "revoke", "default", token.key_id);
  assert.equal(await status(rotated.key), 401);
  assert.equal((await command("project", "list"))[0].keys.length, 2);
  const logs = await docker(["logs", server.name]);
  assert.ok(!logs.includes(token.key) && !logs.includes(rotated.key));
});
