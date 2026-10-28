import { writeFile } from "node:fs/promises";
import { join } from "node:path";

const arrays = new Set(["server.allowed_origins", "server.trusted_proxies", "mcp.allowed_hosts"]);
const strings = new Set(["data_dir", "server.bind", "batch.insert_mode", "storage.backend", "storage.path",
  "storage.url", "storage.endpoint", "storage.region", "storage.access_key_id", "storage.secret_access_key",
  "storage.session_token", "storage.url_style", "mcp.url", "mcp.secret_key"]);
const field = name => name === "data_dir" || name === "profile" || /^(server|retention|batch|duckdb|experimental|storage|mcp|rate_limits)\./.test(name);

// Fixtures use the same dotted field names as config.toml. Preserve invalid
// values so the executable, rather than this helper, validates configuration.
export function configSource(settings) {
  return Object.entries(settings).filter(([name]) => field(name)).map(([name, original]) => {
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
  return Object.fromEntries(Object.entries(settings).filter(([name]) => !field(name)
    && (!name.startsWith("CHUNKIFY_") || name === "CHUNKIFY_ADMIN_KEY")));
}

export async function writeConfig(directory, settings) {
  const path = join(directory, "config.toml");
  await writeFile(path, configSource(settings), { mode: 0o600 });
  return path;
}
