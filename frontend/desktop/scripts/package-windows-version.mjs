import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

export function createTauriVersionConfig(version) {
  if (!version) {
    return {
      arguments: [],
      path: undefined,
      cleanup() {},
    };
  }

  // Inline JSON for --config loses its quotes in cmd.exe, so pass a file instead.
  const directory = mkdtempSync(path.join(tmpdir(), "agentdesktop-tauri-"));
  const configPath = path.join(directory, "release.json");
  writeFileSync(configPath, `${JSON.stringify({ version })}\n`, "utf8");

  return {
    arguments: ["--config", configPath],
    path: configPath,
    cleanup() {
      rmSync(directory, { force: true, recursive: true });
    },
  };
}
