#!/usr/bin/env node
import { createServer } from 'node:http';
import { createHash } from 'node:crypto';
import { access, mkdir, mkdtemp, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { createWriteStream } from 'node:fs';
import { spawn, execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { once } from 'node:events';
import { fileURLToPath, pathToFileURL } from 'node:url';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { createDemoHandler } from './server.mjs';

const exec = promisify(execFile);
export const root = fileURLToPath(new URL('../../', import.meta.url));
const executable = process.platform === 'win32' ? 'chunkify-analytics.exe' : 'chunkify-analytics';
export async function findCollector(explicit) {
  const candidates = explicit ? [path.resolve(explicit)] : ['bin', 'target/release', 'target/debug'].map(dir => path.join(root, dir, executable));
  for (const candidate of candidates) {
    try { await access(candidate); return candidate; } catch {}
  }
  throw new Error('Collector executable not found. Put it in bin/, pass --collector /path/to/executable, or build with cargo build --locked.');
}
async function saveState(dir, state) {
  const temporary = path.join(dir, 'demo.json.tmp');
  const saved = { ...state, projects: Object.fromEntries(Object.entries(state.projects).map(([id, project]) => [id, { project_id: project.project_id, name: project.name }])) };
  await writeFile(temporary, JSON.stringify(saved, null, 2) + '\n', { mode: 0o600 });
  await rename(temporary, path.join(dir, 'demo.json'));
}
// Analytics settings for this demo directory. Commands run in the directory, so
// they and the demo server use the same ClickHouse database.
async function writeConfig(dir, clickhouse, database, origins = []) {
  await writeFile(path.join(dir, 'config.toml'), `server.allowed_origins = ${JSON.stringify(origins)}
server.ingest_per_second = 10000
retention.seconds = 0
clickhouse.url = ${JSON.stringify(clickhouse)}
clickhouse.database = ${JSON.stringify(database)}
clickhouse.registry_refresh_seconds = 1
`, { mode: 0o600 });
}
// One running demo per directory: a second launch would rotate the first one's keys.
const running = new Set();
async function lock(dir) {
  const busy = new Error('A demo is already running for this data directory. Stop it first.');
  if (running.has(dir)) throw busy;
  const file = path.join(dir, 'demo.lock');
  // Create the lock exclusively, so two launches cannot both claim it. A lock
  // left by a process that no longer runs is removed once, then claimed.
  for (let attempt = 0; ; attempt++) {
    try {
      await writeFile(file, String(process.pid), { mode: 0o600, flag: 'wx' });
      break;
    } catch (error) {
      if (error.code !== 'EEXIST' || attempt > 0) throw error.code === 'EEXIST' ? busy : error;
    }
    const pid = Number(await readFile(file, 'utf8').catch(() => ''));
    if (pid && pid !== process.pid) {
      try { process.kill(pid, 0); throw busy; }
      catch (error) { if (error.code !== 'ESRCH') throw error; }
    }
    await rm(file, { force: true });
  }
  running.add(dir);
  return async () => { running.delete(dir); await rm(file, { force: true }); };
}
async function prepareData(dir, binary, env, signal, clickhouse) {
  try {
    const content = await readFile(path.join(dir, 'demo.json'), 'utf8');
    let state;
    try { state = JSON.parse(content); } catch { throw new Error('Invalid demo.json. Restore the saved demo state or choose a new data directory.'); }
    const hasProject = id => typeof state.projects?.[id]?.project_id === 'string' && /^[0-9a-f-]{36}$/.test(state.projects[id].project_id);
    if (state.version !== 1 || typeof state.seeded !== 'boolean' || !hasProject('demo') || !hasProject('player') ||
      typeof state.database !== 'string' || !/^chunkify_demo_[0-9a-f]{16}$/.test(state.database) ||
      !/^\d{4}-\d{2}-\d{2}T00:00:00\.000Z$/.test(state.seedDate) || !Number.isFinite(Date.parse(state.seedDate))) throw new Error('Invalid demo state.');
    return state;
  } catch (error) {
    if (error.code !== 'ENOENT') throw error;
  }
  // Never initialize a pre-existing directory without our demo marker.
  try { await access(dir); throw new Error('Data directory already exists without demo.json. Choose a new --data-dir; existing data will not be changed.'); }
  catch (error) { if (error.code !== 'ENOENT') throw error; }
  await mkdir(path.dirname(dir), { recursive: true });
  const staging = await mkdtemp(dir + '.setup-');
  try {
    const database = `chunkify_demo_${createHash('sha256').update(`${dir}:${Date.now()}:${Math.random()}`).digest('hex').slice(0, 16)}`;
    await writeConfig(staging, clickhouse, database);
    const run = async args => {
      signal.throwIfAborted();
      try { return (await exec(binary, args, { env, cwd: staging, signal, timeout: 30000 })).stdout; }
      catch { throw new Error(`Collector ${args[0]} failed. Check that ClickHouse is running at ${clickhouse} and that the executable matches your OS and this repository version.`); }
    };
    await run(['init']);
    const state = { version: 1, database, seedDate: new Date(new Date().setUTCHours(0, 0, 0, 0)).toISOString(), seeded: false, projects: {} };
    for (const [id, name] of [['demo', 'Synthetic demo data'], ['player', 'Player test']]) {
      state.projects[id] = JSON.parse(await run(['project', 'create', name]));
    }
    await saveState(staging, state);
    signal.throwIfAborted();
    await rename(staging, dir);
    return state;
  } finally { await rm(staging, { recursive: true, force: true }); }
}
async function refreshKeys(dir, binary, env, signal, state) {
  // Keep runtime credentials in memory. Each launch replaces the demo's keys.
  const run = async args => {
    signal.throwIfAborted();
    try { return JSON.parse((await exec(binary, args, { env, cwd: dir, signal, timeout: 30000 })).stdout); }
    catch { throw new Error('Could not refresh demo credentials. Stop any running demo using this directory and try again.'); }
  };
  const projects = await run(['project', 'list']);
  for (const project of Object.values(state.projects)) {
    const saved = projects.find(item => item.id === project.project_id && !item.deleted);
    if (!saved) throw new Error('Demo project is missing from the registry.');
    for (const permission of ['public', 'secret']) {
      const key = saved.keys.find(item => item.permission === permission && !item.read_only);
      if (!key) throw new Error('Demo project credential is missing.');
      project[`${permission}_key`] = (await run(['key', 'rotate', project.project_id, key.id])).key;
    }
  }
}
async function seed(url, state, signal) {
  let buildDemoEvents;
  try { ({ buildDemoEvents } = await import('../dashboard/seed-events.mjs')); }
  catch { throw new Error('Demo dependencies are missing. Run npm ci in the repository, or use a complete demo bundle.'); }
  const events = buildDemoEvents(new Date(state.seedDate));
  // Small concurrent groups keep first launch quick. Stable IDs make an interrupted seed resumable.
  for (let offset = 0; offset < events.length; offset += 8) {
    signal.throwIfAborted();
    await Promise.all(events.slice(offset, offset + 8).map(async event => {
      for (let attempt = 0; ; attempt++) {
        const response = await fetch(url + '/v1/events', {
          method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Chunkify-Ingest-Key': state.projects.demo.public_key },
          body: JSON.stringify(event), signal: AbortSignal.any([signal, AbortSignal.timeout(15000)]),
        });
        await response.body?.cancel();
        if (response.ok) return;
        if (![429, 503].includes(response.status) || attempt === 2) throw new Error(`Sample ingestion failed with HTTP ${response.status}. Restart the demo to resume.`);
        await delay(250 * (attempt + 1), undefined, { signal });
      }
    }));
  }
}
export async function launchDemo({ collector, clickhouse = process.env.CHUNKIFY_CLICKHOUSE_URL ?? 'http://127.0.0.1:8123',
  dataDir = path.join(root, 'demo-data'), port = 8787, open = true, signal = new AbortController().signal, log = console.log } = {}) {
  if (Number(process.versions.node.split('.')[0]) !== 24) throw new Error('Use Node.js 24 for this demo.');
  if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('--port must be between 0 and 65535.');
  const binary = await findCollector(collector || process.env.CHUNKIFY_BINARY);
  const dir = path.resolve(dataDir);
  let handler, child, collectorLog, stopping, unlock;
  const server = createServer((req, res) => {
    if (handler) return handler(req, res);
    res.writeHead(503, { 'Content-Type': 'text/plain' }); res.end('Preparing the local demo.');
  });
  const stop = () => stopping ||= (async () => {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    if (child && child.exitCode === null && child.signalCode === null) {
      const closed = once(child, 'close');
      child.kill('SIGTERM');
      const timer = setTimeout(() => child.kill('SIGKILL'), 15000); timer.unref();
      await closed; clearTimeout(timer);
    }
    collectorLog?.end();
    await unlock?.();
  })();
  signal.addEventListener('abort', stop, { once: true });
  try {
    signal.throwIfAborted();
    server.listen(port, '127.0.0.1');
    try { await once(server, 'listening'); }
    catch { throw new Error(`Dashboard port ${port} is unavailable. Stop the other demo, or use --port 0 to choose a free port.`); }
    const dashboardUrl = `http://127.0.0.1:${server.address().port}`;
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('CHUNKIFY_')));
    Object.assign(env, { CHUNKIFY_BIND: '127.0.0.1:0' });
    const state = await prepareData(dir, binary, env, signal, clickhouse);
    unlock = await lock(dir);
    const origins = [dashboardUrl, 'http://localhost:4173', 'http://127.0.0.1:4173', 'http://localhost:5173', 'http://127.0.0.1:5173'];
    await writeConfig(dir, clickhouse, state.database, origins);
    signal.throwIfAborted();
    collectorLog = createWriteStream(path.join(dir, 'collector.log'), { flags: 'a', mode: 0o600 });
    child = spawn(binary, ['serve'], { env, cwd: dir, stdio: ['ignore', 'pipe', 'pipe'] });
    child.stdout.pipe(collectorLog, { end: false }); child.stderr.pipe(collectorLog, { end: false });
    const collectorUrl = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`Collector did not start. See ${path.join(dir, 'collector.log')}.`)), 30000);
      let output = '';
      const finish = (error, value) => { clearTimeout(timer); child.stdout.off('data', read); error ? reject(error) : resolve(value); };
      const read = chunk => {
        output = (output + chunk).slice(-4096);
        const match = output.match(/Listening on (http:\/\/127\.0\.0\.1:\d+)/);
        if (match) finish(null, match[1]);
      };
      child.stdout.on('data', read);
      child.once('error', () => finish(new Error('Could not launch the collector executable.')));
      child.once('close', () => finish(new Error(`Collector stopped. See ${path.join(dir, 'collector.log')}.`)));
    });
    child.once('close', () => { if (!stopping) { log('Collector stopped; closing the demo.'); void stop(); } });
    if (!state.projects.demo.secret_key || !state.projects.player.secret_key) await refreshKeys(dir, binary, env, signal, state);
    if (!state.seeded) {
      log('Loading 268 sample views. This only runs on first launch.');
      await seed(collectorUrl, state, signal);
      state.seeded = true; await saveState(dir, state);
    }
    signal.throwIfAborted();
    handler = createDemoHandler({ collectorUrl, state });
    log(`Dashboard: ${dashboardUrl}\nPlayer: ${dashboardUrl}/player\nData: ${dir}\nPress Ctrl+C to stop. Data is kept for the next launch.`);
    if (open) {
      const command = process.platform === 'darwin' ? ['open', dashboardUrl] : process.platform === 'win32' ? ['cmd', '/c', 'start', '', dashboardUrl] : ['xdg-open', dashboardUrl];
      const browser = spawn(command[0], command.slice(1), { stdio: 'ignore', detached: true });
      browser.on('error', () => log('Open the dashboard URL above in your browser.')); browser.unref();
    }
    return { dashboardUrl, collectorUrl, dataDir: dir, stop };
  } catch (error) { await stop(); throw error; }
}
function options(args) {
  const result = {};
  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--no-open') result.open = false;
    else if (['--collector', '--clickhouse', '--data-dir', '--port'].includes(args[i]) && args[i + 1] && !args[i + 1].startsWith('--')) {
      const key = { '--collector': 'collector', '--clickhouse': 'clickhouse', '--data-dir': 'dataDir', '--port': 'port' }[args[i]];
      result[key] = key === 'port' ? Number(args[++i]) : args[++i];
    } else throw new Error('Usage: npm run demo -- [--collector PATH] [--clickhouse URL] [--data-dir PATH] [--port NUMBER] [--no-open]');
  }
  return result;
}
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  const abort = new AbortController();
  for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => abort.abort());
  try { await launchDemo({ ...options(process.argv.slice(2)), signal: abort.signal }); }
  catch (error) { if (!abort.signal.aborted) { console.error(error.message); process.exitCode = 1; } }
}
