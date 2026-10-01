import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

export function normalizeTauriArguments(arguments_) {
  return arguments_[0] === "--" ? arguments_.slice(1) : arguments_;
}

const WINDOWS_PACKAGE_MODES = {
  // Compile both executables with the release version; signing happens next.
  "--build-only": {
    buildService: true,
    command: ["build", "--no-bundle"],
    requireApplication: false,
  },
  // Package already-signed executables without rebuilding or rewriting them.
  "--bundle-only": {
    buildService: false,
    command: ["bundle", "--no-binary-patching"],
    requireApplication: true,
  },
};

export function resolveWindowsPackageMode(arguments_) {
  const normalizedArguments = normalizeTauriArguments(arguments_);
  const flags = Object.keys(WINDOWS_PACKAGE_MODES).filter((flag) =>
    normalizedArguments.includes(flag),
  );
  if (flags.length > 1) {
    throw new Error(`${flags.join(" and ")} cannot be combined`);
  }
  const mode = WINDOWS_PACKAGE_MODES[flags[0]] ?? {
    buildService: true,
    command: ["build"],
    requireApplication: false,
  };
  return {
    ...mode,
    arguments: normalizedArguments.filter(
      (argument) => !(argument in WINDOWS_PACKAGE_MODES),
    ),
  };
}

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
