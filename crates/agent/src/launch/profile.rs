//! Profile data: what each launchable program gets, as lists and constants.
//!
//! [`SHARED`] holds what every box gets whatever the program (base
//! environment, denied secrets, the git surface, the union of the workspace
//! executable-configuration pins); [`ProgramProfile`] holds what differs per
//! program (executable, relay injection, fixed environment, home directory
//! and its pins, refused arguments, preconditions). `plan_program` in the
//! parent module turns both into a [`super::plan::LaunchPlan`], and
//! `integrity_paths_for` watches the same executable-configuration entries.
//! Request paths are joined with the string `join` (`\`), integrity paths
//! with `Path::join`, so every entry here is a single relative segment unless
//! noted as a `\`-joined relative path.

/// What every box gets, whatever the program.
pub(crate) struct SharedProfile {
    /// Environment variable names copied from the daemon into the box.
    pub base_env_keys: &'static [&'static str],
    /// The git user configuration (home-relative). Since PR 4 this is DENIED
    /// (not granted): the box reads the generated per-launch gitconfig via
    /// `GIT_CONFIG_GLOBAL` instead, so the real file (and any PAT in it)
    /// stays out of reach.
    pub git_user_config: &'static str,
    /// The XDG fallback of the git user configuration (`\`-joined), denied
    /// alongside `git_user_config` since PR 4.
    pub git_xdg_config: &'static str,
    /// Executable configuration at the workspace root: read-only when
    /// present, denied when absent, so the box can neither change nor create
    /// it. Read by Claude Code and by Copilot CLI.
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
    /// Executable configuration of the other profiled harnesses at the
    /// workspace root (OpenCode's project config), same read-only-or-denied
    /// rule unless the program asks for the read-only-when-absent variant
    /// (`ProgramProfile::workspace_union_readonly_when_absent`): workspace
    /// pins are host facts, so every box carries the union (a box may not
    /// plant config that a later, unboxed harness executes).
    pub workspace_union_entries: &'static [&'static str],
    /// Executable configuration under `<cwd>\.github` (Copilot CLI's project
    /// config, hooks, agents and MCP servers), same rule; emitted only when
    /// the folder (or one of the entries) exists, otherwise the folder itself
    /// is denied (MXC refuses a denied path under a missing parent).
    pub workspace_github_entries: &'static [&'static str],
    /// Home-relative secrets (`\`-joined) denied for every launch, in request
    /// order. The single-segment entries (the top-level home directories)
    /// are also the workspace-validation denied directories. An entry below
    /// a home folder is denied when that folder exists (or the program's
    /// launch creates it): MXC refuses a denied path under a missing parent.
    pub denied_secrets: &'static [&'static str],
}

pub(crate) const SHARED: SharedProfile = SharedProfile {
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
    git_user_config: ".gitconfig",
    git_xdg_config: r".config\git\config",
    mcp_json: ".mcp.json",
    workspace_head: "HEAD",
    workspace_claude_entries: &["settings.json", "settings.local.json", "hooks"],
    git_executable_config: &["config", "hooks", "commondir"],
    workspace_union_entries: &["opencode.json", "opencode.jsonc", ".opencode"],
    workspace_github_entries: &["copilot", "mcp.json", "hooks", "agents"],
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
};

/// Claude Code's own home, `~\.claude` (Claude only).
pub(crate) struct ClaudeHome {
    /// The home-relative directory, granted read-write and created by the
    /// launch when missing.
    pub dir: &'static str,
    /// The environment variable that points Claude Code at it.
    pub config_env: &'static str,
    /// Executable configuration under the directory: read-only when present,
    /// denied when absent, so the box can neither change nor create it.
    pub home_executable_config: &'static [&'static str],
    /// Home-relative runtime path (`\`-joined) of the client's shared data,
    /// granted read-only when it exists and simply absent otherwise. Order is
    /// part of the request: this entry sits between the executable's
    /// directory and the git installation.
    pub runtime_share: &'static str,
}

pub(crate) const CLAUDE_HOME: ClaudeHome = ClaudeHome {
    dir: ".claude",
    config_env: "CLAUDE_CONFIG_DIR",
    home_executable_config: &["settings.json", "hooks", "plugins"],
    runtime_share: r".local\share\claude",
};

/// How the relay base URL and the session pairing reach the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InjectionStrategy {
    /// The relay base URL goes into `base_url`, the pairing header into
    /// `headers` as `<name>: <value>` (Claude: `ANTHROPIC_*`; Copilot BYOK:
    /// `COPILOT_PROVIDER_*`).
    Env {
        base_url: &'static str,
        headers: &'static str,
    },
    /// The provider configuration, pairing included, goes inline into
    /// `OPENCODE_CONFIG_CONTENT`.
    OpenCodeInlineConfig,
}

/// The directory under the home that holds every harness home.
pub(crate) const HARNESS_ROOT: &str = r".agentdesktop\harness";

/// Copilot CLI's package cache under [`HARNESS_ROOT`]: a sibling of
/// `COPILOT_HOME`, granted read-only, primed outside the box.
pub(crate) const COPILOT_PKG_CACHE: &str = "copilot-pkg";

/// The per-harness home under `<home>\.agentdesktop\harness`. Every path
/// below is `\`-joined and relative to `<home>\.agentdesktop\harness`.
pub(crate) struct HarnessHome {
    /// The program's own directory (`opencode`, `copilot`).
    pub dir: &'static str,
    /// Environment variables pointing into the harness tree: `XDG_*_HOME`
    /// for OpenCode, `COPILOT_HOME` and `COPILOT_PKG_CACHE_HOME` for
    /// Copilot. Each is created before the plan runs; the grants come from
    /// `readwrite_dirs` and `readonly_dirs`.
    pub env_dirs: &'static [(&'static str, &'static str)],
    /// Directories granted read-write (created before the plan runs). A
    /// directory that holds pinned entries is granted itself, not its
    /// parent, so the box cannot rename or replace the folder holding the
    /// pins.
    pub readwrite_dirs: &'static [&'static str],
    /// Directories created before the plan runs and granted read-only, even
    /// when they sit inside a read-write directory: the program executes
    /// what lies there, so a box must not write code a later box runs
    /// (Copilot's package cache, OpenCode's downloaded tools).
    pub readonly_dirs: &'static [&'static str],
    /// Directories the daemon empties at prepare time, before the plan and
    /// the integrity snapshot: the program populates them itself at startup,
    /// and a box can rename and recreate them inside a read-write parent, so
    /// a launch starts them empty. Children are removed without following a
    /// reparse point (the link itself goes); a reparse point at the
    /// directory itself fails the launch.
    pub wiped_dirs: &'static [&'static str],
    /// Executable configuration inside the harness home: read-only when
    /// present, denied when absent, and integrity roots.
    pub executable_config: &'static [&'static str],
    /// Pinned files of `executable_config` the daemon writes as `{}` before
    /// the plan runs when they are absent (never over an existing file), and
    /// the plan grants read-only instead of denying (DV-10: MXC answers a
    /// denied path with EPERM, and OpenCode's bootstrap fails on EPERM
    /// reading its global config; Copilot CLI refuses to start when it
    /// cannot read `providers.json`). They stay pins and integrity roots.
    pub pre_created_files: &'static [&'static str],
    /// Entries denied whether or not they exist; every parent is created
    /// before the plan runs. Empty for every program: Copilot's `config.json`
    /// must stay writeable (denying it ends the CLI silently).
    pub denied: &'static [&'static str],
}

/// Per-program effective-configuration rules: `Err` carries the 409 text.
pub(crate) type ProgramPreconditions = fn(
    &agentdesktop_core::config::DaemonConfig,
    agentdesktop_core::config::ContainmentEgress,
) -> Result<(), String>;

/// The program's host-side launch checks after its preconditions, run off
/// the async runtime: reads the host where needed and returns the model the
/// box uses (`None` for a program that takes none). `Err` carries the 409
/// text.
pub(crate) type LaunchChecks = fn(
    &agentdesktop_core::config::DaemonConfig,
    &dyn super::LaunchHost,
    &std::path::Path,
) -> Result<Option<String>, String>;

/// One launchable program.
pub(crate) struct ProgramProfile {
    /// Program ID, also the session client ID.
    pub program: &'static str,
    /// The program's name in messages.
    pub display_name: &'static str,
    /// Native executable file name.
    pub exe_name: &'static str,
    /// Home-relative (`\`-joined) directories searched for `exe_name`
    /// before the daemon `PATH`.
    pub home_candidates: &'static [&'static str],
    /// Relay injection.
    pub injection: InjectionStrategy,
    /// Fixed environment key-value pairs of this program.
    pub fixed_env: &'static [(&'static str, &'static str)],
    /// The environment variable that carries the resolved model, if the
    /// program takes it from the environment (Copilot: `COPILOT_MODEL`).
    pub model_env: Option<&'static str>,
    /// Claude Code's `~\.claude`, its executable pins, runtime share and
    /// installation grant; `None` for every other program.
    pub claude_home: Option<ClaudeHome>,
    /// The harness home; `None` for Claude (which keeps `~\.claude`).
    pub harness_home: Option<HarnessHome>,
    /// Grant [`SharedProfile::workspace_union_entries`] read-only whether or
    /// not they exist, instead of denying the absent ones (OpenCode: its
    /// config scan treats a denied absent path as fatal, and its own
    /// project-config switch keeps it from reading them).
    pub workspace_union_readonly_when_absent: bool,
    /// Arguments refused when equal to or prefixed by an entry.
    pub denied_args: &'static [&'static str],
    /// Bare tokens refused wherever they appear in the args (fail-closed:
    /// no flag-arity parsing).
    pub denied_tokens: &'static [&'static str],
    /// Title of the `cmd.exe start` window.
    pub window_title: &'static str,
    /// Per-program precondition rules.
    pub preconditions: ProgramPreconditions,
    /// Per-program host-side launch checks and the model.
    pub launch_checks: LaunchChecks,
}

impl ProgramProfile {
    /// Whether this is Claude Code (the only program with `~\.claude`).
    pub(crate) fn is_claude(&self) -> bool {
        self.claude_home.is_some()
    }
}

pub(crate) const CLAUDE_CODE: ProgramProfile = ProgramProfile {
    program: crate::provider::claude_code::ClaudeCode::ID,
    display_name: "Claude Code",
    exe_name: "claude.exe",
    home_candidates: &[r".local\bin"],
    injection: InjectionStrategy::Env {
        base_url: "ANTHROPIC_BASE_URL",
        headers: "ANTHROPIC_CUSTOM_HEADERS",
    },
    fixed_env: &[
        ("TERM", "xterm-256color"),
        ("CLAUDE_CODE_USE_POWERSHELL_TOOL", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("ANTHROPIC_AUTH_TOKEN", "placeholder"),
        ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"),
    ],
    model_env: None,
    claude_home: Some(CLAUDE_HOME),
    harness_home: None,
    workspace_union_readonly_when_absent: false,
    denied_args: &[],
    denied_tokens: &[],
    window_title: "agentdesktop: claude",
    preconditions: super::claude_code_preconditions,
    launch_checks: super::no_launch_checks,
};

/// OpenCode's executable configuration under its global config directory
/// (`XDG_CONFIG_HOME\opencode`): the global config files (`config` is the
/// legacy TOML file OpenCode imports and rewrites as `config.json`), the TUI
/// config, the global `AGENTS.md`, and the plural and singular directories
/// OpenCode loads agents, commands, modes, plugins, skills, tools and themes
/// from (there is no singular `theme`).
pub(crate) const OPENCODE: ProgramProfile = ProgramProfile {
    program: "opencode",
    display_name: "OpenCode",
    exe_name: "opencode.exe",
    home_candidates: &[r".local\bin"],
    injection: InjectionStrategy::OpenCodeInlineConfig,
    fixed_env: &[
        ("TERM", "xterm-256color"),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
    ],
    model_env: None,
    claude_home: None,
    harness_home: Some(HarnessHome {
        dir: "opencode",
        env_dirs: &[
            ("XDG_DATA_HOME", r"opencode\data"),
            ("XDG_CONFIG_HOME", r"opencode\config"),
            ("XDG_STATE_HOME", r"opencode\state"),
            ("XDG_CACHE_HOME", r"opencode\cache"),
        ],
        // `config\opencode`, not `config`: the pins below sit directly under
        // the read-write directory, like `~\.claude`'s.
        readwrite_dirs: &[
            r"opencode\data",
            r"opencode\config\opencode",
            r"opencode\state",
            r"opencode\cache",
        ],
        // OpenCode runs ripgrep and LSP servers from `<cache>\opencode\bin`
        // when they are not on PATH. The daemon empties it at prepare time:
        // a box can rename the read-write cache root and recreate the tree.
        readonly_dirs: &[r"opencode\cache\opencode\bin"],
        wiped_dirs: &[r"opencode\cache\opencode\bin"],
        executable_config: &[
            r"opencode\config\opencode\opencode.json",
            r"opencode\config\opencode\opencode.jsonc",
            r"opencode\config\opencode\config.json",
            r"opencode\config\opencode\config",
            r"opencode\config\opencode\tui.json",
            r"opencode\config\opencode\tui.jsonc",
            r"opencode\config\opencode\AGENTS.md",
            r"opencode\config\opencode\agents",
            r"opencode\config\opencode\agent",
            r"opencode\config\opencode\commands",
            r"opencode\config\opencode\command",
            r"opencode\config\opencode\modes",
            r"opencode\config\opencode\mode",
            r"opencode\config\opencode\plugins",
            r"opencode\config\opencode\plugin",
            r"opencode\config\opencode\skills",
            r"opencode\config\opencode\skill",
            r"opencode\config\opencode\tools",
            r"opencode\config\opencode\tool",
            r"opencode\config\opencode\themes",
        ],
        // The global config trio OpenCode's bootstrap reads unconditionally.
        // The legacy TOML `config` stays denied when absent: OpenCode only
        // imports it when it exists.
        pre_created_files: &[
            r"opencode\config\opencode\config.json",
            r"opencode\config\opencode\opencode.json",
            r"opencode\config\opencode\opencode.jsonc",
        ],
        denied: &[],
    }),
    workspace_union_readonly_when_absent: true,
    // An in-box HTTP listener would hand the session pairing to anything on
    // loopback (ingress is allowed so the box reaches the relay).
    denied_args: &["--port", "--hostname", "--mdns"],
    denied_tokens: &["serve", "web"],
    window_title: "agentdesktop: opencode",
    preconditions: super::opencode_preconditions,
    launch_checks: super::opencode_launch_checks,
};

/// Copilot CLI in BYOK mode against the relay. `COPILOT_HOME` holds its
/// state; the executable entries of its config-dir reference are pinned.
/// `config.json` (auth state and trusted folders) is written by Copilot
/// itself and stays writeable under the home's grant; `providers.json` is
/// pre-created read-only because the CLI refuses to start without it. The
/// package cache is a read-only sibling the daemon never primes.
pub(crate) const COPILOT: ProgramProfile = ProgramProfile {
    program: "copilot",
    display_name: "Copilot CLI",
    exe_name: "copilot.exe",
    home_candidates: &[r".local\bin"],
    injection: InjectionStrategy::Env {
        base_url: "COPILOT_PROVIDER_BASE_URL",
        headers: "COPILOT_PROVIDER_HEADERS",
    },
    fixed_env: &[
        ("TERM", "xterm-256color"),
        ("COPILOT_PROVIDER_TYPE", "anthropic"),
        ("COPILOT_PROVIDER_API_KEY", "placeholder"),
        ("COPILOT_AUTO_UPDATE", "false"),
    ],
    model_env: Some("COPILOT_MODEL"),
    claude_home: None,
    harness_home: Some(HarnessHome {
        dir: "copilot",
        env_dirs: &[
            ("COPILOT_HOME", "copilot"),
            ("COPILOT_PKG_CACHE_HOME", COPILOT_PKG_CACHE),
        ],
        readwrite_dirs: &["copilot"],
        readonly_dirs: &[COPILOT_PKG_CACHE],
        wiped_dirs: &[],
        executable_config: &[
            r"copilot\settings.json",
            r"copilot\mcp-config.json",
            r"copilot\lsp-config.json",
            r"copilot\providers.json",
            r"copilot\copilot-instructions.md",
            r"copilot\instructions",
            r"copilot\agents",
            r"copilot\skills",
            r"copilot\hooks",
            r"copilot\extensions",
            r"copilot\installed-plugins",
        ],
        // Copilot CLI reads its BYOK provider registry at startup and refuses
        // to start when the file is missing or unreadable.
        pre_created_files: &[r"copilot\providers.json"],
        denied: &[],
    }),
    workspace_union_readonly_when_absent: false,
    // The box is the sandbox; Copilot's own cannot nest inside it.
    denied_args: &["--sandbox"],
    denied_tokens: &[],
    window_title: "agentdesktop: copilot",
    preconditions: super::copilot_preconditions,
    launch_checks: super::copilot_launch_checks,
};

/// The profile of `program` (`claude-code`, `opencode`, `copilot`); `None`
/// for any other program.
pub(crate) fn program_profile(program: &str) -> Option<&'static ProgramProfile> {
    [&CLAUDE_CODE, &OPENCODE, &COPILOT]
        .into_iter()
        .find(|profile| profile.program == program)
}

/// The launchable program IDs, for messages.
pub(crate) const PROGRAMS: &str = "claude-code, opencode, copilot";
