import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
import { root } from './start.mjs';

const exec = promisify(execFile);
test('shared archive excludes data and credentials even when stored under the example tree', { timeout: 60000, skip: !['darwin', 'linux'].includes(process.platform) }, async () => {
  const canary = await mkdtemp(path.join(root, 'examples/demo/.private-package-test-'));
  const dependencyCanary = await mkdtemp(path.join(root, 'node_modules/@chunkify/analytics/dist/.private-package-test-'));
  try {
    await writeFile(path.join(dependencyCanary, 'keys.json'), 'SYNTHETIC-PRIVATE-CANARY');
    await writeFile(path.join(canary, 'demo.json'), JSON.stringify({ read_token: 'SYNTHETIC-PRIVATE-CANARY' }));
    await writeFile(path.join(canary, 'analytics.duckdb'), 'synthetic data');
    await exec(process.execPath, ['examples/demo/pack.mjs'], { cwd: root, timeout: 50000 });
    const prefix = `chunkify-analytics-demo-${process.platform}-${process.arch}`;
    const archive = path.join(root, 'dist', prefix + '.tar.gz');
    const { stdout } = await exec('tar', ['-tzf', archive]);
    assert.ok(stdout.includes(prefix + '/bin/chunkify-analytics-collector'));
    assert.ok(stdout.includes(prefix + '/examples/demo/public/player.js'));
    assert.equal(stdout.includes(path.basename(canary)), false);
    assert.equal(stdout.includes(path.basename(dependencyCanary)), false);
    assert.equal(/(?:demo\.json|keys\.json|\.duckdb|\.env|demo-data)/.test(stdout), false);
    const { stdout: readme } = await exec('tar', ['-xOzf', archive, prefix + '/README.md']);
    assert.ok(readme.includes('Node.js 24'));
    // This deliberately checks the packaged artifact, not just the source allowlist.
    assert.equal((await readFile(path.join(canary, 'demo.json'), 'utf8')).includes('SYNTHETIC-PRIVATE-CANARY'), true);
  } finally { await rm(canary, { recursive: true, force: true }); await rm(dependencyCanary, { recursive: true, force: true }); }
});
