import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

// Node sizes its heap from the host. Use a smaller budget so transient HTTP
// allocations are collected before the benchmark's separate 1 GiB RSS guard.
const child = spawn(process.execPath, [
  "--max-old-space-size=512", "--max-semi-space-size=64",
  ...process.execArgv.filter((arg) => arg === "--expose-gc"),
  fileURLToPath(new URL("./load-worker.mjs", import.meta.url)), ...process.argv.slice(2),
], { stdio: "inherit" });

const interrupt = () => child.kill("SIGINT");
const terminate = () => child.kill("SIGTERM");
process.on("SIGINT", interrupt);
process.on("SIGTERM", terminate);
function finish(code) {
  process.off("SIGINT", interrupt);
  process.off("SIGTERM", terminate);
  process.exitCode = code;
}
child.once("error", (error) => { console.error(error.message); finish(1); });
child.once("exit", (code, signal) => finish(code ?? (signal === "SIGINT" ? 130 : signal === "SIGTERM" ? 143 : 1)));
