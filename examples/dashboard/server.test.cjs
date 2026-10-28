'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const { randomBytes } = require('node:crypto');
const { createHandler } = require('./server.cjs');

async function request(handler, url, method = 'GET') {
  const res = { writeHead(status, headers) { this.status = status; this.headers = headers; }, end(body) { this.body = JSON.parse(body); } };
  await handler({ url, method, headers: { host: '127.0.0.1' } }, res);
  return res;
}
function setup(fetchImpl) {
  const env = { CHUNKIFY_URL: 'http://127.0.0.1:9000', CHUNKIFY_LIVE_SECRET_KEY: randomBytes(24).toString('hex'), CHUNKIFY_STAGING_SECRET_KEY: randomBytes(24).toString('hex') };
  return { env, handler: createHandler({ env, fetchImpl }) };
}
test('fixed project and route mapping, summary passthrough, exact filters and eight-row pages', async () => {
  const calls = []; const payload = { probe: randomBytes(8).toString('hex') };
  const { handler, env } = setup(async (url, options) => { calls.push({ url, options }); return Response.json(payload); });
  const query = new URLSearchParams({ project: 'staging', from: '2026-10-18T00:00:00Z', to: '2026-10-20T00:00:00Z', video_id: ' exact & ID ' });
  const result = await request(handler, `/api/summary?${query}`);
  assert.equal(result.status, 200); assert.deepEqual(result.body, payload);
  assert.equal(calls[0].url.pathname, '/v1/summary');
  assert.equal(calls[0].url.searchParams.get('video_id'), ' exact & ID ');
  assert.equal(calls[0].url.searchParams.has('limit'), false);
  assert.equal(calls[0].options.headers.Authorization, `Bearer ${env.CHUNKIFY_STAGING_SECRET_KEY}`);
  assert.equal(calls[0].options.redirect, 'error');
  query.set('cursor', 'opaque+/=');
  await request(handler, `/api/videos?${query}`);
  assert.equal(calls[1].url.searchParams.get('limit'), '8');
  assert.equal(calls[1].url.searchParams.get('cursor'), 'opaque+/=');
  assert.equal(calls[1].url.searchParams.get('from'), query.get('from'));
  await request(handler, '/api/summary?project=live');
  assert.equal(calls[2].options.headers.Authorization, `Bearer ${env.CHUNKIFY_LIVE_SECRET_KEY}`);
});
test('rejects arbitrary routes, projects, query keys, duplicate keys, dates and write methods without upstream access', async () => {
  const { handler } = setup(() => { throw new Error('Must not call upstream'); });
  for (const url of ['/api/summary?project=', '/api/summary?project=other', '/api/summary?project=toString', '/api/summary?url=http://example.com', '/api/summary?project=live&project=staging', '/api/summary?from=yesterday', '/api/summary?from=2026-10-20T00:00:00Z&to=2026-10-18T00:00:00Z', '/api/summary?cursor=x', '/api/videos?limit=100']) {
    assert.equal((await request(handler, url)).status, 400);
  }
  assert.equal((await request(handler, '/v1/logs')).status, 404);
  assert.equal((await request(handler, '/api/summary', 'POST')).status, 405);
});
test('error responses never include upstream error bodies or exception details', async () => {
  const secret = randomBytes(24).toString('hex');
  for (const fetchImpl of [async () => new Response(secret, { status: 401 }), async () => { throw new Error(secret); }, async () => new Response(secret)]) {
    const { handler } = setup(fetchImpl);
    const result = await request(handler, '/api/summary');
    assert.equal(result.status, 502); assert.equal(JSON.stringify(result.body).includes(secret), false);
  }
  const handler = createHandler({ env: {}, fetchImpl: () => { throw new Error(); } });
  assert.equal((await request(handler, '/api/summary')).status, 503);
});
test('new metric routes preserve the same cohort filters and keep interval on the timeline', async () => {
  const calls = []; const payload = { views: 7, value: null };
  const { handler } = setup(async url => { calls.push(url); return Response.json(payload); });
  const filters = { project: 'live', from: '2026-10-18T00:00:00Z', to: '2026-10-25T00:00:00Z', video_id: 'video-a', viewer_id: 'viewer-b', browser_name: 'Chrome', os_name: 'macOS', device_type: 'desktop' };
  for (const route of ['summary', 'quality', 'engagement', 'timeseries', 'videos']) {
    const query = new URLSearchParams(filters); if (route === 'timeseries') query.set('interval', 'day');
    const result = await request(handler, `/api/${route}?${query}`);
    assert.equal(result.status, 200); assert.deepEqual(result.body, payload);
    const forwarded = calls.at(-1);
    assert.equal(forwarded.pathname, `/v1/${route}`);
    for (const [key, value] of Object.entries(filters)) if (key !== 'project') assert.equal(forwarded.searchParams.get(key), value);
    assert.equal(forwarded.searchParams.has('project'), false);
    assert.equal(forwarded.searchParams.has('interval'), route === 'timeseries');
  }
  for (const query of ['device_type=bot', 'interval=day', 'limit=10', 'cursor=x']) assert.equal((await request(handler, `/api/quality?${query}`)).status, 400);
  assert.equal((await request(handler, '/api/timeseries?interval=day')).status, 400);
});

test('project discovery contains only configured labels, never secrets', async () => {
  const { env, handler } = setup(() => { throw new Error('No upstream request expected'); });
  const response = await request(handler, '/api/projects');
  assert.deepEqual(response.body.projects.map(p => p.id), ['live', 'staging']);
  assert.equal(JSON.stringify(response.body).includes(env.CHUNKIFY_LIVE_SECRET_KEY), false);
});

test('foreign hosts and origins cannot read the local credentialed proxy', async () => {
  const { handler } = setup(() => { throw new Error('No upstream request expected'); });
  for (const headers of [{ host: 'attacker.example' }, { host: '127.0.0.1', origin: 'https://attacker.example' }]) {
    const res = { writeHead(status) { this.status = status; }, end() {} };
    await handler({ url: '/api/summary', method: 'GET', headers }, res);
    assert.equal(res.status, 403);
  }
});

test('small event pages fit ten large accepted events under the response cap', async () => {
  const { handler } = setup(async url => {
    assert.equal(url.searchParams.get('limit'), '10');
    return Response.json({ events: Array.from({ length: 10 }, (_, id) => ({ event: { event_id: id, custom: 'x'.repeat(60000) } })), next_cursor: null });
  });
  const result = await request(handler, '/api/events');
  assert.equal(result.status, 200);
  assert.equal(result.body.events.length, 10);
});


test('latest events use one bounded descending request even when older pages exist', async () => {
  const calls = [];
  const raw = '<img src=x onerror=alert(1)>';
  const events = Array.from({ length: 10 }, (_, i) => ({ event: { event_id: `event-${816-i}`, video_id: raw } }));
  const { handler } = setup(async url => {
    calls.push(url);
    assert.equal(url.searchParams.get('order'), 'desc');
    assert.equal(url.searchParams.get('limit'), '10');
    assert.equal(url.searchParams.get('video_id'), 'exact');
    assert.equal(url.searchParams.get('browser_name'), 'Chrome');
    assert.equal(url.searchParams.has('cursor'), false);
    return Response.json({ events, next_cursor: 'older-records' });
  });
  const result = await request(handler, '/api/events?project=live&video_id=exact&browser_name=Chrome');
  assert.equal(result.status, 200);
  assert.equal(calls.length, 1);
  assert.deepEqual(result.body.events, events);
  assert.equal(result.body.coverage.complete, true);
  assert.equal(result.body.coverage.has_more, true);
});
