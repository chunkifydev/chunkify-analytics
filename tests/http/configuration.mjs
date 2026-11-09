import { createHash } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

// The ClickHouse server for tests. Each test server gets its own database.
export const clickhouseUrl = process.env.CHUNKIFY_TEST_CLICKHOUSE_URL ?? "http://127.0.0.1:8123";

const arrays = new Set(["server.allowed_origins", "server.trusted_proxies", "mcp.allowed_hosts"]);
const strings = new Set(["server.bind", "server.geoip_database", "logging.level", "mcp.url", "mcp.secret_key",
  "clickhouse.url", "clickhouse.database", "clickhouse.user", "clickhouse.password"]);
const field = name => /^(server|retention|batch|clickhouse|mcp|rate_limits|logging|cache)\./.test(name);

// Fixtures use the same dotted field names as config.toml. Preserve invalid
// values so the executable, rather than this helper, validates configuration.
export function configSource(settings) {
  // A null value leaves the field to the server's default.
  return Object.entries(settings).filter(([name, value]) => field(name) && value !== null).map(([name, original]) => {
    let value = original;
    if (typeof value === "string") {
      if (arrays.has(name)) value = value ? value.split(",") : [];
      else if (!strings.has(name)) {
        if (/^-?\d+$/.test(value)) return `${name} = ${value}`;
        if (value === "true" || value === "false") value = value === "true";
        else if (value.trim() && Number.isFinite(Number(value))) value = Number(value);
      }
    }
    return `${name} = ${JSON.stringify(value)}`;
  }).join("\n") + "\n";
}

export function collectorEnvironment(settings = process.env) {
  return Object.fromEntries(Object.entries(settings).filter(([name]) => !field(name) && name !== "data_dir"
    && (!name.startsWith("CHUNKIFY_") || name === "CHUNKIFY_ADMIN_KEY")));
}

// The database for one test server, named after its working directory
// (`data_dir` in fixture settings) unless the settings choose one.
export function databaseFor(directory, settings = {}) {
  if (settings["clickhouse.database"]) return settings["clickhouse.database"];
  const id = createHash("sha256").update(resolve(settings.data_dir ?? directory)).digest("hex").slice(0, 16);
  return `chunkify_test_${id}`;
}

// Databases created through writeConfig in this process; see dropTestDatabases.
const created = new Set();

export async function writeConfig(directory, settings) {
  const path = join(directory, "config.toml");
  created.add(databaseFor(directory, settings));
  await writeFile(path, configSource({ "server.geoip_database": resolve("tests/fixtures/GeoIP2-City-Test.mmdb"),
    // Tests read right after ingesting, so reports stay exact unless a test asks for lag.
    "clickhouse.url": clickhouseUrl, "clickhouse.rollup_max_lag_seconds": "0", ...settings,
    "clickhouse.database": databaseFor(directory, settings) }), { mode: 0o600 });
  return path;
}

// Drop the databases this process created. Test files call it once after all tests.
export async function dropTestDatabases() {
  for (const database of created) await clickhouseQuery(`DROP DATABASE IF EXISTS ${database}`).catch(() => {});
  created.clear();
}

// Run SQL on the test ClickHouse server, for setup and inspection only.
export async function clickhouseQuery(query) {
  const response = await fetch(clickhouseUrl, { method: "POST", body: query });
  const body = await response.text();
  if (!response.ok) throw new Error(`ClickHouse query failed: ${body}`);
  return body;
}

// The current project registry document of a test server's database.
export async function storedRegistry(directory, settings = {}) {
  const rows = await clickhouseQuery(`SELECT document FROM ${databaseFor(directory, settings)}.registry ORDER BY version FORMAT JSONEachRow`);
  return rows.trim().split("\n").filter(Boolean).map(line => JSON.parse(JSON.parse(line).document));
}
