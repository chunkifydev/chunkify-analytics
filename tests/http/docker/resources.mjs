import assert from "node:assert/strict";

function integer(name, fallback, min, max) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < min || value > max) throw new Error(`${name} must be ${min}..${max}`);
  return value;
}

export const containerLimits = {
  cpus: integer("COLLECTOR_LOAD_CPUS", 2, 1, 16),
  memory_mib: integer("COLLECTOR_LOAD_MEMORY_MIB", 2048, 1024, 16384),
  swap_mib: 0,
};
export const resources = [
  `--cpus=${containerLimits.cpus}`, `--memory=${containerLimits.memory_mib}m`,
  `--memory-swap=${containerLimits.memory_mib}m`,
];
// Preserve the original 1,800/2,048 MiB guard ratio as the hard limit changes.
export const containerMemoryGuardMiB = Math.floor(containerLimits.memory_mib * 1800 / 2048);

export function checkEngineBudget(info) {
  assert.ok(containerLimits.cpus <= info.NCPU, "Collector CPU budget exceeds Docker's available CPUs");
  assert.ok((containerLimits.memory_mib + 512) * 1024 * 1024 <= info.MemTotal,
    "Collector memory budget must leave at least 512 MiB for the Docker VM; other workloads need additional headroom");
}

// These fields describe different and sometimes overlapping memory categories.
// Inactive file pages are not a guarantee of immediately reclaimable memory.
export function parseMemoryStats(raw) {
  const read = (pattern, name) => {
    const text = raw.match(pattern)?.[1];
    const value = text === undefined ? NaN : Number(text);
    assert.ok(Number.isSafeInteger(value) && value >= 0,
      `Resource sample unavailable: ${name}. The benchmark requires cgroup v2 memory accounting and PSI.`);
    return value;
  };
  const fields = {};
  for (const name of ["anon", "file", "inactive_file", "file_dirty", "file_writeback"]) {
    fields[`container_${name}_bytes`] = read(new RegExp(`^${name} (\\S+)$`, "m"), name);
  }
  for (const name of ["high", "max", "oom", "oom_kill"]) {
    fields[`container_memory_${name}_events`] = read(new RegExp(`^${name} (\\S+)$`, "m"), name);
  }
  for (const name of ["some", "full"]) {
    fields[`container_memory_pressure_${name}_usec`] = read(new RegExp(`^${name} .*\\btotal=(\\S+)$`, "m"), `${name} pressure`);
  }
  return fields;
}
