import assert from "node:assert/strict";
import test from "node:test";
import { isIP } from "node:net";
import { attachAnalytics } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";
import { createOtlpAdapter } from "@chunkify/analytics/otlp";
import { containerCollector, docker } from "./support.mjs";

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
  const headers = { "X-Chunkify-Ingest-Key": server.keys.ingest_key };
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
  assert.ok(!logs.includes(server.keys.ingest_key) && !logs.includes(server.keys.read_token));
});
