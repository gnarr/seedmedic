/**
 * Spawn a real SeedMedic against a throwaway directory.
 *
 * The **built binary**, not a Vite dev server: the point of embedding the bundle
 * is that what ships is what is tested, and a dev server tests a different asset
 * pipeline. `/health` is the readiness signal because it is auth-exempt by design
 * (so it works in the auth-on scenarios too) and because a 200 means the worker
 * has ticked — which removes a whole class of flake.
 */

import { spawn } from "node:child_process";
import { mkdtempSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const BINARY = new URL("../../target/debug/seedmedic", import.meta.url).pathname;

/** Library files sized to match `bootstrap::demo_torrents` exactly. */
const DEMO_LIBRARY = [
  ["Demo.Movie.2024.1080p/movie.mkv", 1 << 20],
  ["Demo.Show.S01.1080p/S01E01.mkv", 2 << 20],
  ["Demo.Show.S01.1080p/S01E02.mkv", 3 << 20],
];

export function scenario({ port, library = "matching", authToken = null, configured = true }) {
  const dir = mkdtempSync(join(tmpdir(), "seedmedic-e2e-"));
  const libraryPath = join(dir, "library");
  const stagingPath = join(dir, "staging");
  mkdirSync(libraryPath, { recursive: true });
  mkdirSync(stagingPath, { recursive: true });

  if (library === "matching") {
    for (const [name, size] of DEMO_LIBRARY) {
      const full = join(libraryPath, name);
      mkdirSync(join(full, ".."), { recursive: true });
      writeFileSync(full, Buffer.alloc(size, 7));
    }
  } else if (library === "ambiguous") {
    // Two files of *exactly* the movie's size whose names do not normalise to
    // `movie`, so `plan_matches` hits (sized = 2, named = 0) and parks on
    // AmbiguousMatch with both candidates recorded — the fixture for the most
    // important screen in the product, from two writeFileSync calls.
    mkdirSync(join(libraryPath, "Demo.Movie.2024.1080p"), { recursive: true });
    writeFileSync(join(libraryPath, "Demo.Movie.2024.1080p", "aaa.mkv"), Buffer.alloc(1 << 20, 1));
    writeFileSync(join(libraryPath, "Demo.Movie.2024.1080p", "bbb.mkv"), Buffer.alloc(1 << 20, 2));
  }

  const config = configured
    ? `
[server]
bind_address = "127.0.0.1:${port}"
${authToken ? `auth_token = "${authToken}"` : ""}

[database]
path = "${join(dir, "seedmedic.db")}"

[staging]
root = "${stagingPath}"

[library]
roots = ["${libraryPath}"]

[worker]
owner = "e2e"
poll_interval_seconds = 1
discovery_interval_seconds = 2

[[trackers]]
id = "demo"
kind = "fake"

[download_client]
kind = "fake"
`
    : `
[server]
bind_address = "127.0.0.1:${port}"

[database]
path = "${join(dir, "seedmedic.db")}"
`;

  const configPath = join(dir, "config.toml");
  writeFileSync(configPath, config);
  return { dir, configPath, port };
}

export async function start(scenarioConfig) {
  const child = spawn(BINARY, [], {
    env: { ...process.env, SEEDMEDIC_CONFIG: scenarioConfig.configPath, RUST_LOG: "warn" },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const base = `http://127.0.0.1:${scenarioConfig.port}`;
  for (let attempt = 0; attempt < 120; attempt += 1) {
    try {
      const response = await fetch(`${base}/health`);
      if (response.ok) return { child, base };
    } catch {
      // not listening yet
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  child.kill("SIGKILL");
  throw new Error(`SeedMedic did not become ready on ${base}`);
}

export function stop(handle) {
  handle?.child.kill("SIGTERM");
}
