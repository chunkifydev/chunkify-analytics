import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { randomUUID } from 'node:crypto';
import test from 'node:test';
import { containerCollector } from './support.mjs';

const contract = JSON.parse(await readFile(new URL('../../../openapi.json', import.meta.url), 'utf8'));

test('packaged metrics work without a writable home or downloaded extensions', async t => {
  const server = await containerCollector({ settings: { "retention.seconds": '0', "duckdb.threads": '1' } });
  t.after(() => server.close());
  const empty = await server.get('/v1/quality');
  assert.equal(empty.views, 0);
  assert.equal(empty.startup_time_ms.median, null);
  const start = { ...contract.paths['/v1/events'].post.requestBody.content['application/json'].example,
    event_id: randomUUID(), view_id: randomUUID(), occurred_at: '2026-10-19T12:00:00Z', sequence: 1 };
  const end = { ...start, event_id: randomUUID(), sequence: 2, event_name: 'chunkify.video.view_ended',
    reason: 'ended', playing_time_ms: 9000, startup_time_ms: 250.25, rebuffer_duration_ms: 1000.5 };
  for (const event of [start, end]) {
    const response = await fetch(`${server.url}/v1/events`, { method: 'POST', headers: {
      'Content-Type': 'application/json', 'X-Chunkify-Ingest-Key': server.keys.ingest_key,
    }, body: JSON.stringify(event) });
    assert.equal(response.status, 201, await response.text());
  }
  const quality = await server.get('/v1/quality');
  assert.equal(quality.startup_time_ms.median, 250.25);
  assert.equal(quality.buffering.total_rebuffer_duration_ms, 1000.5);
  assert.equal((await server.get('/v1/engagement')).reached_end_views, 1);
  const timeline = await server.get('/v1/timeseries?from=2026-10-19T00:00:00Z&to=2026-10-20T00:00:00Z&interval=day');
  assert.equal(timeline.buckets[0].total_playing_time_ms, 9000);
});

test('time buckets reject unrepresentable playing-time totals', async t => {
  const server = await containerCollector({ settings: { "retention.seconds": '0', "duckdb.threads": '1' } });
  t.after(() => server.close());
  for (let i = 0; i < 2; i++) {
    const start = { ...contract.paths['/v1/events'].post.requestBody.content['application/json'].example,
      event_id: randomUUID(), view_id: randomUUID(), occurred_at: '2026-10-19T12:00:00Z', sequence: 1 };
    const end = { ...start, event_id: randomUUID(), sequence: 2, event_name: 'chunkify.video.view_ended', playing_time_ms: 1e308 };
    for (const event of [start, end]) {
      const response = await fetch(`${server.url}/v1/events`, { method: 'POST', headers: {
        'Content-Type': 'application/json', 'X-Chunkify-Ingest-Key': server.keys.ingest_key,
      }, body: JSON.stringify(event) });
      assert.equal(response.status, 201, await response.text());
    }
  }
  const response = await fetch(`${server.url}/v1/timeseries?from=2026-10-19T00:00:00Z&to=2026-10-20T00:00:00Z&interval=day`, {
    headers: { Authorization: `Bearer ${server.keys.read_token}` },
  });
  assert.equal(response.status, 500, await response.text());
});
