import { readFile } from 'node:fs/promises';
import { createHandler } from '../dashboard/server.cjs';

const assets = { '/player': ['player.html', 'text/html'], '/player.js': ['player.js', 'text/javascript'], '/player.css': ['player.css', 'text/css'] };
export function createDemoHandler({ collectorUrl, state }) {
  const from = new Date(Date.parse(state.seedDate) - 6 * 86400000).toISOString().slice(0, 10);
  const dashboard = createHandler({ env: {
    COLLECTOR_URL: collectorUrl,
    CHUNKIFY_DEMO_SECRET_KEY: state.projects.demo.secret_key,
    CHUNKIFY_PLAYER_SECRET_KEY: state.projects.player.secret_key,
  }, demo: { player_url: '/player', from, to: state.seedDate.slice(0, 10) } });
  return async (req, res) => {
    let url;
    try {
      if (!req.url?.startsWith('/') || req.url.startsWith('//') || req.url.length > 16384) throw new Error();
      url = new URL(req.url, 'http://127.0.0.1');
    } catch { res.writeHead(400); return res.end('Invalid request.'); }
    if (!Object.hasOwn(assets, url.pathname) && url.pathname !== '/api/player-config') return dashboard(req, res);
    const headers = {
      'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer',
      'Content-Security-Policy': "default-src 'self'; script-src 'self' https://cdn.jsdelivr.net; style-src 'self' 'unsafe-inline' https://cdn.jsdelivr.net; connect-src 'self' http: https:; media-src http: https: blob:; img-src 'self' data: https: blob:; worker-src blob:; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
    };
    try {
      const host = new URL(`http://${req.headers.host || 'invalid'}`);
      if (!['127.0.0.1', 'localhost', '[::1]'].includes(host.hostname) || req.headers.origin && req.headers.origin !== host.origin) {
        res.writeHead(403, headers); return res.end('Local same-origin access only.');
      }
      if (req.method !== 'GET' || url.search) { res.writeHead(400, headers); return res.end('Unsupported request.'); }
      if (url.pathname === '/api/player-config') {
        res.writeHead(200, { ...headers, 'Content-Type': 'application/json' });
        return res.end(JSON.stringify({ endpoint: collectorUrl + '/v1/events', ingestKey: state.projects.player.public_key }));
      }
      const [file, type] = assets[url.pathname];
      const content = await readFile(new URL('./public/' + file, import.meta.url));
      res.writeHead(200, { ...headers, 'Content-Type': type + '; charset=utf-8' }); res.end(content);
    } catch { res.writeHead(500, headers); res.end('Demo page unavailable.'); }
  };
}
