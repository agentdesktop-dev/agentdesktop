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

const MACOS_PACKAGE_MODES = {
  // Build the app bundle with the release version; signing happens next.
  "--build-only": {
    buildApplication: true,
    buildPackage: false,
  },
  // Package an already-signed app bundle without rebuilding or rewriting it.
  "--bundle-only": {
    buildApplication: false,
    buildPackage: true,
  },
};

function resolvePackageMode(modes, defaultMode, arguments_) {
  const normalizedArguments = normalizeTauriArguments(arguments_);
  const flags = Object.keys(modes).filter((flag) =>
    normalizedArguments.includes(flag),
  );
  if (flags.length > 1) {
    throw new Error(`${flags.join(" and ")} cannot be combined`);
  }
  const mode = modes[flags[0]] ?? defaultMode;
  return {
    ...mode,
    arguments: normalizedArguments.filter((argument) => !(argument in modes)),
  };
}

export function resolveWindowsPackageMode(arguments_) {
  return resolvePackageMode(
    WINDOWS_PACKAGE_MODES,
    {
      buildService: true,
      command: ["build"],
      requireApplication: false,
    },
    arguments_,
  );
}

export function resolveMacosPackageMode(arguments_) {
  return resolvePackageMode(
    MACOS_PACKAGE_MODES,
    {
      buildApplication: true,
      buildPackage: true,
    },
    arguments_,
  );
}

// WiX only accepts numeric versions, so a pre-release or build-metadata
// version (1.2.3-rc.1) gets an MSI version of its release core (1.2.3).
export function msiVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)([-+].*)?$/.exec(version ?? "");
  if (!match?.[4]) return undefined;
  return `${match[1]}.${match[2]}.${match[3]}`;
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
  const wixVersion = msiVersion(version);
  writeFileSync(
    configPath,
    `version = ${JSON.stringify(version)}\n` +
      (wixVersion
        ? `\n[bundle.windows.wix]\nversion = ${JSON.stringify(wixVersion)}\n`
        : ""),
    "utf8",
  );

  return {
    arguments: ["--config", configPath],
    path: configPath,
    cleanup() {
      rmSync(directory, { force: true, recursive: true });
    },
  };
}
