import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import { macosArchitecture } from "./package-macos-architecture.mjs";
import { normalizeTauriArguments } from "./package-tauri.mjs";

const nativeDirectory = path.resolve(
  import.meta.dirname,
  "../../../crates/agentdesktop",
);
const macosDirectory = path.join(nativeDirectory, "macos");

test("Tauri names the macOS bundle agentdesktop.app", () => {
  const config = JSON.parse(
    readFileSync(path.join(nativeDirectory, "tauri.macos.conf.json"), "utf8"),
  );

  assert.equal(config.productName, "agentdesktop");
});

test("names native and Rust target architectures consistently", () => {
  assert.equal(macosArchitecture(undefined, "arm64"), "arm64");
  assert.equal(macosArchitecture(undefined, "x64"), "amd64");
  assert.equal(macosArchitecture("aarch64-apple-darwin"), "arm64");
  assert.equal(macosArchitecture("x86_64-apple-darwin"), "amd64");
  assert.throws(() => macosArchitecture(undefined, "riscv64"));
});

test("forwards the target option to Tauri instead of Cargo", () => {
  assert.deepEqual(
    normalizeTauriArguments(["--", "--target", "aarch64-apple-darwin"]),
    ["--target", "aarch64-apple-darwin"],
  );
  assert.deepEqual(
    normalizeTauriArguments(["--target", "x86_64-apple-darwin"]),
    ["--target", "x86_64-apple-darwin"],
  );
});

test("package installs the app at a fixed location", () => {
  const component = readFileSync(
    path.join(macosDirectory, "component.plist"),
    "utf8",
  );

  assert.match(component, /<key>BundleIsRelocatable<\/key>\s*<false\/>/);
  assert.match(component, /<key>BundleIsVersionChecked<\/key>\s*<true\/>/);
  assert.match(
    component,
    /<key>BundleOverwriteAction<\/key>\s*<string>upgrade<\/string>/,
  );
  assert.ok(
    component.includes("<string>Applications/agentdesktop.app</string>"),
  );
});

test("LaunchDaemon runs the bundled binary with system paths", () => {
  const plist = readFileSync(
    path.join(macosDirectory, "dev.agentdesktop.daemon.plist"),
    "utf8",
  );

  for (const value of [
    "dev.agentdesktop.daemon",
    "/Applications/agentdesktop.app/Contents/MacOS/agentdesktop",
    "/etc/agentdesktop/config.yaml",
    "/var/lib/agentdesktop",
  ]) {
    assert.ok(plist.includes(`<string>${value}</string>`), value);
  }
  assert.match(plist, /<string>daemon<\/string>/);
  assert.match(plist, /<key>RunAtLoad<\/key>\s*<true\/>/);
  assert.match(plist, /<key>KeepAlive<\/key>\s*<true\/>/);
});

test("installer hooks replace running processes without losing state", () => {
  const preinstall = readFileSync(
    path.join(macosDirectory, "scripts", "preinstall"),
    "utf8",
  );
  const postinstall = readFileSync(
    path.join(macosDirectory, "scripts", "postinstall"),
    "utf8",
  );

  assert.match(preinstall, /launchctl bootout/);
  assert.match(preinstall, /dev\.agentdesktop\.daemon\.user/);
  assert.match(preinstall, /launchctl bootout "gui\/\$\{uid\}/);
  assert.match(preinstall, /application_pids/);
  assert.match(preinstall, /application_user_ids/);
  assert.match(preinstall, /sort -un/);
  assert.match(preinstall, /kill -TERM/);
  assert.match(preinstall, /kill -KILL/);
  assert.match(preinstall, /agentdesktop-relaunch/);
  assert.match(preinstall, /\/Applications\/Agent Desktop\.app/);
  assert.match(preinstall, /dev\.agentdesktop\.tray/);
  assert.match(postinstall, /dseditgroup .* -o create/);
  assert.match(postinstall, /dseditgroup .* -o edit/);
  assert.match(postinstall, /dev\.agentdesktop\.daemon\.user/);
  assert.match(postinstall, /launchctl bootout "gui\/\$\{uid\}/);
  assert.match(postinstall, /if \[ ! -e "\$\{CONFIG_PATH\}" \]/);
  assert.doesNotMatch(postinstall, /rm .*CONFIG_PATH/);
  assert.match(postinstall, /launchctl bootstrap system/);
  assert.match(postinstall, /launchctl kickstart -k/);
  assert.match(postinstall, /agentdesktop-relaunch/);
  assert.match(postinstall, /launchctl asuser/);
  assert.match(postinstall, /sudo -u/);
  assert.match(postinstall, /open -g/);
  assert.match(postinstall, /for uid in \$\{user_ids\}/);
});

const releaseWorkflow = readFileSync(
  path.resolve(nativeDirectory, "../../.github/workflows/release.yml"),
  "utf8",
);
const signingCleanupSteps = [
  ...releaseWorkflow.matchAll(
    /^ {6}- name: Remove macOS signing identity\n((?: {8}[^\n]*\n|\n)*)/gm,
  ),
].map(([, body]) => {
  const run = body.match(/^ {8}run: \|\n([\s\S]*)/m);
  assert.ok(run, "Expected an inline signing cleanup script");
  return { body, script: run[1].replace(/^ {10}/gm, "") };
});

test("both release signing cleanups always run with a short timeout", () => {
  assert.equal(signingCleanupSteps.length, 2);
  for (const { body } of signingCleanupSteps) {
    assert.match(body, /^ {8}if: always\(\)/m);
    const timeout = body.match(/^ {8}timeout-minutes: (\d+)$/m);
    assert.ok(timeout, "Signing cleanup must have a step timeout");
    assert.ok(Number(timeout[1]) > 0 && Number(timeout[1]) <= 2);
    assert.doesNotMatch(body, /continue-on-error: true/);
  }
});

const signingFiles = [
  "agentdesktop-signing.keychain-db",
  "agentdesktop-signing.pem",
  "agentdesktop-signing.p12",
];

function signingCleanupFixture(t, files = signingFiles) {
  const directory = mkdtempSync(path.join(tmpdir(), "agentdesktop signing-"));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  for (const file of [...files, "unrelated-file"]) {
    writeFileSync(path.join(directory, file), "fixture");
  }
  return directory;
}

function runSigningCleanup(script, directory, failKeychainDeletion = false) {
  // Intercept macOS commands before executing the real workflow script.
  // Trust removal fails immediately here instead of hanging on authorization.
  const mocks = `
    sudo() {
      echo "Unexpected privileged trust-settings command" >&2
      echo sudo >> "$RUNNER_TEMP/unexpected-commands"
      return 99
    }
    security() {
      if [[ "$#" != 2 || "$1" != delete-keychain || "$2" != "$RUNNER_TEMP/agentdesktop-signing.keychain-db" ]]; then
        echo "Unexpected security command" >&2
        echo security >> "$RUNNER_TEMP/unexpected-commands"
        return 98
      fi
      printf '%s\\n' "$1" >> "$RUNNER_TEMP/security-calls"
      if [[ "$FAIL_KEYCHAIN_DELETION" == 1 ]]; then
        echo "Simulated keychain deletion failure" >&2
        return 42
      fi
      rm -f "$2"
    }
  `;
  const result = spawnSync(
    "bash",
    ["--noprofile", "--norc", "-e", "-o", "pipefail"],
    {
      input: `${mocks}\n${script}`,
      encoding: "utf8",
      timeout: 5000,
      env: {
        PATH: process.env.PATH,
        RUNNER_TEMP: directory,
        FAIL_KEYCHAIN_DELETION: failKeychainDeletion ? "1" : "0",
      },
    },
  );
  assert.ifError(result.error);
  assert.equal(
    existsSync(path.join(directory, "unexpected-commands")),
    false,
    "Cleanup must not invoke trust settings, even with suppressed errors",
  );
  return result;
}

for (const [index, { script }] of signingCleanupSteps.entries()) {
  test(`signing cleanup ${index + 1} removes only signing material and is repeatable`, (t) => {
    const directory = signingCleanupFixture(t);
    for (let attempt = 0; attempt < 2; attempt++) {
      const result = runSigningCleanup(script, directory);
      assert.equal(result.status, 0, result.stderr);
      for (const file of signingFiles) {
        assert.equal(existsSync(path.join(directory, file)), false, file);
      }
      assert.equal(
        readFileSync(path.join(directory, "unrelated-file"), "utf8"),
        "fixture",
      );
    }
    assert.equal(
      readFileSync(path.join(directory, "security-calls"), "utf8"),
      "delete-keychain\n",
    );
  });

  test(`signing cleanup ${index + 1} handles a partially imported identity`, (t) => {
    const directory = signingCleanupFixture(t, signingFiles.slice(1));
    const result = runSigningCleanup(script, directory);
    assert.equal(result.status, 0, result.stderr);
    for (const file of signingFiles) {
      assert.equal(existsSync(path.join(directory, file)), false, file);
    }
    assert.equal(existsSync(path.join(directory, "security-calls")), false);
  });

  test(`signing cleanup ${index + 1} deletes exported files even when keychain deletion fails`, (t) => {
    const directory = signingCleanupFixture(t);
    const result = runSigningCleanup(script, directory, true);
    assert.equal(result.status, 42, result.stderr);
    for (const file of signingFiles.slice(1)) {
      assert.equal(existsSync(path.join(directory, file)), false, file);
    }
    assert.equal(existsSync(path.join(directory, signingFiles[0])), true);
  });
}
