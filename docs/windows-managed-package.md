# Windows managed package: Claude Code in wrap mode

This guide describes how a Windows fleet makes `claude` launch into an
agentdesktop containment box by default. It covers building the managed
package from the recipe in `scripts/windows-package/`, what must already be
on each device, deploying, migrating devices that carry a per-user install,
and what the resulting posture in daemon-info means.

Wrap mode is a transparency and posture layer, not an execution-plane
security boundary. A determined user can still run the product natively; what
holds then are the gateway identity and network egress controls, and native
runs of the managed binary become visible in the device's posture.

## What the package delivers

The MSI your organization builds (the agentdesktop project never distributes
the vendor binary or a built MSI) lays down:

- `%ProgramFiles%\Anthropic\ClaudeCode\claude.exe`: the fetched and verified
  vendor binary, admin-locked by the standard `%ProgramFiles%` ACL and not on
  PATH. Beside it, `agentdesktop-package.json` records the packaged version;
  agentdesktop discovery reads the version from this file and never runs the
  binary to ask. On Windows, discovery considers `.exe` executables only, so
  the package's own extensionless launcher is never reported as Claude Code;
  the accepted trade-off is that an npm-installed `claude.cmd` shim is not
  inventoried either, on any device, packaged or not (npm shims were already
  refused for launching; the planned name-based unmanaged-instances counter
  is the follow-up that would surface them). This directory is intentionally not
  `%ProgramFiles%\ClaudeCode`: that directory is Claude Code's
  managed-settings root and is granted read-only into Claude boxes.
- `%ProgramFiles%\ClaudeCode\managed-settings.d\40-agentdesktop-package.json`:
  a managed-settings drop-in holding `env.DISABLE_AUTOUPDATER` and
  `env.DISABLE_UPDATES`. `DISABLE_AUTOUPDATER` alone only stops the
  background check; a user could still recreate a user-writable copy with
  `claude update` or `claude install`, which `DISABLE_UPDATES` closes. The
  package never writes `managed-settings.json` or `50-agentdesktop.json`;
  those belong to the agentdesktop system daemon.
- `%ProgramFiles%\Agent Desktop\launchers\` (the agentdesktop product MSI's
  own install directory): two launcher scripts per wrapped program,
  `claude.cmd` (cmd and PowerShell) and an extensionless `claude` POSIX
  script (Git Bash/MSYS), with the launcher directory prepended to the
  machine PATH. Wrap mode is accepted in the policy for `openCode` and
  `copilot` too, but this v1 package ships launchers for Claude Code only;
  the other program packages are a follow-up, and a wrap policy naming them
  reports `aliasesPresent: false` until one exists. This PATH prepend, through the standard MSI Environment
  table, is the package's only HKLM write; it installs no policy keys and no
  AutoRun keys, and uninstalling removes the PATH entry again.

Because the machine PATH precedes every user PATH entry, and both cmd and
PowerShell walk PATH directories in order, the launchers win name resolution
for `claude` in cmd, PowerShell 5.1 and 7 (including `-NoProfile`), Git Bash
(interactive and `bash script.sh`), and the `cmd /c` children build tools
spawn. There are no shell-profile edits and no PowerShell execution-policy
prerequisite: a `.cmd` file is not a PowerShell script and runs under the
`Restricted` policy. Managing the execution policy remains a reasonable
hardening measure, it is just not required for wrapping. PowerShell 7's
external-command resolution is expected to behave like the measured 5.1
directory-major search, but it is not yet measured (pwsh was absent on the
lab VM).

Environment pins that point the product at your LLM gateway are fleet
specific and are not part of the committed drop-in template; add them to the
drop-in at package build time or deliver them through your existing
managed-settings channel. One measured limit applies to every `env` entry in
managed settings: the managed value is applied only when the process
environment does not already set that variable, so a variable exported in the
user's own environment wins. Treat the pins as deployed-default hygiene; the
hard stops for a deliberate bypass are gateway identity and network egress.

Fleets that deliver Claude Code managed settings through the HKLM policy key
instead of files should put the same keys (`DISABLE_AUTOUPDATER`,
`DISABLE_UPDATES`, and any pins) into the HKLM policy tier: HKLM wins over
the file tier, so a conflicting file entry would not take effect there
anyway.

## Prerequisites on every device

1. **Daemon versions and upgrade order.** The `containment` policy block is
   new, and daemon configs reject unknown fields. Upgrade the controller
   first (it refuses a policy it cannot parse at authoring time), then every
   daemon (system and user), and push the policy naming the program last. A
   pre-upgrade daemon that receives a config carrying `containment` rejects
   the whole config, keeps its last accepted one, and reports a config error.
2. **A containment policy in wrap mode**, controller-delivered, or in the
   local config file on controller-less devices (local-file changes are read
   at daemon startup only):

   ```yaml
   containment:
     programs:
       claudeCode:
         executable: 'C:\Program Files\Anthropic\ClaudeCode\claude.exe'
   ```

   `mode` defaults to `wrap` when the policy names a program; `mode: native`
   is the explicit per-program opt-out. In wrap mode only the configured
   `executable` is ever launched; `~\.local\bin` and PATH are not searched.
3. **The local containment startup config**, MDM-written into the device's
   local config file, naming the launch executor:

   ```yaml
   daemon:
     containment:
       executor: 'C:\Program Files\mxc\wxc-exec.exe'
   ```

   Without it the device reports no wrap posture, logs a warning at startup
   and on each applied config, and the launchers fail with the daemon's
   refusal naming `daemon.containment.executor`.
4. **The executor binary itself.** `wxc-exec.exe` is required and is never
   part of this package. Obtain it from the Microsoft-signed
   `mxc-release-binaries.zip` attached to the microsoft/mxc GitHub releases
   (the v1.0.1 zip carries `x64/wxc-exec.exe` and `arm64/wxc-exec.exe`), or
   build it from source (MIT licensed, `build.bat --all`). Deliver it to the
   configured path with your deployment tooling.
5. **A per-user daemon on a per-user pipe.** The launchers connect to
   `\\.\pipe\agentdesktop-user-<USERNAME>`, expanded at call time, so the
   name does not collide across sessions on multi-user and RDS hosts. Start
   the user daemon at logon through a scheduled task that passes the same
   pipe name, for example:

   ```powershell
   # cmd expands %USERNAME% at start; Task Scheduler itself does not expand
   # environment variables in action arguments. The pipe name is quoted: a
   # local account name may contain a space.
   $action = New-ScheduledTaskAction -Execute 'cmd.exe' -Argument `
       '/c ""C:\Program Files\Agent Desktop\agentdesktop.exe" --socket "\\.\pipe\agentdesktop-user-%USERNAME%" daemon --user"'
   $trigger = New-ScheduledTaskTrigger -AtLogOn
   Register-ScheduledTask -TaskName 'agentdesktop user daemon' -Action $action -Trigger $trigger
   ```

   The pipe's access control admits SYSTEM and the daemon's own user only,
   so another local user's processes cannot reach a running daemon. The pipe
   NAME is still predictable, so before sending any request the client
   additionally reads the pipe object's owner and refuses with one line
   unless the pipe is owned by the same user or by SYSTEM (the system
   daemon): a pipe created by another local user under this user's name is
   owned by that user and captures nothing. The owner read deliberately
   avoids opening the server process or its token, which a standard user
   cannot do against a SYSTEM daemon.

## Building the package

On a build machine with the WiX v4 CLI, pinned to the version the build
script requires (`dotnet tool install --global wix --version 4.0.5`; the
script refuses any other version so package builds stay reproducible):

```powershell
cd scripts\windows-package
.\build-package.ps1 -ClaudeCodeVersion 2.1.98 `
    -UpgradeCode '<the GUID your organization generated once and keeps>' `
    -Manufacturer 'Example Corp' `
    -AgentdesktopInstallPath 'C:\Program Files\Agent Desktop' `
    -ExecutorPath 'C:\Program Files\mxc\wxc-exec.exe'
```

The script fetches the pinned version from Anthropic's official release
bucket (the same URL the `Anthropic.ClaudeCode` winget manifest points at,
so winget is the feed, never the installer), then verifies before packaging:
the binary's SHA256 against the release `manifest.json`, the Authenticode
signature (an exact signer subject CN comparison against `Anthropic, PBC`),
and, when you pass the vendor's GPG public key, the manifest's detached
signature. Without the GPG key, the SHA256 comes from a manifest fetched
from the same origin as the binary, so it proves transport integrity, not
independent authenticity; the Authenticode check is then the authenticity
anchor. It renders the launchers and the drop-in from `templates/` with one
substitution, the agentdesktop install path, writes the
`agentdesktop-package.json` version manifest, and builds the MSI. It refuses
a non-empty output directory and an install path the launchers could not
carry safely (relative or UNC, or containing `" % ! $`, a backtick or a
newline), and it warns, but builds anyway, when the executor path is absent
on the build machine. No vendor bytes ever enter this repository; the fetch
and the built MSI stay on your infrastructure.

Keep `-AgentdesktopInstallPath` accurate for your fleet: the launchers call
`agentdesktop.exe` by that absolute path and never search PATH for it.

To ship a launcher or recipe fix for the same Claude Code version, bump
`-PackageRevision` (the MSI version's fourth field, default 0): the package
allows same-version upgrades, so the rebuilt MSI replaces the installed one
in place under the unchanged UpgradeCode.

**Why this recipe lives in-tree.** The launchers' guard line, pipe name and
argument forwarding must stay byte-identical to what the daemon's golden
tests pin (`crates/agent/src/packaging.rs`), and the wrap posture's
`aliasesPresent` check names the exact launcher payload paths the MSI lays
down; a recipe maintained outside this repository would drift from both.
The directory carries templates, one `.wxs` and one script, no vendor bytes
and no build output. What CI should cover: the Rust render-vs-golden tests
(already in the test suite) on every platform, and, on a Windows runner, a
`build-package.ps1` run against a locally supplied binary stub
(`-ClaudeExePath`) so template rendering, path validation and the pinned
WiX build stay exercised; the real MSI is always the customer's build.
Fleets with an established packaging toolchain (PSADT, Advanced Installer)
can reproduce the same layout from the rendered templates instead of the
WiX project: the file layout and the verification steps are the contract,
the `.wxs` is one implementation of it.

## Deploying and migrating

Install with your usual tooling, for example
`msiexec /i claude-code-managed-2.1.98-x64.msi /qn`.

**Migration step, required where the per-user installer ever ran.** The
native installer puts a user-writable `claude.exe` into `~\.local\bin` and
adds that directory to the user PATH. The launchers still precede it for
`claude` by name, but a copy on the user PATH is reachable as `claude.exe`
(typed with the extension) and by programmatic spawns, and it is invisible to
the posture counters, which watch the managed path only. Remove it with a
remediation script in user context (per-user state is not the MSI's job).
Intune detection:

```powershell
$dir = Join-Path $env:USERPROFILE '.local\bin'
$onPath = ([Environment]::GetEnvironmentVariable('Path', 'User') -split ';') |
    Where-Object { $_ -and ($_.TrimEnd('\') -ieq $dir.TrimEnd('\')) }
if ((Test-Path (Join-Path $dir 'claude.exe')) -or $onPath) { exit 1 } else { exit 0 }
```

Remediation:

```powershell
$dir = Join-Path $env:USERPROFILE '.local\bin'
Remove-Item (Join-Path $dir 'claude.exe') -Force -ErrorAction SilentlyContinue
$entries = ([Environment]::GetEnvironmentVariable('Path', 'User') -split ';') |
    Where-Object { $_ -and ($_.TrimEnd('\') -ine $dir.TrimEnd('\')) }
[Environment]::SetEnvironmentVariable('Path', ($entries -join ';'), 'User')
```

**Behavior change to announce.** Scripts and jobs that invoked a native
`claude` now launch into the box; that is the policy intent. The per-program
`mode: native` policy entry is the opt-out for a fleet that needs one, with
the caveat below.

### `mode: native` with the package installed

The launchers are unconditional files on the machine PATH: they do not read
the policy, so `claude` keeps going through the daemon in native mode too.
Native mode changes what the daemon launches, not whether the launcher runs:
the daemon then resolves the program the pre-package way (`~\.local\bin` and
the daemon PATH), and it never searches
`%ProgramFiles%\Anthropic\ClaudeCode`, which this package deliberately keeps
off PATH. Concretely, on a device with the package installed and the
migration applied, `mode: native` makes every `claude` call fail with the
daemon's 400 not-found; where a user-profile copy still exists, that copy is
what gets launched (boxed). A fleet that wants plain native `claude` back
must uninstall the package (or remove the launchers), not just set
`mode: native`; the genuine binary then needs to be at a discoverable
location again.

### No-MSI alternative: winget machine scope

`winget install Anthropic.ClaudeCode --scope machine` places the portable
binary under the admin-locked `%PROGRAMFILES%\WinGet\Packages` root, which
covers the admin-locked requirement without building a package. Caveats: the
binary stays reachable through a PATH shim (the prepended launcher directory
still precedes it for `claude` by name, but `claude.exe` typed with the
extension reaches it); the exact machine-scope Links directory and PATH
mechanics are not verified here, check them on a reference device; winget-cli
issue #6200 tracks a package-collision problem around `claude.exe`; Intune
cannot deploy community winget packages natively, so a Win32 wrapper script
is needed anyway; and this path installs neither the launchers, the drop-in,
nor the version manifest, so wrapping, updater-off, and discovery's version
reporting all still need separate delivery.

## What a wrapped launch does

The launcher refuses with one stderr line when invoked from inside a box
(every Claude box sets `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST`, and the in-box
PATH excludes the launcher directory, so in-box `claude` resolves to the
program directory). Otherwise it executes

```
agentdesktop.exe run --wait --socket "\\.\pipe\agentdesktop-user-<USERNAME>" claude -- <args>
```

from the caller's working directory. The boxed program opens in its own
console window; same-terminal hosting is not available. Notes:

- **Exit code**: `--wait` returns the launch's recorded exit code, which is
  the EXECUTOR's exit code for the boxed session. It does not carry the
  program's own exit code (measured: a boxed `claude` that itself exited 1
  ended as executor exit 0). Scripts must not branch on the wrapped command's
  exit code to detect program failures. Program-exit-code fidelity is a
  planned launch-chain follow-up.
- **Failures** produce exactly one stderr line and exit code 1: daemon not
  running or unreachable (fail-closed, nothing native is started; no launcher
  contains the vendor binary's path, so no failure mode can fall back to a
  native run), a refused launch request, or a launch evicted or lost to a
  daemon restart while waiting.
- **Ctrl+C** stops the waiting launcher only; the boxed program keeps running
  in its window.
- **No wait timeout**: `--wait` polls until the launch ends and has no
  timeout of its own; a calling script that must bound the wait needs its
  own. The daemon's launch list is bounded and running launches are never
  evicted; only an exited record can be dropped, when more launches exit
  than the list keeps before the waiter's next poll. The exit code is then
  lost: the wait ends with the one-line "no longer known to the daemon"
  message and exit 1, never with a native fallback.
- **Workspace rules**: the working directory must be a real workspace. The
  home directory itself, drive and share roots, AppData, system directories
  and the daemon's own directories are refused; `cd` into a project first.
- **Argument rules**: arguments containing `" % ^ & | < > !` or control
  characters are refused. Arguments beginning with `/` (for example
  `claude -p "/help"`) pass through unchanged from Git Bash; the launcher
  disables MSYS path conversion for the call. From cmd specifically, the
  launcher forwards the arguments inline (`%*`), so a balanced-quoted
  argument (a quoted prompt containing `&`, say) arrives intact and is
  refused by the daemon. The residual is cmd's own parsing: a metacharacter
  that reaches the launcher line unquoted (for example from an escaped `^&`)
  is re-parsed by cmd before the refusal can see it, exactly as it would be
  for any `.cmd` on PATH; the launcher neither improves nor worsens cmd's
  parsing. Pass such arguments from PowerShell or Git Bash.
- **Batch files**: invoking `claude` from another `.cmd`/`.bat` without
  `CALL` ends the calling batch when the launcher returns, the standard cmd
  behavior for batch-to-batch invocation. Use `CALL claude ...` in batch
  scripts.

## Posture: what the daemon reports

A Windows user-mode daemon with `daemon.containment` configured reports, in
its daemon-info under `containment.programs.<program>` for each wrap-mode
program of the applied policy:

- `mode` and `executable`: the applied policy values.
- `aliasesPresent`: true when every launcher payload file for the program
  exists under `%ProgramFiles%\Agent Desktop\launchers` and the launcher
  directory appears in the machine PATH value (the HKLM Session Manager
  `Path`). It does not prove PATH order inside any given live session, where
  a user may have prepended their own entries.
- `violations` and `lastViolationUnixSeconds`: the count of distinct native
  processes of the managed `executable` (same path, not in an AppContainer,
  running as the daemon's user) observed since daemon start, and when the
  last one was seen. The scan runs every 30 seconds in the user-mode daemon
  only; it reports and never terminates anything. The counter resets when
  the daemon restarts, a run shorter than one scan interval may never be
  sampled, and copies of the binary at other paths are not counted (a
  name-based unmanaged-instances counter is a planned follow-up). Two
  further residuals: an ELEVATED native run is invisible to the scan (the
  user-mode daemon cannot read an elevated process's identity, and an
  unreadable identity never counts, by the fail-closed rule), and with a
  policy `executable` spelled through a mapped drive, starts through the
  drive letter and through the UNC path are both counted (measured on a loopback share; a non-loopback share is not yet measured), while a
  start through the same file's local path is not.

One residual applies to the SYSTEM daemon's pipe, not to the wrap launchers
(which use the per-user pipe): the system pipe grants interactive users
generic read/write, and the generic-write grant necessarily includes the
create-pipe-instance right (they share an access bit), so an interactive user can
add a server instance to the existing pipe that inherits its SYSTEM-owned
security descriptor and passes the client's owner check (demonstrated in the
lab: a standard user's added instance received the client's health requests);
this includes any interactive user, RDP sessions among them. Closing it needs two changes
together: the client opens the pipe with an explicit access mask instead of
generic rights, and the interactive-user grant drops the instance-creation
bit. That lands as a follow-up, to be verified in the lab. On single-user
devices this is unreachable; the user-mode pipe grants only SYSTEM and the
daemon user, so it is unaffected.

The posture is device-local: it is visible in the device's own daemon-info,
not at the controller. A fleet can consume it with Intune custom compliance
reading the device's daemon-info and feeding Conditional Access.

A device that receives the `containment` policy but lacks the local
`daemon.containment` startup config reports no posture, warns in its log,
and refuses launches as described under prerequisites.

## IDE and app surfaces

- **JetBrains plugin**: it bundles no CLI; it runs `claude` in the IDE's
  integrated terminal and fails visibly when the command is missing. Its
  "Claude command" setting takes an explicit path: set it to the managed
  binary, `C:\Program Files\Anthropic\ClaudeCode\claude.exe`. Such runs are
  native, so they read the managed settings and count in the posture's
  violation counter; terminal-typed `claude` in the IDE goes through the
  launchers like any other shell.
- **VS Code extension**: the extension bundles its own copy of the CLI for
  the chat panel and does not use PATH, so panel sessions are neither wrapped
  nor visible to the posture counters. They are still governed by the
  machine-path managed settings (the drop-in and HKLM policy tiers), gateway
  identity, and egress. The estate lever is the VS Code `AllowedExtensions`
  enterprise policy (HKLM, ADMX/Intune, VS Code 1.96 and later), which can
  allowlist or block `anthropic.claude-code` fleet-wide; routing panel
  sessions through agentdesktop via the extension's `claudeProcessWrapper`
  setting is a planned follow-up that depends on same-terminal hosting.
- **Claude Desktop app**: a separate application with its own install
  channel. Whether it is deployed at all is an estate application-control
  decision outside this package.
- **Remaining native paths**: `claude.exe` typed with the extension, and
  programmatic spawns that search only `.exe`, reach a native copy where one
  exists; the migration step and `DISABLE_UPDATES` are what keep such copies
  from existing or returning. Prepending to `$env:PATH` inside a session is
  a deliberate bypass; posture stays report-only there, and gateway identity
  plus egress remain the enforcement planes.
