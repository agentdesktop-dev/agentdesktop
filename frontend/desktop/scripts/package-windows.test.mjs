import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  createTauriVersionConfig,
  resolveWindowsPackageMode,
} from "./package-tauri.mjs";

test("keeps a stable MSI upgrade identity", () => {
  const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
  const config = JSON.parse(
    readFileSync(
      path.resolve(
        scriptDirectory,
        "../../../crates/agentdesktop/tauri.windows.conf.json",
      ),
      "utf8",
    ),
  );

  assert.equal(config.bundle.windows.allowDowngrades, false);
  assert.equal(
    config.bundle.windows.wix.upgradeCode,
    "b90e038c-7777-4aa6-ab02-9675fe051e83",
  );
});

test("writes the Tauri release version to a temporary config file", () => {
  const config = createTauriVersionConfig("0.1.0");

  try {
    assert.deepEqual(config.arguments, ["--config", config.path]);
    assert.equal(path.extname(config.path), ".toml");
    assert.equal(readFileSync(config.path, "utf8"), 'version = "0.1.0"\n');
    assert.equal(
      config.arguments.some((argument) => argument.includes("{")),
      false,
    );
  } finally {
    config.cleanup();
  }

  assert.equal(existsSync(config.path), false);
});

test("build-only compiles both executables without bundling", () => {
  const mode = resolveWindowsPackageMode([
    "--",
    "--build-only",
    "--target",
    "aarch64-pc-windows-msvc",
  ]);

  assert.equal(mode.buildService, true);
  assert.equal(mode.requireApplication, false);
  assert.deepEqual(mode.command, ["build", "--no-bundle"]);
  assert.deepEqual(mode.arguments, ["--target", "aarch64-pc-windows-msvc"]);
});

test("bundle-only packages signed executables without rewriting them", () => {
  const mode = resolveWindowsPackageMode([
    "--",
    "--bundle-only",
    "--target",
    "aarch64-pc-windows-msvc",
  ]);

  assert.equal(mode.buildService, false);
  assert.equal(mode.requireApplication, true);
  assert.deepEqual(mode.command, ["bundle", "--no-binary-patching"]);
  assert.deepEqual(mode.arguments, ["--target", "aarch64-pc-windows-msvc"]);
});

test("default mode keeps the local build-and-bundle behavior", () => {
  const mode = resolveWindowsPackageMode([
    "--target",
    "x86_64-pc-windows-msvc",
  ]);

  assert.equal(mode.buildService, true);
  assert.equal(mode.requireApplication, false);
  assert.deepEqual(mode.command, ["build"]);
  assert.deepEqual(mode.arguments, ["--target", "x86_64-pc-windows-msvc"]);
});

test("rejects combining build-only with bundle-only", () => {
  assert.throws(
    () => resolveWindowsPackageMode(["--build-only", "--bundle-only"]),
    /cannot be combined/,
  );
});

test("uses Tauri-preserved environment variables in the WiX fragment", () => {
  const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
  const fragmentPath = path.resolve(
    scriptDirectory,
    "../../../crates/agentdesktop/windows/installer.wxs",
  );
  const fragment = readFileSync(fragmentPath, "utf8");
  const environmentVariables = [
    ...fragment.matchAll(/\$\(env\.([A-Z0-9_]+)\)/g),
  ].map((match) => match[1]);

  assert.ok(environmentVariables.length > 0);
  assert.ok(
    environmentVariables.every((name) => name.startsWith("TAURI")),
    `Tauri removes non-TAURI variables before running WiX: ${environmentVariables.join(", ")}`,
  );
});

test("MSI closes the tray app and restarts the service during upgrades", () => {
  const scriptDirectory = path.dirname(fileURLToPath(import.meta.url));
  const fragmentPath = path.resolve(
    scriptDirectory,
    "../../../crates/agentdesktop/windows/installer.wxs",
  );
  const fragment = readFileSync(fragmentPath, "utf8");

  assert.match(fragment, /xmlns:util=.*UtilExtension/);
  assert.match(fragment, /<util:CloseApplication/);
  assert.match(fragment, /Target="agentdesktop\.exe"/);
  assert.match(fragment, /CloseMessage="yes"/);
  assert.match(fragment, /ElevatedCloseMessage="yes"/);
  assert.match(fragment, /Timeout="15"/);
  assert.match(fragment, /TerminateProcess="1"/);
  assert.match(fragment, /RebootPrompt="no"/);
  assert.match(fragment, /<ServiceControl/);
  assert.match(fragment, /Start="install"/);
  assert.match(fragment, /Stop="both"/);
  assert.match(fragment, /Wait="yes"/);
});
