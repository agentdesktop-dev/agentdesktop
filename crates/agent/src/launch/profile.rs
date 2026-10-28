//! Profile data: lists and constants for the Claude Code profile.
//!
//! The profile carries what the launch grants and denies as data; `plan` in
//! the parent module turns it into a [`super::plan::LaunchPlan`], and
//! `integrity_paths` watches the same executable-configuration entries.
//! Request paths are joined with the string `join` (`\`), integrity paths
//! with `Path::join`, so every entry here is a single relative segment unless
//! noted as a `\`-joined home-relative path.

/// The Claude Code profile.
pub(crate) struct ClaudeCodeProfile {
    /// Environment variable names copied from the daemon into the box.
    pub base_env_keys: &'static [&'static str],
    /// Fixed environment variable key-value pairs set for every launch.
    pub fixed_env: &'static [(&'static str, &'static str)],
    /// Home-relative runtime path (`\`-joined) of the client's shared data,
    /// granted read-only when it exists and simply absent otherwise. Order is
    /// part of the request: this entry sits between the executable's
    /// directory and the git installation.
    pub runtime_share: &'static str,
    /// The git user configuration (home-relative). Since PR 4 this is DENIED
    /// (not granted): the box reads the generated per-launch gitconfig via
    /// `GIT_CONFIG_GLOBAL` instead, so the real file (and any PAT in it)
    /// stays out of reach.
    pub git_user_config: &'static str,
    /// The XDG fallback of the git user configuration (`\`-joined), denied
    /// alongside `git_user_config` since PR 4.
    pub git_xdg_config: &'static str,
    /// Executable configuration under `<home>\.claude`: read-only when
    /// present, denied when absent, so the box can neither change nor
    /// create it.
    pub home_executable_config: &'static [&'static str],
    /// Executable configuration at the workspace root, same read-only-or-
    /// denied rule as the home entries.
    pub mcp_json: &'static str,
    /// Executable configuration at the workspace root, same rule: a `HEAD`
    /// file at the workspace root would let git treat the workspace itself
    /// as a bare repository with box-written config.
    pub workspace_head: &'static str,
    /// Executable configuration under `<cwd>\.claude`, same rule; emitted
    /// only when the folder (or one of the entries) exists, because MXC
    /// refuses a denied path whose parent folder does not exist
    /// (lab-verified, 0x80070003). A workspace without `.claude` gets the
    /// folder itself denied instead.
    pub workspace_claude_entries: &'static [&'static str],
    /// Executable configuration under `<cwd>\.git`, emitted whether or not
    /// the entries exist once `.git` is a directory.
    pub git_executable_config: &'static [&'static str],
    /// Home-relative secrets (`\`-joined) denied for every launch, in request
    /// order. The single-segment entries (the top-level home directories)
    /// are also the workspace-validation denied directories.
    pub denied_secrets: &'static [&'static str],
    /// Title of the `cmd.exe start` window.
    pub window_title: &'static str,
}

pub(crate) const CLAUDE_CODE: ClaudeCodeProfile = ClaudeCodeProfile {
    base_env_keys: &[
        "SystemRoot",
        "windir",
        "SystemDrive",
        "ComSpec",
        "PATHEXT",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "ProgramFiles",
        "ProgramData",
        "USERNAME",
    ],
    fixed_env: &[
        ("TERM", "xterm-256color"),
        ("CLAUDE_CODE_USE_POWERSHELL_TOOL", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_AUTH_TOKEN", "placeholder"),
        ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"),
    ],
    runtime_share: r".local\share\claude",
    git_user_config: ".gitconfig",
    git_xdg_config: r".config\git\config",
    home_executable_config: &["settings.json", "hooks", "plugins"],
    mcp_json: ".mcp.json",
    workspace_head: "HEAD",
    workspace_claude_entries: &["settings.json", "settings.local.json", "hooks"],
    git_executable_config: &["config", "hooks", "commondir"],
    denied_secrets: &[
        r".claude\.credentials.json",
        r".claude\history.jsonl",
        // IDE lock files carry auth tokens for the editor bridge.
        r".claude\ide",
        ".ssh",
        ".aws",
        ".azure",
        ".kube",
    ],
    window_title: "agentdesktop: claude",
};

// --- PR 6a: program profiles (`specs/PR-6a-harness-profiles.md`, interface
// contract). Test-first scaffolding: the types the contract names, with the
// table itself left to the writer (`program_profile` is `todo!()`). The writer
// folds `ClaudeCodeProfile` into `ProgramProfile` (D1) and the shared pieces
// into one shared const (D10, G4); field names here may be renamed with a
// ledger line (compile-only test edits).

/// How the relay base URL and the session pairing reach the harness (D2).
#[allow(dead_code)] // test-first scaffolding; the writer wires it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InjectionStrategy {
    /// The relay base URL goes into `base_url`, the pairing header into
    /// `headers` (Claude: `ANTHROPIC_*`; Copilot BYOK: `COPILOT_PROVIDER_*`).
    Env {
        base_url: &'static str,
        headers: &'static str,
    },
    /// The provider configuration, pairing included, goes inline into
    /// `OPENCODE_CONFIG_CONTENT` (D2a).
    OpenCodeInlineConfig,
}

/// The per-harness home under `<home>\.agentdesktop\harness` (D5). Every
/// path below is `\`-joined and relative to `<home>\.agentdesktop\harness`.
#[allow(dead_code)] // test-first scaffolding; the writer wires it
pub(crate) struct HarnessHome {
    /// The program's own directory (`opencode`, `copilot`).
    pub dir: &'static str,
    /// Environment variables pointing into the harness tree (the
    /// "harness-home subdir map"): `XDG_*_HOME` for OpenCode,
    /// `COPILOT_HOME` and `COPILOT_PKG_CACHE_HOME` for Copilot.
    pub env_dirs: &'static [(&'static str, &'static str)],
    /// Executable configuration inside the harness home: read-only when
    /// present, denied when absent, and integrity roots (D5, D12).
    pub executable_config: &'static [&'static str],
}

/// Per-program effective-configuration rules (D11, D1b): `Err` carries the
/// 409 text.
pub(crate) type ProgramPreconditions = fn(
    &agentdesktop_core::config::DaemonConfig,
    agentdesktop_core::config::ContainmentEgress,
) -> Result<(), String>;

/// One launchable program (D1): the generalisation of `ClaudeCodeProfile`.
#[allow(dead_code)] // test-first scaffolding; the writer wires it
pub(crate) struct ProgramProfile {
    /// Program ID, also the session client ID (D11).
    pub program: &'static str,
    /// Native executable file name (D3).
    pub exe_name: &'static str,
    /// Home-relative (`\`-joined) directories searched for `exe_name`
    /// before the daemon `PATH` (D3).
    pub home_candidates: &'static [&'static str],
    /// Relay injection (D2).
    pub injection: InjectionStrategy,
    /// Fixed environment key-value pairs of this program (D8, D10).
    pub fixed_env: &'static [(&'static str, &'static str)],
    /// The harness home; `None` for Claude (which keeps `~\.claude`).
    pub harness_home: Option<HarnessHome>,
    /// Arguments refused when equal to or prefixed by an entry (D6, G9).
    pub denied_args: &'static [&'static str],
    /// Bare tokens refused wherever they appear in the args (G9,
    /// fail-closed: no flag-arity parsing).
    pub denied_tokens: &'static [&'static str],
    /// Title of the `cmd.exe start` window (G5).
    pub window_title: &'static str,
    /// Per-program precondition rules (D11, D1b).
    pub preconditions: ProgramPreconditions,
}

/// The profile of `program` (`claude-code`, `opencode`, `copilot`); `None`
/// for any other program.
#[allow(dead_code)] // test-first scaffolding; the writer wires it
pub(crate) fn program_profile(_program: &str) -> Option<&'static ProgramProfile> {
    todo!("PR 6a: the program profile table (D1)")
}
