use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use agentdesktop_client as client;
use agentdesktop_core::{
    config::DaemonConfig,
    model::{
        ConsoleAttach, Discovery, EnrollmentStatus, Health, Launch, LaunchConsole, LaunchRequest,
        LaunchStatus, LlmGatewayCredential, LlmGatewayLoginStatus, ProgramChoice,
        ProgramManagement, SessionCreated, SessionInfo, SessionRequest, TelemetryEventKind,
    },
};
use anyhow::Context;
use clap::Subcommand;
use serde::Deserialize;

const MAX_HOOK_INPUT_BYTES: u64 = 1024 * 1024;

#[derive(Subcommand)]
pub enum ClientCommand {
    /// Check whether the daemon is reachable.
    Status,
    /// Print the device's enrollment status and any pending sign-in URL.
    Enrollment,
    /// Discover locally installed agents.
    Discover,
    /// Print the daemon's local startup configuration.
    Config,
    /// Print a short-lived credential for an LLM gateway.
    Credential {
        /// Developer tool requesting the credential.
        #[arg(long, default_value = "agentdesktop")]
        client_id: String,
    },
    /// Sign in to the LLM gateway with OIDC device authorization and print
    /// the verification URL and code to approve from any device.
    Login,
    /// List the programs this device's configuration manages, or choose which
    /// of them Agentdesktop manages.
    Programs {
        #[command(subcommand)]
        command: Option<ProgramsCommand>,
    },
    /// Run a program in a sandbox (Windows AppContainer) with the current
    /// directory as its workspace. Arguments after `--` go to the program.
    Run {
        /// Program to run: `claude-code` (alias `claude`), `opencode` or
        /// `copilot` (Copilot CLI).
        program: String,
        /// Wait for the box to exit. From a console the program runs in that
        /// console (same-terminal hosting) and its own exit code is returned;
        /// callers without a console get the windowed launch and the
        /// executor's exit code. In same-terminal mode Ctrl+C reaches the
        /// program; in windowed mode it stops the waiter only.
        #[arg(long)]
        wait: bool,
        /// Arguments passed to the program.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Internal: attach the box to a client's console and run the executor.
    #[command(hide = true)]
    ConsoleAttach {
        /// The requesting client process whose console to attach to.
        #[arg(long)]
        client_pid: u32,
        /// The executor binary (`wxc-exec.exe`).
        #[arg(long)]
        executor: PathBuf,
        /// The launch's request.json.
        #[arg(long)]
        request: PathBuf,
        /// The launch's log file for helper diagnostics.
        #[arg(long)]
        log: PathBuf,
    },
    /// Handle an event emitted by a managed developer-tool hook.
    Hook {
        #[command(subcommand)]
        hook: HookCommand,
    },
    /// Manage the local LLM proxy.
    LlmProxy {
        #[command(subcommand)]
        command: LlmProxyCommand,
    },
}

#[derive(Subcommand)]
pub enum LlmProxyCommand {
    /// Manage sandbox sessions on the proxy.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

#[derive(Subcommand)]
pub enum SessionCommand {
    /// Register a session and print its pairing as JSON.
    Create {
        /// Credential policy client ID the session presents to the gateway.
        #[arg(long)]
        client_id: String,
        /// Human-readable name for the session.
        #[arg(long)]
        label: String,
        /// Seconds without a request after which the session expires.
        #[arg(long)]
        ttl_seconds: Option<u64>,
    },
    /// List registered sessions.
    List,
    /// Revoke a session.
    Revoke {
        /// Session ID from `session create` or `session list`.
        id: String,
    },
}

/// The message printed for a failed `llm-proxy session` command: the daemon's
/// own text for a missing session, a hint at the bound, else the status and
/// body as the client reports them.
fn session_error_message(error: &anyhow::Error) -> String {
    match error.downcast_ref::<agentdesktop_client::StatusError>() {
        Some(status) if status.status == 404 && !status.body.is_empty() => status.body.clone(),
        Some(status) if status.status == 404 => {
            "this daemon has no session API (upgrade agentdesktop)".to_owned()
        }
        Some(status) if status.status == 429 => {
            crate::llm_proxy::sessions::too_many_sessions_message()
        }
        Some(status) => status.to_string(),
        None => format!("{error:#}"),
    }
}

const SESSIONS_PATH: &str = "/v1/llm-proxy/sessions";

/// A session id as the daemon mints it: 16 lowercase hexadecimal characters.
fn valid_session_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn run_session_command(command: SessionCommand, socket: &Path) -> anyhow::Result<()> {
    match command {
        SessionCommand::Create {
            client_id,
            label,
            ttl_seconds,
        } => {
            // The only place the pairing is shown: the caller passes it into
            // the sandboxed program's environment.
            let created: SessionCreated = client::post_json_response(
                socket,
                SESSIONS_PATH,
                &SessionRequest {
                    client_id,
                    label,
                    ttl_seconds,
                },
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&created)?);
        }
        SessionCommand::List => {
            let sessions: Vec<SessionInfo> = client::get(socket, SESSIONS_PATH).await?;
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}",
                    session.id, session.client_id, session.label, session.expires_at_unix_seconds
                );
            }
        }
        SessionCommand::Revoke { id } => {
            if !valid_session_id(&id) {
                anyhow::bail!("session id must be 16 lowercase hexadecimal characters");
            }
            client::delete(socket, &format!("{SESSIONS_PATH}/{id}")).await?;
        }
    }
    Ok(())
}

#[derive(Subcommand)]
pub enum ProgramsCommand {
    /// Choose the programs Agentdesktop manages on this device: `all`, `none`,
    /// or program IDs such as `claude-code codex`. Allowed only when the
    /// configuration sets `programSelection.allowUserChoice`; Agentdesktop
    /// removes the settings it wrote for each program left out.
    Set {
        #[arg(required = true, value_name = "all|none|PROGRAM")]
        programs: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum HookCommand {
    /// Report a Claude Code SessionStart event as telemetry.
    ClaudeSessionStart,
    /// Report a Claude Code PreToolUse event as telemetry.
    ClaudePreToolUse {
        /// Include tool input in the event.
        #[arg(long)]
        include_input: bool,
    },
}

#[derive(Deserialize)]
struct ClaudePreToolUseInput {
    hook_event_name: Option<String>,
    tool_name: String,
    tool_use_id: Option<String>,
    tool_input: serde_json::Value,
}

#[derive(Deserialize)]
struct ClaudeSessionStartInput {
    hook_event_name: Option<String>,
    session_id: String,
}

pub async fn run(command: ClientCommand, socket: PathBuf) -> anyhow::Result<()> {
    match command {
        ClientCommand::Status => {
            let health: Health = client::get(&socket, "/v1/health").await?;
            println!("{}", health.status);
        }
        ClientCommand::Enrollment => {
            let enrollment: EnrollmentStatus = client::get(&socket, "/v1/enrollment").await?;
            println!("{}", enrollment.status);
            if let Some(url) = enrollment.authorization_url {
                println!("Sign in at: {url}");
            }
            if let Some(code) = enrollment.user_code {
                println!("Code: {code}");
            }
        }
        ClientCommand::Login => {
            let login: LlmGatewayLoginStatus =
                client::post(&socket, "/v1/llm-gateway/login").await?;
            println!("{}", login.status);
            if let Some(url) = login.verification_url {
                println!("Sign in at: {url}");
            }
            if let Some(code) = login.user_code {
                println!("Code: {code}");
            }
        }
        ClientCommand::Programs { command: None } => {
            let management: ProgramManagement = client::get(&socket, "/v1/programs").await?;
            print_programs(&management);
        }
        ClientCommand::Programs {
            command: Some(ProgramsCommand::Set { programs }),
        } => {
            let choice = parse_program_choice(&programs)?;
            let management: ProgramManagement =
                client::post_json_decode(&socket, "/v1/programs/choice", &choice).await?;
            print_programs(&management);
        }
        ClientCommand::Discover => {
            let discovery: Discovery = client::get(&socket, "/v1/discovery").await?;
            if discovery.agents.is_empty() && discovery.model_runtimes.is_empty() {
                println!("No agents or models discovered");
            }
            for agent in discovery.agents {
                let version = agent.version.as_deref().unwrap_or("unknown version");
                println!(
                    "{}\t{}\t{}",
                    agent.kind,
                    version,
                    agent.executable.display()
                );
            }
            for runtime in discovery.model_runtimes {
                for model in runtime.models {
                    println!("{}\tmodel\t{}", runtime.kind, model.name);
                }
            }
        }
        ClientCommand::Config => {
            let config: DaemonConfig = client::get(&socket, "/v1/config").await?;
            print!(
                "{}",
                agentdesktop_core::serdes::yamlviajson::to_string(&config)?
            );
        }
        ClientCommand::Credential { client_id } => {
            let client_id: String =
                url::form_urlencoded::byte_serialize(client_id.as_bytes()).collect();
            let response: LlmGatewayCredential = client::get(
                &socket,
                &format!("/v1/llm-gateway/credential?client_id={client_id}"),
            )
            .await?;
            println!("{}", response.credential);
        }
        ClientCommand::Run {
            program,
            wait,
            args,
        } => {
            let cwd = std::env::current_dir().context("read the current directory")?;
            let mut request = run_request(&program, cwd, args);
            // Same-terminal hosting: a waiting caller that owns a console hands
            // it to the daemon; the boxed program runs there and its own exit
            // code comes back. Callers without a console keep the window.
            if wait && crate::launch::console::client_console_available() {
                request.console = Some(ConsoleAttach {
                    client_pid: std::process::id(),
                });
            }
            if wait {
                let (code, line) = run_and_wait(&socket, &request, RUN_WAIT_POLL).await;
                if let Some(line) = line {
                    eprintln!("{line}");
                }
                std::process::exit(code);
            }
            match client::post_json_response::<_, Launch>(&socket, "/v1/launch", &request).await {
                Ok(launch) => {
                    println!("{}", run_success_line(&launch));
                }
                Err(error) => {
                    eprintln!("{}", run_error_message(&error));
                    std::process::exit(1);
                }
            }
        }
        ClientCommand::ConsoleAttach {
            client_pid,
            executor,
            request,
            log,
        } => match crate::launch::console::run_attach(client_pid, &executor, &request, &log) {
            Ok(code) => std::process::exit(code),
            Err(_) => std::process::exit(crate::launch::console::ATTACH_FAILED_EXIT),
        },
        ClientCommand::LlmProxy {
            command: LlmProxyCommand::Session { command },
        } => run_session_command(command, &socket)
            .await
            .map_err(|error| anyhow::anyhow!(session_error_message(&error)))?,
        ClientCommand::Hook {
            hook: HookCommand::ClaudePreToolUse { include_input },
        } => {
            // Telemetry is deliberately fail-open: an unavailable daemon must
            // never prevent Claude Code from using a tool.
            let _ = report_claude_pre_tool_use(&socket, include_input).await;
        }
        ClientCommand::Hook {
            hook: HookCommand::ClaudeSessionStart,
        } => {
            // Telemetry is deliberately fail-open: an unavailable daemon must
            // never prevent Claude Code from starting a session.
            let _ = report_claude_session_start(&socket).await;
        }
    }

    Ok(())
}

/// `all`, `none`, or the program IDs to manage.
fn parse_program_choice(programs: &[String]) -> anyhow::Result<ProgramChoice> {
    match programs {
        [only] if only == "all" => Ok(ProgramChoice::All),
        [only] if only == "none" => Ok(ProgramChoice::None),
        _ if programs
            .iter()
            .any(|program| program == "all" || program == "none") =>
        {
            anyhow::bail!("`all` and `none` cannot be combined with program IDs")
        }
        _ => Ok(ProgramChoice::Selected {
            programs: programs.iter().cloned().collect(),
        }),
    }
}

fn print_programs(management: &ProgramManagement) {
    if management.user_choice_allowed {
        let choice = match &management.choice {
            ProgramChoice::All => "all".to_owned(),
            ProgramChoice::None => "none".to_owned(),
            ProgramChoice::Selected { programs } => {
                programs.iter().cloned().collect::<Vec<_>>().join(" ")
            }
        };
        println!("Your choice: {choice}");
    } else {
        println!("Your organization manages every configured program on this device.");
    }
    if management.programs.is_empty() {
        println!("No programs are configured on this device.");
    }
    for program in &management.programs {
        let managed = if program.managed {
            "managed"
        } else {
            "opted out"
        };
        let state = program.state.as_deref().unwrap_or("not applied yet");
        if program.detail.is_empty() {
            println!("{}\t{managed}\t{state}", program.program);
        } else {
            println!(
                "{}\t{managed}\t{state}\t{}",
                program.program, program.detail
            );
        }
    }
}

/// The launch request for `agentdesktop run <program>`; the alias `claude`
/// maps to the provider id `claude-code`. `opencode` and `copilot` are their
/// own IDs (the boxed Copilot CLI is `copilot`, distinct from the unboxed
/// route identity `copilot-cli`).
pub fn run_request(program: &str, cwd: PathBuf, args: Vec<String>) -> LaunchRequest {
    let program = if program == "claude" {
        "claude-code"
    } else {
        program
    };
    LaunchRequest {
        program: program.to_owned(),
        cwd,
        args,
        console: None,
    }
}

/// The line `agentdesktop run` prints on stdout for a successful launch.
pub fn run_success_line(launch: &Launch) -> String {
    format!(
        "launched {} (container {})",
        launch.launch_id, launch.container_id
    )
}

/// The line `agentdesktop run` prints on stderr for a failed launch request.
pub fn run_error_message(error: &anyhow::Error) -> String {
    match error.downcast_ref::<agentdesktop_client::StatusError>() {
        Some(status) if status.status == 404 => {
            "this daemon does not support `run` (upgrade agentdesktop)".to_owned()
        }
        Some(status) => status.to_string(),
        None => format!("{error:#}"),
    }
}

/// Poll cadence of `agentdesktop run --wait`.
pub(crate) const RUN_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// Posts `request`, then polls `GET /v1/launches` every `poll` and
/// selects the launch id client-side (the route has no filter) until it
/// exits, is evicted, or the daemon becomes unreachable. Returns the exit
/// code and, except on a clean exit, exactly one stderr line (newlines in the
/// underlying error joined with `; `; only `--wait` joins, default `run`
/// output is unchanged):
/// - the launch reaches `exited`: no output, exit `exitCode`, or 1 when null;
/// - the POST is refused (transport error, any non-2xx): exit 1, one line;
/// - the launch id is absent from a later poll (evicted, or the daemon
///   restarted): `"launch <id> is no longer known to the daemon"`, exit 1;
/// - the daemon becomes unreachable while waiting: exit 1.
pub(crate) async fn run_and_wait(
    socket: &Path,
    request: &LaunchRequest,
    poll: Duration,
) -> (i32, Option<String>) {
    let launch = match client::post_json_response::<_, Launch>(socket, "/v1/launch", request).await
    {
        Ok(launch) => launch,
        Err(error) => return (1, Some(one_line(&run_error_message(&error)))),
    };
    // Same-terminal hosting: Ctrl+C belongs to the boxed program; the waiter
    // must stay so the shell keeps waiting for `claude` like stock. In windowed
    // mode the documented behaviour stays (Ctrl+C ends the waiter only).
    if launch.console == LaunchConsole::Attached {
        crate::launch::console::ignore_ctrl_c();
    }
    loop {
        tokio::time::sleep(poll).await;
        let launches: Vec<Launch> = match client::get(socket, "/v1/launches").await {
            Ok(launches) => launches,
            Err(error) => return (1, Some(one_line(&run_error_message(&error)))),
        };
        match launches.iter().find(|l| l.launch_id == launch.launch_id) {
            Some(found) if found.status == LaunchStatus::Exited => {
                return (found.exit_code.unwrap_or(1), None);
            }
            Some(_) => {}
            None => {
                return (
                    1,
                    Some(format!(
                        "launch {} is no longer known to the daemon",
                        launch.launch_id
                    )),
                );
            }
        }
    }
}

/// Joins the lines of an error message into one, for `--wait`'s single
/// stderr line.
fn one_line(message: &str) -> String {
    message.lines().collect::<Vec<_>>().join("; ")
}

async fn report_claude_pre_tool_use(
    socket: &std::path::Path,
    include_input: bool,
) -> anyhow::Result<()> {
    let input = read_hook_input()?;
    let event = parse_claude_pre_tool_use(&input, include_input)?;
    client::post_json(socket, "/v1/telemetry", &event).await
}

async fn report_claude_session_start(socket: &std::path::Path) -> anyhow::Result<()> {
    let input = read_hook_input()?;
    let input: ClaudeSessionStartInput = serde_json::from_slice(&input)?;
    if input
        .hook_event_name
        .as_deref()
        .is_some_and(|name| name != "SessionStart")
    {
        anyhow::bail!("expected a Claude Code SessionStart event");
    }
    if input.session_id.trim().is_empty() {
        anyhow::bail!("Claude hook event has no session ID");
    }
    client::post_json(
        socket,
        "/v1/telemetry",
        &TelemetryEventKind::SessionNew {
            client_id: claude_client_id(),
            session_id: input.session_id,
        },
    )
    .await
}

fn read_hook_input() -> anyhow::Result<Vec<u8>> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_HOOK_INPUT_BYTES {
        anyhow::bail!("Claude hook input exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    Ok(input)
}

fn parse_claude_pre_tool_use(
    input: &[u8],
    include_input: bool,
) -> anyhow::Result<TelemetryEventKind> {
    let input: ClaudePreToolUseInput = serde_json::from_slice(input)?;
    if input
        .hook_event_name
        .as_deref()
        .is_some_and(|name| name != "PreToolUse")
    {
        anyhow::bail!("expected a Claude Code PreToolUse event");
    }
    if input.tool_name.trim().is_empty() {
        anyhow::bail!("Claude hook event has no tool name");
    }

    Ok(TelemetryEventKind::ToolUse {
        client_id: claude_client_id(),
        tool_name: input.tool_name,
        tool_use_id: input.tool_use_id.filter(|id| !id.is_empty()),
        tool_input: include_input.then_some(input.tool_input),
    })
}

fn claude_client_id() -> String {
    claude_client_id_for_entrypoint(std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref())
        .to_owned()
}

fn claude_client_id_for_entrypoint(entrypoint: Option<&str>) -> &'static str {
    match entrypoint {
        Some("claude-desktop" | "claude-desktop-3p") => "claude-desktop",
        _ => "claude-code",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agentdesktop_client::StatusError;
    use agentdesktop_core::model::{LaunchRequest, ProgramChoice, TelemetryEventKind};
    use clap::Parser;

    use super::{
        ClientCommand, LlmProxyCommand, SessionCommand, parse_claude_pre_tool_use,
        parse_program_choice, run_error_message, run_request, session_error_message,
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn program_choice_is_all_none_or_program_ids() {
        assert_eq!(
            parse_program_choice(&args(&["all"])).unwrap(),
            ProgramChoice::All
        );
        assert_eq!(
            parse_program_choice(&args(&["none"])).unwrap(),
            ProgramChoice::None
        );
        assert_eq!(
            parse_program_choice(&args(&["codex", "claude-code"])).unwrap(),
            ProgramChoice::Selected {
                programs: ["claude-code".to_owned(), "codex".to_owned()].into(),
            }
        );
        assert!(parse_program_choice(&args(&["none", "codex"])).is_err());
    }

    /// Mirrors the binaries: a global `--socket` plus the flattened client commands.
    #[derive(Parser)]
    struct TestCli {
        #[arg(long, global = true, default_value = "default.sock")]
        socket: PathBuf,
        #[command(subcommand)]
        command: ClientCommand,
    }

    fn parse(args: &[&str]) -> TestCli {
        TestCli::try_parse_from(std::iter::once("agentdesktop").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn run_maps_the_claude_alias_to_the_provider_id() {
        let cwd = PathBuf::from("/work/app");
        assert_eq!(
            run_request("claude", cwd.clone(), vec!["--model".into(), "x".into()]),
            LaunchRequest {
                program: "claude-code".into(),
                cwd: cwd.clone(),
                args: vec!["--model".into(), "x".into()],
                console: None,
            }
        );
        assert_eq!(
            run_request("claude-code", cwd.clone(), vec![]),
            LaunchRequest {
                program: "claude-code".into(),
                cwd,
                args: vec![],
                console: None,
            }
        );
    }

    #[test]
    fn run_passes_everything_after_the_separator_to_the_program() {
        let cli = parse(&["run", "claude", "--", "--socket", "x.sock", "--model", "x"]);
        let ClientCommand::Run { program, args, .. } = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(program, "claude");
        assert_eq!(args, ["--socket", "x.sock", "--model", "x"]);
        assert_eq!(
            cli.socket,
            PathBuf::from("default.sock"),
            "--socket after -- belongs to the program"
        );
    }

    #[test]
    fn run_honours_the_socket_flag_before_the_separator_and_allows_no_arguments() {
        let cli = parse(&["--socket", "mine.sock", "run", "claude", "--", "--debug"]);
        assert_eq!(cli.socket, PathBuf::from("mine.sock"));
        let ClientCommand::Run { args, .. } = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(args, ["--debug"]);

        let cli = parse(&["run", "claude", "--socket", "late.sock"]);
        assert_eq!(cli.socket, PathBuf::from("late.sock"));
        let ClientCommand::Run { args, .. } = cli.command else {
            panic!("expected the run command");
        };
        assert!(args.is_empty());
    }

    #[test]
    fn run_error_message_maps_404_to_an_upgrade_hint() {
        let error = anyhow::Error::new(StatusError {
            status: 404,
            body: String::new(),
        });
        assert_eq!(
            run_error_message(&error),
            "this daemon does not support `run` (upgrade agentdesktop)"
        );
    }

    #[test]
    fn run_error_message_prints_the_body_after_the_status() {
        let error = anyhow::Error::new(StatusError {
            status: 409,
            body: "executor not found: C:\\missing\\wxc-exec.exe".into(),
        });
        assert_eq!(
            run_error_message(&error),
            "daemon returned 409 Conflict: executor not found: C:\\missing\\wxc-exec.exe"
        );
    }

    #[test]
    fn run_error_message_keeps_the_text_of_a_transport_error() {
        let error = anyhow::anyhow!("connect to /tmp/x.sock\nCheck that the daemon is running.");
        assert!(run_error_message(&error).contains("connect to /tmp/x.sock"));
    }

    // AC 4: run_success_line test
    #[test]
    fn run_success_line_formats_launch_and_container_id() {
        use agentdesktop_core::model::{Launch, LaunchStatus};
        let launch = Launch {
            launch_id: "0123456789abcdef".into(),
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            container_id: "agentdesktop-0123456789abcdef".into(),
            status: LaunchStatus::Running,
            exit_code: None,
            started_at_unix_seconds: 1_700_000_000,
            ended_at_unix_seconds: None,
            integrity_drift: false,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            session_id: "test-session-id".into(),
            console: agentdesktop_core::model::LaunchConsole::Window,
        };
        let line = super::run_success_line(&launch);
        assert_eq!(
            line,
            "launched 0123456789abcdef (container agentdesktop-0123456789abcdef)"
        );
    }

    // PR 6 (AC-4): `run --wait`.

    #[test]
    fn run_wait_is_off_by_default_and_parses_as_a_flag() {
        let cli = parse(&["run", "claude"]);
        let ClientCommand::Run { wait, .. } = cli.command else {
            panic!("expected the run command");
        };
        assert!(!wait, "default run behaviour is unchanged without --wait");

        let cli = parse(&["run", "claude", "--wait"]);
        let ClientCommand::Run { wait, .. } = cli.command else {
            panic!("expected the run command");
        };
        assert!(wait);
    }

    #[cfg(target_os = "linux")]
    mod run_and_wait_tests {
        use std::{
            convert::Infallible,
            path::PathBuf,
            sync::{Arc, Mutex},
            time::Duration,
        };

        use agentdesktop_core::model::{Launch, LaunchRequest, LaunchStatus};
        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::{Request, Response, service::service_fn};
        use hyper_util::rt::TokioIo;
        use tokio::net::UnixListener;

        use super::super::run_and_wait;

        /// One canned HTTP response: status and body.
        type Canned = (u16, String);

        /// A daemon stand-in on a Unix socket. `/v1/launch` always answers
        /// `post_launch`; `/v1/launches` answers the next entry of
        /// `launches_sequence` (each GET consumes one; the last entry repeats).
        struct FakeDaemon {
            socket: PathBuf,
            task: tokio::task::JoinHandle<()>,
        }

        impl Drop for FakeDaemon {
            fn drop(&mut self) {
                self.task.abort();
                let _ = std::fs::remove_file(&self.socket);
            }
        }

        fn fake_daemon(
            name: &str,
            post_launch: Canned,
            launches_sequence: Vec<Canned>,
        ) -> FakeDaemon {
            let socket = std::env::temp_dir().join(format!(
                "agentdesktop-cli-test-{}-{name}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            let post_launch = Arc::new(post_launch);
            let launches_sequence = Arc::new(Mutex::new(launches_sequence));
            let task = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let post_launch = post_launch.clone();
                    let launches_sequence = launches_sequence.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(stream),
                                service_fn(move |request: Request<hyper::body::Incoming>| {
                                    let post_launch = post_launch.clone();
                                    let launches_sequence = launches_sequence.clone();
                                    async move {
                                        let path = request.uri().path().to_owned();
                                        let _ = request.into_body().collect().await;
                                        let (status, body) = if path == "/v1/launch" {
                                            (*post_launch).clone()
                                        } else {
                                            let mut sequence = launches_sequence.lock().unwrap();
                                            if sequence.len() > 1 {
                                                sequence.remove(0)
                                            } else {
                                                sequence[0].clone()
                                            }
                                        };
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(status)
                                                .body(Full::new(Bytes::from(body)))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            FakeDaemon { socket, task }
        }

        fn request() -> LaunchRequest {
            LaunchRequest {
                program: "claude-code".into(),
                cwd: "/work/app".into(),
                args: vec![],
                console: None,
            }
        }

        fn launch_json(status: LaunchStatus, exit_code: Option<i32>) -> String {
            serde_json::to_string(&Launch {
                launch_id: "0123456789abcdef".into(),
                program: "claude-code".into(),
                cwd: "/work/app".into(),
                container_id: "agentdesktop-0123456789abcdef".into(),
                status,
                exit_code,
                started_at_unix_seconds: 1_700_000_000,
                ended_at_unix_seconds: None,
                integrity_drift: false,
                managed_settings_source: None,
                managed_hooks_locked: false,
                managed_mcp_locked: false,
                session_id: "test-session-id".into(),
                console: agentdesktop_core::model::LaunchConsole::Window,
            })
            .unwrap()
        }

        #[tokio::test]
        async fn an_exited_launch_with_a_code_prints_nothing_and_returns_it() {
            let running = launch_json(LaunchStatus::Running, None);
            let exited = launch_json(LaunchStatus::Exited, Some(7));
            let daemon = fake_daemon(
                "exit-code",
                (201, running.clone()),
                vec![(200, format!("[{running}]")), (200, format!("[{exited}]"))],
            );
            let (code, line) =
                run_and_wait(&daemon.socket, &request(), Duration::from_millis(1)).await;
            assert_eq!(code, 7);
            assert_eq!(line, None);
        }

        #[tokio::test]
        async fn an_exited_launch_with_no_code_returns_one() {
            let exited = launch_json(LaunchStatus::Exited, None);
            let daemon = fake_daemon(
                "exit-null",
                (201, exited.clone()),
                vec![(200, format!("[{exited}]"))],
            );
            let (code, line) =
                run_and_wait(&daemon.socket, &request(), Duration::from_millis(1)).await;
            assert_eq!(code, 1);
            assert_eq!(line, None);
        }

        #[tokio::test]
        async fn a_refused_post_answers_exit_1_with_one_stderr_line() {
            for status in [400u16, 403, 404, 409, 500] {
                let daemon = fake_daemon(
                    &format!("post-refused-{status}"),
                    (status, "refused".to_owned()),
                    vec![(200, "[]".to_owned())],
                );
                let (code, line) =
                    run_and_wait(&daemon.socket, &request(), Duration::from_millis(1)).await;
                assert_eq!(code, 1, "status {status}");
                let line = line.unwrap_or_else(|| panic!("status {status} needs a stderr line"));
                assert_eq!(line.lines().count(), 1, "{line}");
            }
        }

        /// AC-4: no failure mode of `run --wait` falls back to running a
        /// native `claude.exe`. A sentinel executable sits beside the test's
        /// socket; had anything executed it, the marker file would exist.
        #[tokio::test]
        async fn a_sentinel_claude_exe_beside_the_test_never_runs() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let sentinel = dir.path().join("claude.exe");
            let marker = dir.path().join("marker");
            std::fs::write(
                &sentinel,
                format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o755)).unwrap();

            // A refused POST and a vanished id both end the wait with exit 1;
            // neither may start anything natively.
            let daemon = fake_daemon(
                "sentinel",
                (409, "refused".to_owned()),
                vec![(200, "[]".to_owned())],
            );
            let (code, line) =
                run_and_wait(&daemon.socket, &request(), Duration::from_millis(1)).await;
            assert_eq!(code, 1);
            assert!(line.is_some());
            assert!(!marker.exists(), "the sentinel claude.exe was executed");
        }

        #[tokio::test]
        async fn an_id_that_vanishes_mid_wait_is_reported_and_exits_1() {
            let running = launch_json(LaunchStatus::Running, None);
            let daemon = fake_daemon(
                "evicted",
                (201, running.clone()),
                vec![(200, format!("[{running}]")), (200, "[]".to_owned())],
            );
            let (code, line) =
                run_and_wait(&daemon.socket, &request(), Duration::from_millis(1)).await;
            assert_eq!(code, 1);
            assert_eq!(
                line,
                Some("launch 0123456789abcdef is no longer known to the daemon".to_owned())
            );
        }

        /// Serves exactly the POST `/v1/launch`, then stops: the socket file
        /// is removed, so the first poll's connect fails (unreachable mid-wait).
        #[tokio::test]
        async fn an_unreachable_daemon_while_waiting_exits_1_with_one_line() {
            let running = launch_json(LaunchStatus::Running, None);
            let socket = std::env::temp_dir().join(format!(
                "agentdesktop-cli-test-{}-midwait.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            let task = tokio::spawn({
                let socket = socket.clone();
                async move {
                    let (stream, _) = listener.accept().await.unwrap();
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(stream),
                            service_fn(move |request: Request<hyper::body::Incoming>| {
                                let running = running.clone();
                                async move {
                                    let _ = request.into_body().collect().await;
                                    Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(201)
                                            .body(Full::new(Bytes::from(running)))
                                            .unwrap(),
                                    )
                                }
                            }),
                        )
                        .await;
                    drop(listener);
                    let _ = std::fs::remove_file(&socket);
                }
            });
            let (code, line) = run_and_wait(&socket, &request(), Duration::from_millis(1)).await;
            task.abort();
            assert_eq!(code, 1);
            let line = line.unwrap();
            assert_eq!(line.lines().count(), 1, "{line}");
        }
    }

    fn session_command(args: &[&str]) -> Result<SessionCommand, clap::Error> {
        let mut argv = vec!["agentdesktop", "llm-proxy", "session"];
        argv.extend_from_slice(args);
        match TestCli::try_parse_from(argv)?.command {
            ClientCommand::LlmProxy {
                command: LlmProxyCommand::Session { command },
            } => Ok(command),
            _ => panic!("expected an llm-proxy session command"),
        }
    }

    #[test]
    fn parses_session_create_with_and_without_a_ttl() {
        let SessionCommand::Create {
            client_id,
            label,
            ttl_seconds,
        } = session_command(&[
            "create",
            "--client-id",
            "claude-code",
            "--label",
            "lab session",
            "--ttl-seconds",
            "120",
        ])
        .unwrap()
        else {
            panic!("expected create");
        };
        assert_eq!(client_id, "claude-code");
        assert_eq!(label, "lab session");
        assert_eq!(ttl_seconds, Some(120));

        let SessionCommand::Create { ttl_seconds, .. } =
            session_command(&["create", "--client-id", "my_tool", "--label", "x"]).unwrap()
        else {
            panic!("expected create");
        };
        assert_eq!(ttl_seconds, None);
    }

    #[test]
    fn session_create_requires_a_client_id_a_label_and_a_numeric_ttl() {
        assert!(session_command(&["create", "--label", "x"]).is_err());
        assert!(session_command(&["create", "--client-id", "claude-code"]).is_err());
        assert!(
            session_command(&[
                "create",
                "--client-id",
                "claude-code",
                "--label",
                "x",
                "--ttl-seconds",
                "soon",
            ])
            .is_err()
        );
    }

    #[test]
    fn parses_session_list_and_revoke() {
        assert!(matches!(
            session_command(&["list"]).unwrap(),
            SessionCommand::List
        ));
        let SessionCommand::Revoke { id } =
            session_command(&["revoke", "0123456789abcdef"]).unwrap()
        else {
            panic!("expected revoke");
        };
        assert_eq!(id, "0123456789abcdef");
        assert!(session_command(&["revoke"]).is_err());
        assert!(session_command(&[]).is_err());
        assert!(session_command(&["show"]).is_err());
    }

    fn status_error(status: u16, body: &str) -> anyhow::Error {
        anyhow::Error::new(agentdesktop_client::StatusError {
            status,
            body: body.to_owned(),
        })
    }

    #[test]
    fn session_error_message_names_the_session_for_404() {
        assert_eq!(
            session_error_message(&status_error(404, "no session 0123456789abcdef")),
            "no session 0123456789abcdef"
        );
    }

    #[test]
    fn session_error_message_explains_429() {
        assert_eq!(
            session_error_message(&status_error(
                429,
                "too many sessions (64); revoke unused ones"
            )),
            "too many sessions (64); revoke unused ones"
        );
        // The message is the CLI's own, whatever body the daemon sent.
        assert_eq!(
            session_error_message(&status_error(429, "")),
            "too many sessions (64); revoke unused ones"
        );
    }

    #[test]
    fn session_error_message_reports_other_statuses_with_reason_and_body() {
        assert_eq!(
            session_error_message(&status_error(409, "the local LLM proxy is not running")),
            "daemon returned 409 Conflict: the local LLM proxy is not running"
        );
        assert_eq!(
            session_error_message(&status_error(500, "boom")),
            "daemon returned 500 Internal Server Error: boom"
        );
    }

    #[test]
    fn parses_claude_pre_tool_use_input_without_unrelated_fields() {
        let event = parse_claude_pre_tool_use(
            br#"{
                "session_id": "session-secret",
                "transcript_path": "/tmp/transcript.jsonl",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_use_id": "tool-1",
                "tool_input": {"command": "cargo test"}
            }"#,
            true,
        )
        .expect("valid Claude hook input");

        let serialized = serde_json::to_string(&event).expect("serialize telemetry");
        let TelemetryEventKind::ToolUse {
            client_id,
            tool_name,
            tool_use_id,
            tool_input,
        } = &event
        else {
            panic!("expected tool-use telemetry");
        };
        assert_eq!(client_id, "claude-code");
        assert_eq!(tool_name, "Bash");
        assert_eq!(tool_use_id.as_deref(), Some("tool-1"));
        assert_eq!(tool_input.as_ref().unwrap()["command"], "cargo test");
        assert!(!serialized.contains("session-secret"));
        assert!(!serialized.contains("transcript"));
    }
}

#[cfg(test)]
mod session_cli_tests {
    use super::{session_error_message, valid_session_id};

    #[test]
    fn a_session_id_is_sixteen_lowercase_hex_characters() {
        assert!(valid_session_id("0123456789abcdef"));
        for bad in [
            "0123456789ABCDEF",
            "0123456789abcde",
            "0123456789abcdef0",
            "a?b",
            "",
            "a b",
        ] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn an_empty_404_means_an_older_daemon_without_the_session_api() {
        let error = anyhow::Error::new(agentdesktop_client::StatusError {
            status: 404,
            body: String::new(),
        });
        assert_eq!(
            session_error_message(&error),
            "this daemon has no session API (upgrade agentdesktop)"
        );
    }
}
