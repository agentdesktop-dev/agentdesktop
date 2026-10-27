import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";

import {
  createTauriVersionConfig,
  resolveWindowsPackageMode,
} from "./package-tauri.mjs";

if (process.platform !== "win32") {
  throw new Error("Windows MSI packages must be built on Windows");
}

const desktopDirectory = path.resolve(import.meta.dirname, "..");
const repositoryDirectory = path.resolve(desktopDirectory, "../..");
const nativeDirectory = path.join(
  repositoryDirectory,
  "crates",
  "agentdesktop",
);
const packageMode = resolveWindowsPackageMode(process.argv.slice(2));
const forwardedArguments = packageMode.arguments;
const targetIndex = forwardedArguments.indexOf("--target");
const target =
  targetIndex === -1 ? undefined : forwardedArguments[targetIndex + 1];
if (targetIndex !== -1 && !target) {
  throw new Error("--target requires a Rust target triple");
}
if (packageMode.buildService) {
  const cargoArguments = [
    "build",
    "--locked",
    "--release",
    "--package",
    "agentdesktop-agent",
    "--bin",
    "agentdesktop-service",
  ];
  if (target) cargoArguments.push("--target", target);

  const serviceBuild = spawnSync("cargo", cargoArguments, {
    cwd: repositoryDirectory,
    stdio: "inherit",
  });
  if (serviceBuild.error) throw serviceBuild.error;
  if (serviceBuild.status !== 0) process.exit(serviceBuild.status ?? 1);
}

const targetDirectory = path.resolve(
  repositoryDirectory,
  process.env.CARGO_TARGET_DIR ?? "target",
);
const releaseDirectory = path.join(
  targetDirectory,
  ...(target ? [target] : []),
  "release",
);
const serviceExecutable = path.join(
  releaseDirectory,
  "agentdesktop-service.exe",
);
if (!existsSync(serviceExecutable)) {
  throw new Error(`Windows service executable not found: ${serviceExecutable}`);
}
if (packageMode.requireApplication) {
  const applicationExecutable = path.join(releaseDirectory, "agentdesktop.exe");
  if (!existsSync(applicationExecutable)) {
    throw new Error(
      `Windows application executable not found: ${applicationExecutable}`,
    );
  }
}
const tauriExecutable = path.join(
  desktopDirectory,
  "node_modules",
  ".bin",
  "tauri.cmd",
);
const versionConfig = createTauriVersionConfig(
  process.env.AGENTDESKTOP_VERSION,
);
const tauriArguments = [
  ...packageMode.command,
  "--bundles",
  "msi",
  ...forwardedArguments,
  ...versionConfig.arguments,
];
let bundle;
try {
  bundle = spawnSync(tauriExecutable, tauriArguments, {
    cwd: nativeDirectory,
    stdio: "inherit",
    // Node refuses to spawn .cmd shims without a shell.
    shell: true,
    env: {
      ...process.env,
      // Tauri only forwards TAURI-prefixed variables to the WiX toolchain.
      TAURI_AGENTDESKTOP_SERVICE_EXE: serviceExecutable,
      TAURI_AGENTDESKTOP_DEFAULT_CONFIG: path.join(
        nativeDirectory,
        "windows",
        "config.yaml",
      ),
    },
  });
} finally {
  versionConfig.cleanup();
}
if (bundle.error) throw bundle.error;
process.exit(bundle.status ?? 1);
