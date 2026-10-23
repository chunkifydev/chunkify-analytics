'use strict';
const http = require('node:http');
const fs = require('node:fs/promises');
const path = require('node:path');

const projects = Object.freeze({ demo: 'CHUNKIFY_DEMO_READ_TOKEN', live: 'CHUNKIFY_LIVE_READ_TOKEN', staging: 'CHUNKIFY_STAGING_READ_TOKEN', player: 'CHUNKIFY_PLAYER_READ_TOKEN' });
const routes = Object.freeze({ '/api/summary': '/v1/summary', '/api/videos': '/v1/videos', '/api/events': '/v1/events', '/api/quality': '/v1/quality', '/api/engagement': '/v1/engagement', '/api/timeseries': '/v1/timeseries' });
const assets = Object.freeze({ '/': ['index.html', 'text/html'], '/app.js': ['app.js', 'text/javascript'], '/style.css': ['style.css', 'text/css'], '/event-details.js': ['event-details.js', 'text/javascript'] });
class SafeError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}
function buildQuery(url) {
  const allowed = new Set(['project', 'from', 'to', 'video_id', 'viewer_id', 'browser_name', 'os_name', 'device_type']);
  if (url.pathname === '/api/videos') allowed.add('cursor');
  if (url.pathname === '/api/timeseries') allowed.add('interval');
  for (const key of url.searchParams.keys()) {
    if (!allowed.has(key) || url.searchParams.getAll(key).length !== 1) throw new SafeError(400, 'Unsupported or duplicate query parameter.');
  }
  const project = url.searchParams.has('project') ? url.searchParams.get('project') : 'live';
  if (!Object.hasOwn(projects, project)) throw new SafeError(400, 'Unknown project.');
  const query = new URLSearchParams();
  for (const key of ['from', 'to', 'video_id', 'viewer_id', 'browser_name', 'os_name', 'device_type', 'cursor', 'interval']) {
    if (url.searchParams.has(key)) query.set(key, url.searchParams.get(key));
  }
  for (const key of ['from', 'to']) {
    if (query.has(key) && (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/.test(query.get(key)) || !Number.isFinite(Date.parse(query.get(key))))) {
      throw new SafeError(400, 'Dates must be RFC3339 timestamps with a timezone.');
    }
  }
  if (query.has('from') && query.has('to') && Date.parse(query.get('from')) >= Date.parse(query.get('to'))) throw new SafeError(400, 'From must be before to.');
  if (query.has('device_type') && !['desktop', 'mobile', 'tablet', 'unknown'].includes(query.get('device_type'))) throw new SafeError(400, 'Unknown device type.');
  if (url.pathname === '/api/timeseries' && (!query.has('from') || !query.has('to') || !['hour', 'day'].includes(query.get('interval')))) throw new SafeError(400, 'Timeline needs from, to and an hour or day interval.');
  const filters = Object.fromEntries([...query].filter(([key]) => key !== 'cursor'));
  if (Buffer.byteLength(JSON.stringify(filters)) > 4096) throw new SafeError(400, 'Filters are too large.');
  if (url.pathname === '/api/videos') query.set('limit', '8');
  return { project, query };
}
function createHandler({ env = process.env, fetchImpl = fetch, demo = null } = {}) {
  async function upstream(route, project, query, deadline) {
    if (!Object.values(routes).includes(route)) throw new SafeError(400, 'Unknown read route.');
    const token = env[projects[project]];
    if (!token) throw new SafeError(503, 'Read token is not configured for this project.');
    let base;
    try {
      base = new URL(env.COLLECTOR_URL);
      if (!['http:', 'https:'].includes(base.protocol) || base.username || base.password || base.search || base.hash || !['', '/'].includes(base.pathname)) throw new Error();
    } catch { throw new SafeError(503, 'Collector URL must be an HTTP(S) origin.'); }
    const target = new URL(route, base);
    target.search = query.toString();
    try {
      const response = await fetchImpl(target, { method: 'GET', headers: { Authorization: `Bearer ${token}`, Accept: 'application/json' }, redirect: 'error', signal: AbortSignal.any([deadline, AbortSignal.timeout(15000)]) });
      if (!response.ok) throw new SafeError(response.status === 400 ? 400 : 502, response.status === 400 ? 'Collector rejected the filters or cursor. Apply filters again.' : 'Collector read failed. Check backend configuration and collector availability.');
      const chunks = []; let bytes = 0;
      for await (const chunk of response.body) {
        bytes += chunk.byteLength;
        if (bytes > 1048576) throw new Error('Response too large');
        chunks.push(Buffer.from(chunk));
      }
      // Decode chunks together so UTF-8 characters split across chunks survive.
      const data = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      return data;
    } catch (error) {
      if (error instanceof SafeError) throw error;
      throw new SafeError(502, 'Collector unavailable or returned an invalid response.');
    }
  }
  return async (req, res) => {
    const headers = { 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', 'Content-Security-Policy': "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'", 'Referrer-Policy': 'no-referrer' };
    function json(status, data) { res.writeHead(status, { ...headers, 'Content-Type': 'application/json; charset=utf-8' }); res.end(JSON.stringify(data)); }
    try {
      if (req.method !== 'GET') throw new SafeError(405, 'Only GET is supported.');
      if (!req.url || req.url.length > 16384 || !req.url.startsWith('/') || req.url.startsWith('//')) throw new SafeError(400, 'Invalid request.');
      const url = new URL(req.url, 'http://127.0.0.1');
      const host = new URL(`http://${req.headers.host || 'invalid'}`);
      if (!['127.0.0.1', 'localhost', '[::1]'].includes(host.hostname)) throw new SafeError(403, 'Local access only.');
      if (req.headers.origin && req.headers.origin !== host.origin) throw new SafeError(403, 'Same-origin access only.');
      if (url.pathname === '/api/projects') {
        if (url.search) throw new SafeError(400, 'Unsupported query parameter.');
        const names = { demo: 'Demo data', live: 'Live', staging: 'Staging', player: 'Player test' };
        return json(200, { projects: Object.keys(projects).filter(id => env[projects[id]]).map(id => ({ id, name: names[id], synthetic: id === 'demo' })), ...(demo ? { demo } : {}) });
      }
      if (Object.hasOwn(routes, url.pathname)) {
        const { project, query } = buildQuery(url);
        // Bound the entire scan, not just each page, below the browser timeout.
        const deadline = AbortSignal.timeout(20000);
        if (url.pathname !== '/api/events') return json(200, await upstream(routes[url.pathname], project, query, deadline));
        query.set('limit', '10');
        let sample = []; let next = null; let pages = 0; let scanned = 0;
        do {
          if (next) query.set('cursor', next);
          const page = await upstream('/v1/events', project, query, deadline);
          if (!Array.isArray(page.events)) throw new SafeError(502, 'Invalid collector event page.');
          scanned += page.events.length;
          sample = sample.concat(page.events).slice(-10);
          next = page.next_cursor; pages++;
        } while (next && pages < 200);
        return json(200, { events: next ? [] : sample.reverse(), coverage: { complete: !next, scanned, pages, order: 'descending ingestion order', limit: 10 } });
      }
      if (!Object.hasOwn(assets, url.pathname)) throw new SafeError(404, 'Not found.');
      const [file, type] = assets[url.pathname];
      const content = await fs.readFile(path.join(__dirname, 'public', file));
      res.writeHead(200, { ...headers, 'Content-Type': `${type}; charset=utf-8` }); res.end(content);
    } catch (error) { json(error instanceof SafeError ? error.status : 500, { error: error instanceof SafeError ? error.message : 'Dashboard request failed.' }); }
  };
}
if (require.main === module) {
  const port = Number(process.env.PORT || 3000);
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('PORT must be between 1 and 65535.');
  const server = http.createServer(createHandler());
  server.on('error', () => { console.error('Dashboard could not listen on the configured port.'); process.exitCode = 1; });
  server.listen(port, '127.0.0.1', () => console.log(`Dashboard at http://127.0.0.1:${port}`));
}
module.exports = { createHandler, buildQuery };
