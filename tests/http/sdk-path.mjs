import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

// Serve the same published SDK modules that the Node integration tests import.
export const sdkDist = dirname(fileURLToPath(import.meta.resolve("@chunkify/player-analytics")));
