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

  // Inline configuration loses its quotes in cmd.exe, so pass a TOML file.
  // Tauri parses file-based `--config` overrides as TOML.
  const directory = mkdtempSync(path.join(tmpdir(), "agentdesktop-tauri-"));
  const configPath = path.join(directory, "release.toml");
  writeFileSync(configPath, `version = ${JSON.stringify(version)}\n`, "utf8");

  return {
    arguments: ["--config", configPath],
    path: configPath,
    cleanup() {
      rmSync(directory, { force: true, recursive: true });
    },
  };
}
