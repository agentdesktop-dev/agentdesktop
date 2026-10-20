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
    /// The git user configuration (home-relative), granted read-only when it
    /// exists; comes after the discovered directories (git reads the user's
    /// configuration before anything else; unreadable means every git
    /// command fails in the box, lab L4e).
    pub git_user_config: &'static str,
    /// The XDG fallback of the git user configuration (`\`-joined), granted
    /// read-only directly after `git_user_config`.
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
