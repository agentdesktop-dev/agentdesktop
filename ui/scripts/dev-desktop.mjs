#!/usr/bin/env node

import { spawn, spawnSync } from "node:child_process";
import { connect } from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const connectorArguments = process.argv.slice(2);
const isWindows = process.platform === "win32";
const connectorPort = 8080;
const uiPort = 1420;
const connectorStatusUrl = `http://127.0.0.1:${connectorPort}/_agentdesktop/status`;
const uiUrl = `http://127.0.0.1:${uiPort}/`;

if (connectorArguments.includes("--help") || connectorArguments.includes("-h")) {
  console.log(`Usage: npm run dev:desktop -- [backend options]

Starts the UI-local development backend and Tauri UI together. Options are
forwarded to the development backend.

Defaults:
  AGENTDESKTOP_MODE=standalone
  Agent Desktop-owned agentgateway on http://127.0.0.1:4100

Set AGENTDESKTOP_GATEWAY_MODE=external to use an independently started Gateway.`);
  process.exit(0);
}

function findExecutable(name) {
  const lookup = spawnSync(isWindows ? "where" : "which", [name], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"]
  });
  return lookup.status === 0 ? lookup.stdout.trim().split(/\r?\n/, 1)[0] : undefined;
}

const gatewayMode = process.env.AGENTDESKTOP_GATEWAY_MODE || "owned";
if (!new Set(["owned", "external"]).has(gatewayMode)) {
  console.error("[desktop] AGENTDESKTOP_GATEWAY_MODE must be owned or external");
  process.exit(1);
}
const ownsGateway = gatewayMode === "owned";
const gatewayBinary = ownsGateway
  ? process.env.AGENTDESKTOP_GATEWAY_BINARY || findExecutable("agentgateway")
  : undefined;
const gatewayConfig = ownsGateway
  ? resolve(
      process.cwd(),
      process.env.AGENTDESKTOP_GATEWAY_CONFIG ||
        resolve(repositoryRoot, "ui/config/agentgateway-anthropic.yaml")
    )
  : undefined;
if (ownsGateway && !gatewayBinary) {
  console.error("[desktop] cannot find agentgateway in PATH");
  console.error("[desktop] install agentgateway or set AGENTDESKTOP_GATEWAY_BINARY");
  process.exit(1);
}
const upstream =
  process.env.AGENTDESKTOP_UPSTREAM ||
  (ownsGateway ? "http://127.0.0.1:4100" : "http://127.0.0.1:4000");
const gatewayUrl = new URL(upstream);
const gatewayPort = Number(gatewayUrl.port || (gatewayUrl.protocol === "https:" ? 443 : 80));
const environment = {
  ...process.env,
  AGENTDESKTOP_MODE: process.env.AGENTDESKTOP_MODE || "standalone",
  AGENTDESKTOP_UPSTREAM: upstream,
  ...(ownsGateway
    ? {
        AGENTDESKTOP_GATEWAY_BINARY: gatewayBinary,
        AGENTDESKTOP_GATEWAY_CONFIG: gatewayConfig
      }
    : {})
};
delete environment.ANTHROPIC_API_KEY;
const children = new Map();
let shuttingDown = false;
let finalExitCode = 0;
let forceTimer;

function isPortOpen(port) {
  return new Promise((resolvePort) => {
    const socket = connect({ host: "127.0.0.1", port });
    const finish = (open) => {
      socket.destroy();
      resolvePort(open);
    };
    socket.setTimeout(500, () => finish(false));
    socket.once("connect", () => finish(true));
    socket.once("error", () => finish(false));
  });
}

async function isAgentDesktopConnector() {
  try {
    const response = await fetch(connectorStatusUrl, {
      signal: AbortSignal.timeout(1000)
    });
    if (!response.ok) return false;
    const status = await response.json();
    return (
      typeof status.version === "string" &&
      typeof status.mode === "string" &&
      typeof status.platform?.native_gateway === "boolean"
    );
  } catch {
    return false;
  }
}

async function isAgentDesktopUi() {
  try {
    const response = await fetch(uiUrl, { signal: AbortSignal.timeout(1000) });
    return response.ok && (await response.text()).includes("<title>Agent Desktop</title>");
  } catch {
    return false;
  }
}

async function preflight() {
  const [connectorOpen, uiOpen, gatewayOpen] = await Promise.all([
    isPortOpen(connectorPort),
    isPortOpen(uiPort),
    ownsGateway ? isPortOpen(gatewayPort) : false
  ]);
  if (!connectorOpen && !uiOpen && !gatewayOpen) return true;

  const [connectorOwned, uiOwned] = await Promise.all([
    connectorOpen ? isAgentDesktopConnector() : false,
    uiOpen ? isAgentDesktopUi() : false
  ]);
  if (connectorOwned && uiOwned) {
    console.log("[desktop] Agent Desktop is already running");
    return false;
  }

  const conflicts = [];
  if (connectorOpen) {
    conflicts.push(
      connectorOwned
        ? `the Agent Desktop connector already uses port ${connectorPort}`
        : `another process uses connector port ${connectorPort}`
    );
  }
  if (uiOpen) {
    conflicts.push(
      uiOwned
        ? `the Agent Desktop UI already uses port ${uiPort}`
        : `another process uses UI port ${uiPort}`
    );
  }
  if (gatewayOpen) {
    conflicts.push(`another process uses the owned Gateway port ${gatewayPort}`);
  }
  console.error(`[desktop] cannot start: ${conflicts.join("; ")}`);
  console.error("[desktop] stop the existing process, then run this command again");
  process.exitCode = 1;
  return false;
}

function terminateProcessTree(child, signal = "SIGTERM") {
  if (!child.pid) return;
  if (isWindows) {
    spawnSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], {
      stdio: "ignore"
    });
    return;
  }
  try {
    process.kill(-child.pid, signal);
  } catch {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  }
}

function finishWhenStopped() {
  if (!shuttingDown || children.size !== 0) return;
  if (forceTimer) clearTimeout(forceTimer);
  process.exitCode = finalExitCode;
}

function beginShutdown(exitCode, signal = "SIGTERM") {
  if (shuttingDown) return;
  shuttingDown = true;
  finalExitCode = exitCode;
  for (const child of children.values()) terminateProcessTree(child, signal);
  forceTimer = setTimeout(() => {
    for (const child of children.values()) terminateProcessTree(child, "SIGKILL");
    process.exit(finalExitCode);
  }, 5000);
  finishWhenStopped();
}

function start(name, command, arguments_) {
  console.log(`[desktop] starting ${name}`);
  const child = spawn(command, arguments_, {
    cwd: repositoryRoot,
    env: environment,
    stdio: "inherit",
    detached: !isWindows
  });
  children.set(name, child);
  child.on("error", (error) => {
    console.error(`[desktop] ${name} could not start: ${error.message}`);
    children.delete(name);
    beginShutdown(1);
    finishWhenStopped();
  });
  child.on("exit", (code, signal) => {
    terminateProcessTree(child);
    children.delete(name);
    if (!shuttingDown) {
      const detail = signal ? `signal ${signal}` : `code ${code ?? 1}`;
      console.error(`[desktop] ${name} exited with ${detail}`);
      beginShutdown(code ?? 1);
    }
    finishWhenStopped();
  });
}

process.once("SIGINT", () => beginShutdown(130, "SIGINT"));
process.once("SIGTERM", () => beginShutdown(143, "SIGTERM"));
if (!isWindows) process.once("SIGHUP", () => beginShutdown(129, "SIGHUP"));
process.once("exit", () => {
  for (const child of children.values()) terminateProcessTree(child, "SIGTERM");
});

console.log(`[desktop] connector mode=${environment.AGENTDESKTOP_MODE}`);
if (await preflight()) {
  start("backend", "cargo", [
    "run",
    "--manifest-path",
    "ui/src-tauri/Cargo.toml",
    "--bin",
    "agentdesktop-dev-backend",
    "--",
    ...connectorArguments
  ]);
  start("ui", isWindows ? "npm.cmd" : "npm", ["--prefix", "ui", "start"]);
}