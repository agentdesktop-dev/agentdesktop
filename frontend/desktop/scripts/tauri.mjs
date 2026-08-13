import { spawnSync } from "node:child_process";
import path from "node:path";

const desktopDirectory = path.resolve(import.meta.dirname, "..");
const nativeDirectory = path.resolve(
  desktopDirectory,
  "../../crates/agentdesktop",
);
const executable = path.join(
  desktopDirectory,
  "node_modules",
  ".bin",
  process.platform === "win32" ? "tauri.cmd" : "tauri",
);
const result = spawnSync(executable, process.argv.slice(2), {
  cwd: nativeDirectory,
  stdio: "inherit",
  shell: process.platform === "win32",
});

if (result.error) throw result.error;
process.exit(result.status ?? 1);
