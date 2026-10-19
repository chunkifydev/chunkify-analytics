import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import test from "node:test";
import { fileURLToPath } from "node:url";

const execute = promisify(execFile);
const command = fileURLToPath(new URL("./load.mjs", import.meta.url));

test("the public load command leaves memory headroom below its RSS guard", async () => {
  const { stdout } = await execute(process.execPath, [command, "--list"]);
  const plan = JSON.parse(stdout);
  assert.ok(plan.generator_runtime.v8_heap_limit_bytes <= 768 * 1024 * 1024,
    `V8 heap limit ${plan.generator_runtime.v8_heap_limit_bytes / 1024 / 1024} MiB exceeds the 768 MiB budget`);
  assert.equal(plan.safety.max_generator_rss_mib, 1024);
});

test("the load command forwards arguments and configuration errors", async () => {
  const { stdout } = await execute(process.execPath, ["--expose-gc", command, "--list"], {
    env: { ...process.env, COLLECTOR_LOAD_CASES: "native-3000" },
  });
  const plan = JSON.parse(stdout);
  assert.deepEqual(plan.cases.map((item) => item.name), ["native-3000"]);
  assert.ok(plan.generator_runtime.exec_args.includes("--expose-gc"));
  await assert.rejects(execute(process.execPath, [command, "--list"], {
    env: { ...process.env, COLLECTOR_LOAD_CASES: "invalid-case" },
  }), (error) => error.code === 1 && /Unknown benchmark case/.test(error.stderr));
});

test("the worker rejects an oversized heap before starting a benchmark", async () => {
  const worker = fileURLToPath(new URL("./load-worker.mjs", import.meta.url));
  await assert.rejects(execute(process.execPath, ["--max-old-space-size=1024", worker, "--list"]),
    (error) => error.code === 1 && /Generator heap exceeds the 768 MiB budget/.test(error.stderr));
});
