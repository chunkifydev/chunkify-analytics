// Create a clean, OS-specific demo bundle. Never copy a data directory or credentials.
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
import { findCollector, root } from './start.mjs';

const args = process.argv.slice(2);
if (args.length && (args.length !== 2 || args[0] !== '--collector')) throw new Error('Usage: npm run demo:pack -- [--collector PATH]');
if (!['darwin', 'linux'].includes(process.platform)) throw new Error('Demo packaging currently supports macOS and Linux. Windows packaging can be added later.');
const binary = await findCollector(args[1] || process.env.CHUNKIFY_COLLECTOR_BIN);
const name = `chunkify-analytics-demo-${process.platform}-${process.arch}`;
const output = path.join(root, 'dist');
await mkdir(output, { recursive: true });
const staging = await mkdtemp(path.join(output, '.demo-pack-'));
try {
  const bundle = path.join(staging, name);
  await mkdir(path.join(bundle, 'bin'), { recursive: true });
  await copyFile(binary, path.join(bundle, 'bin/chunkify-analytics-collector'));
  await chmod(path.join(bundle, 'bin/chunkify-analytics-collector'), 0o755);
  // Explicit files only: custom --data-dir paths or local files can live anywhere.
  const sourceFiles = [
    'examples/dashboard/server.cjs', 'examples/dashboard/seed-events.mjs',
    'examples/dashboard/public/index.html', 'examples/dashboard/public/app.js',
    'examples/dashboard/public/event-details.js', 'examples/dashboard/public/style.css',
    'examples/demo/start.mjs', 'examples/demo/server.mjs', 'examples/demo/README.md',
    'examples/demo/public/player.html', 'examples/demo/public/player.js', 'examples/demo/public/player.css',
  ];
  for (const file of sourceFiles) {
    await mkdir(path.dirname(path.join(bundle, file)), { recursive: true });
    await copyFile(path.join(root, file), path.join(bundle, file));
  }
  // The fixture generator uses the installed SDK; it has no runtime dependencies.
  const sdk = path.join(root, 'node_modules/@chunkify/analytics');
  const metadata = JSON.parse(await readFile(path.join(sdk, 'package.json'), 'utf8'));
  if (Object.keys(metadata.dependencies || {}).length) throw new Error('Update the bundle packer for the SDK runtime dependencies before packaging.');
  const sdkDestination = path.join(bundle, 'node_modules/@chunkify/analytics');
  await mkdir(sdkDestination, { recursive: true });
  await mkdir(path.join(sdkDestination, 'dist'));
  for (const name of ['browser-context', 'chunkify', 'core', 'datadog', 'event-types', 'hlsjs', 'http', 'index', 'mapper', 'monitor', 'options', 'otlp', 'player', 'posthog', 'shaka', 'videojs']) {
    await copyFile(path.join(sdk, 'dist', name + '.js'), path.join(sdkDestination, 'dist', name + '.js'));
  }
  for (const file of ['package.json', 'LICENSE']) await copyFile(path.join(sdk, file), path.join(sdkDestination, file));
  await copyFile(path.join(root, 'LICENSE'), path.join(bundle, 'LICENSE'));
  await copyFile(path.join(root, 'examples/demo/README.md'), path.join(bundle, 'README.md'));
  await writeFile(path.join(bundle, 'package.json'), JSON.stringify({ name: 'chunkify-analytics-demo', private: true, type: 'module', scripts: { demo: 'node examples/demo/start.mjs' }, engines: { node: '>=24 <25' } }, null, 2) + '\n');
  const launcher = `#!/bin/sh
cd -- "$(dirname -- "$0")" || exit 1
if ! command -v node >/dev/null 2>&1; then
  printf 'Install Node.js 24, then start the demo again. Press Enter to close.\\n'
  read -r demo_reply
  exit 1
fi
node examples/demo/start.mjs "$@"
demo_status=$?
if [ "$demo_status" -ne 0 ]; then
  printf 'Press Enter to close.\\n'
  read -r demo_reply
fi
exit "$demo_status"
`;
  const launcherPath = path.join(bundle, process.platform === 'darwin' ? 'Start demo.command' : 'start-demo.sh');
  await writeFile(launcherPath, launcher, { mode: 0o755 });
  const archive = path.join(output, name + '.tar.gz');
  await promisify(execFile)('tar', ['-czf', archive, '-C', staging, name]);
  console.log(`Demo bundle: ${archive}\nRequires Node.js 24 and a matching ${process.platform}/${process.arch} computer. Contains no demo data or credentials.`);
} finally { await rm(staging, { recursive: true, force: true }); }
