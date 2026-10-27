import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, readFile, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { get } from 'node:http';
import path from 'node:path';
import { launchDemo } from './start.mjs';
import { buildDemoEvents } from '../dashboard/seed-events.mjs';

const quiet = () => {};
async function json(base, route) {
  const response = await fetch(base + route);
  assert.equal(response.status, 200);
  return response.json();
}
test('native demo seeds once, receives player events, resumes a seed and survives a restart', { timeout: 60000 }, async () => {
  const temp = await mkdtemp(path.join(tmpdir(), 'chunkify-demo-test-'));
  const dir = path.join(temp, 'data');
  let demo;
  try {
    demo = await launchDemo({ dataDir: dir, port: 0, open: false, log: quiet });
    const state = JSON.parse(await readFile(path.join(dir, 'demo.json'), 'utf8'));
    const config = await json(demo.dashboardUrl, '/api/player-config');
    assert.equal(config.endpoint, demo.collectorUrl + '/v1/events');
    const publicJson = JSON.stringify(config) + JSON.stringify(await json(demo.dashboardUrl, '/api/projects'));
    for (const project of Object.values(state.projects)) assert.equal(publicJson.includes(project.secret_key), false);
    assert.equal(publicJson.includes(state.projects.demo.public_key), false);
    assert.equal((await json(demo.dashboardUrl, '/api/summary?project=demo')).views, 268);
    assert.equal((await json(demo.dashboardUrl, '/api/summary?project=player')).views, 0);
    const event = { ...buildDemoEvents()[0], video_id: 'real-player-test' };
    for (let i = 0; i < 2; i++) {
      const response = await fetch(config.endpoint, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Chunkify-Ingest-Key': config.ingestKey, Origin: demo.dashboardUrl }, body: JSON.stringify(event) });
      assert.ok(response.ok); assert.equal(response.headers.get('access-control-allow-origin'), demo.dashboardUrl); await response.body.cancel();
    }
    assert.equal((await json(demo.dashboardUrl, '/api/summary?project=player')).views, 1);
    for (const route of ['/api/player-config', '/player', '/player.js']) {
      assert.equal((await fetch(demo.dashboardUrl + route, { headers: { Origin: 'https://foreign.example' } })).status, 403);
      assert.equal(await new Promise((resolve, reject) => {
        get(demo.dashboardUrl + route, { headers: { Host: 'foreign.example' } }, response => { response.resume(); resolve(response.statusCode); }).on('error', reject);
      }), 403);
      assert.equal((await fetch(demo.dashboardUrl + route, { method: 'POST' })).status, 400);
    }
    const previousUrl = demo.collectorUrl;
    await demo.stop(); demo = null;
    await assert.rejects(fetch(previousUrl + '/healthz'));
    // Simulate restart after ingestion completed but before the seed marker was saved.
    await writeFile(path.join(dir, 'demo.json'), JSON.stringify({ ...state, seeded: false }));
    demo = await launchDemo({ dataDir: dir, port: 0, open: false, log: quiet });
    assert.equal((await json(demo.dashboardUrl, '/api/summary?project=demo')).views, 268);
    assert.equal((await json(demo.dashboardUrl, '/api/summary?project=player')).views, 1);
    assert.equal((await json(demo.dashboardUrl, '/api/player-config')).ingestKey, config.ingestKey);
    const saved = JSON.parse(await readFile(path.join(dir, 'demo.json'), 'utf8'));
    assert.equal(saved.seedDate, state.seedDate); assert.equal(saved.seeded, true);
    await demo.stop(); demo = null;
    const logs = [];
    demo = await launchDemo({ dataDir: dir, port: 0, open: false, log: message => logs.push(message) });
    assert.equal(logs.some(message => message.includes('Loading 268')), false);
  } finally { await demo?.stop(); await rm(temp, { recursive: true, force: true }); }
});

test('demo refuses an unrelated data directory and releases its port on failure', async () => {
  const temp = await mkdtemp(path.join(tmpdir(), 'chunkify-demo-refuse-'));
  try {
    await writeFile(path.join(temp, 'keep.txt'), 'existing data');
    await assert.rejects(launchDemo({ dataDir: temp, port: 0, open: false, log: quiet }), /already exists without demo.json/);
    assert.equal(await readFile(path.join(temp, 'keep.txt'), 'utf8'), 'existing data');
    const abort = new AbortController(); abort.abort();
    await assert.rejects(launchDemo({ dataDir: path.join(temp, 'new'), port: 0, open: false, signal: abort.signal }), /abort/i);
  } finally { await rm(temp, { recursive: true, force: true }); }
});
