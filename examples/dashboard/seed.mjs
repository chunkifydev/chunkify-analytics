// Synthetic events for a separate demo project. Never use a production ingest key.
import { buildDemoEvents } from './seed-events.mjs';
import { setTimeout as delay } from 'node:timers/promises';
const endpoint = new URL('/v1/events', process.env.COLLECTOR_URL || 'http://127.0.0.1:4174');
const ingestKey = process.env.CHUNKIFY_DEMO_INGEST_KEY;
if (!ingestKey) throw new Error('Set CHUNKIFY_DEMO_INGEST_KEY for an isolated demo project.');
const events = buildDemoEvents();
for (const event of events) {
  await delay(100);
  const response = await fetch(endpoint, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Chunkify-Ingest-Key': ingestKey }, body: JSON.stringify(event), signal: AbortSignal.timeout(15000) });
  if (!response.ok) throw new Error(`Demo ingestion failed: ${response.status} ${await response.text()}`);
}
console.log(JSON.stringify({ synthetic: true, views: new Set(events.map(event => event.view_id)).size, events: events.length }));
