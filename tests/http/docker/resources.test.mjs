import assert from "node:assert/strict";
import test from "node:test";
import { checkEngineBudget, containerLimits, parseMemoryStats, resources } from "./resources.mjs";

const sample = `123456
usage_usec 1900
VmRSS: 123 kB
anon 524288
file 262144
inactive_file 131072
file_dirty 4096
file_writeback 0
high 1
max 2
oom 3
oom_kill 0
some avg10=0.00 avg60=0.00 avg300=0.00 total=789
full avg10=0.00 avg60=0.00 avg300=0.00 total=123
`;

test("memory samples retain cache, pressure and OOM counters as distinct measurements", () => {
  const values = parseMemoryStats(sample);
  assert.equal(values.container_anon_bytes, 524288);
  assert.equal(values.container_file_bytes, 262144);
  assert.equal(values.container_inactive_file_bytes, 131072);
  assert.equal(values.container_file_dirty_bytes, 4096);
  assert.equal(values.container_file_writeback_bytes, 0);
  assert.equal(values.container_memory_high_events, 1);
  assert.equal(values.container_memory_max_events, 2);
  assert.equal(values.container_memory_oom_events, 3);
  assert.equal(values.container_memory_oom_kill_events, 0);
  assert.equal(values.container_memory_pressure_some_usec, 789);
  assert.equal(values.container_memory_pressure_full_usec, 123);
  for (const input of [sample.replace("file 262144\n", ""), sample.replace("anon 524288", "anon -1"),
    sample.replace("total=789", "total=NaN"), sample.replace("oom_kill 0", "oom_kill missing")]) {
    assert.throws(() => parseMemoryStats(input), /Resource sample unavailable/);
  }
});

test("Docker limits disable swap and reject budgets without VM headroom", () => {
  assert.deepEqual(resources, [`--cpus=${containerLimits.cpus}`, `--memory=${containerLimits.memory_mib}m`,
    `--memory-swap=${containerLimits.memory_mib}m`]);
  const info = { NCPU: containerLimits.cpus, MemTotal: (containerLimits.memory_mib + 512) * 1048576 };
  assert.doesNotThrow(() => checkEngineBudget(info));
  assert.throws(() => checkEngineBudget({ ...info, NCPU: containerLimits.cpus - 1 }), /CPU budget/);
  assert.throws(() => checkEngineBudget({ ...info, MemTotal: info.MemTotal - 1 }), /512 MiB/);
});
