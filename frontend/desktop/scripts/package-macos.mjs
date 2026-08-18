import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { macosArchitecture } from "./package-macos-architecture.mjs";
import { createTauriVersionConfig } from "./package-windows-version.mjs";

if (process.platform !== "darwin") {
  throw new Error("macOS installer packages must be built on macOS");
}

const desktopDirectory = path.resolve(import.meta.dirname, "..");
const repositoryDirectory = path.resolve(desktopDirectory, "../..");
const nativeDirectory = path.join(
  repositoryDirectory,
  "crates",
  "agentdesktop",
);
const macosDirectory = path.join(nativeDirectory, "macos");
const forwardedArguments = process.argv.slice(2);
const targetIndex = forwardedArguments.indexOf("--target");
const target =
  targetIndex === -1 ? undefined : forwardedArguments[targetIndex + 1];
if (targetIndex !== -1 && !target) {
  throw new Error("--target requires a Rust target triple");
}

function run(command, arguments_, options = {}) {
  const result = spawnSync(command, arguments_, {
    stdio: "inherit",
    ...options,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(
      `${path.basename(command)} exited with status ${result.status}`,
    );
  }
}

const configuredVersion = JSON.parse(
  readFileSync(path.join(nativeDirectory, "tauri.conf.json"), "utf8"),
).version;
const version = process.env.AGENTDESKTOP_VERSION ?? configuredVersion;
const versionConfig = createTauriVersionConfig(
  process.env.AGENTDESKTOP_VERSION,
);
const tauriExecutable = path.join(
  desktopDirectory,
  "node_modules",
  ".bin",
  "tauri",
);

try {
  run(
    tauriExecutable,
    [
      "build",
      "--bundles",
      "app",
      ...forwardedArguments,
      ...versionConfig.arguments,
    ],
    { cwd: nativeDirectory },
  );
} finally {
  versionConfig.cleanup();
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
const applicationPath = path.join(
  releaseDirectory,
  "bundle",
  "macos",
  "agentdesktop.app",
);
if (!existsSync(applicationPath)) {
  throw new Error(`macOS application bundle not found: ${applicationPath}`);
}

const architecture = macosArchitecture(target);
const packageDirectory = path.join(releaseDirectory, "bundle", "pkg");
const packagePath = path.join(
  packageDirectory,
  `Agent Desktop_${version}_${architecture}.pkg`,
);
const stagingDirectory = mkdtempSync(path.join(tmpdir(), "agentdesktop-pkg-"));
const payloadRoot = path.join(stagingDirectory, "root");
const installerScriptsDirectory = path.join(stagingDirectory, "scripts");

try {
  const applicationsDirectory = path.join(payloadRoot, "Applications");
  const launchDaemonsDirectory = path.join(
    payloadRoot,
    "Library",
    "LaunchDaemons",
  );
  mkdirSync(applicationsDirectory, { recursive: true });
  mkdirSync(launchDaemonsDirectory, { recursive: true });
  run("/usr/bin/ditto", [
    applicationPath,
    path.join(applicationsDirectory, "agentdesktop.app"),
  ]);

  const launchDaemonPath = path.join(
    launchDaemonsDirectory,
    "dev.agentdesktop.daemon.plist",
  );
  copyFileSync(
    path.join(macosDirectory, "dev.agentdesktop.daemon.plist"),
    launchDaemonPath,
  );
  chmodSync(launchDaemonPath, 0o644);

  mkdirSync(installerScriptsDirectory);
  for (const script of ["preinstall", "postinstall"]) {
    const stagedScript = path.join(installerScriptsDirectory, script);
    copyFileSync(path.join(macosDirectory, "scripts", script), stagedScript);
    chmodSync(stagedScript, 0o755);
  }

  mkdirSync(packageDirectory, { recursive: true });
  rmSync(packagePath, { force: true });
  const packageArguments = [
    "--root",
    payloadRoot,
    "--component-plist",
    path.join(macosDirectory, "component.plist"),
    "--scripts",
    installerScriptsDirectory,
    "--identifier",
    "dev.agentdesktop.installer",
    "--version",
    version,
    "--install-location",
    "/",
    "--ownership",
    "recommended",
  ];
  if (process.env.APPLE_INSTALLER_SIGNING_IDENTITY) {
    packageArguments.push(
      "--sign",
      process.env.APPLE_INSTALLER_SIGNING_IDENTITY,
    );
    if (process.env.APPLE_INSTALLER_KEYCHAIN) {
      packageArguments.push("--keychain", process.env.APPLE_INSTALLER_KEYCHAIN);
    }
  }
  packageArguments.push(packagePath);
  run("/usr/bin/pkgbuild", packageArguments);
} finally {
  rmSync(stagingDirectory, { force: true, recursive: true });
}

console.log(`Created ${packagePath}`);
