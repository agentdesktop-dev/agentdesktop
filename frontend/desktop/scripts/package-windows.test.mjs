import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { createTauriVersionConfig } from "./package-windows-version.mjs";

test("writes the Tauri release version to a temporary config file", () => {
  const config = createTauriVersionConfig("0.1.0");

  try {
    assert.deepEqual(config.arguments, ["--config", config.path]);
    assert.deepEqual(JSON.parse(readFileSync(config.path, "utf8")), {
      version: "0.1.0",
    });
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
