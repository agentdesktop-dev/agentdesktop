import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { createTauriVersionConfig } from "./package-windows-version.mjs";

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
