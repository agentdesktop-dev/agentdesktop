import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";

import { macosArchitecture } from "./package-macos-architecture.mjs";

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

test("package installs the app at a fixed location", () => {
  const component = readFileSync(
    path.join(macosDirectory, "component.plist"),
    "utf8",
  );

  assert.match(component, /<key>BundleIsRelocatable<\/key>\s*<false\/>/);
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

test("installer hooks preserve configuration and manage daemon access", () => {
  const preinstall = readFileSync(
    path.join(macosDirectory, "scripts", "preinstall"),
    "utf8",
  );
  const postinstall = readFileSync(
    path.join(macosDirectory, "scripts", "postinstall"),
    "utf8",
  );

  assert.match(preinstall, /launchctl bootout/);
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
});
