//! Tests for the POSIX launch plan and the MXC bubblewrap request.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use super::engine::{Engine, mxc::MxcEngine};
use super::plan::{ConsolePlan, PathStyle};
use super::*;

const HOME: &str = "/home/dev";
const LAUNCH_DIR: &str = "/home/dev/.local/state/agentdesktop/launches/0123456789abcdef";

fn relay() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 4000))
}

fn posix_plan(
    existing: &[&str],
    egress: agentdesktop_core::config::ContainmentEgress,
    args: &[&str],
) -> plan::LaunchPlan {
    let existing: BTreeSet<String> = existing.iter().map(|path| (*path).to_owned()).collect();
    let exists = |path: &Path| existing.contains(path.to_str().unwrap());
    let is_dir = |path: &Path| path.to_str() == Some("/ws/.git");
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    plan_program(
        &LaunchInput {
            style: PathStyle::Posix,
            host_proxy: Some(relay()),
            launch_id: "0123456789abcdef".into(),
            executable: PathBuf::from("/home/dev/.local/share/claude/versions/2.1.0"),
            client_exe_dir: PathBuf::from("/usr/local/bin"),
            cwd: PathBuf::from("/ws"),
            args: check_args_for(PathStyle::Posix, &args).unwrap(),
            home: PathBuf::from(HOME),
            state_dir: PathBuf::from("/home/dev/.local/state/agentdesktop"),
            launch_dir: PathBuf::from(LAUNCH_DIR),
            git_dir: None,
            program_files_claude: None,
            egress,
            relay_base_url: crate::llm_proxy::sessions::sandbox_base_url(
                MxcEngine.box_relay(PathStyle::Posix, relay()),
            ),
            pairing: "test-pairing".into(),
            base_env: BTreeMap::from([("LANG".to_owned(), "C.UTF-8".to_owned())]),
            ssh_enabled: false,
            exists: &exists,
            is_dir: &is_dir,
            console: ConsolePlan::Window,
        },
        &profile::CLAUDE_CODE,
        None,
    )
}

fn deny() -> agentdesktop_core::config::ContainmentEgress {
    agentdesktop_core::config::ContainmentEgress::Deny
}

#[test]
fn the_box_reaches_the_relay_at_the_gateway_and_windows_keeps_loopback() {
    assert_eq!(
        MxcEngine.box_relay(PathStyle::Posix, relay()).to_string(),
        "10.0.2.2:4000"
    );
    assert_eq!(MxcEngine.box_relay(PathStyle::Windows, relay()), relay());
}

#[test]
fn a_posix_plan_writes_posix_paths_and_environment() {
    let plan = posix_plan(&["/ws/.git", "/home/dev/.local/share/claude"], deny(), &[]);
    let env = &plan.env;
    assert_eq!(
        env["PATH"],
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/dev/.local/share/claude/versions:/usr/local/bin"
    );
    assert_eq!(env["HOME"], HOME);
    assert_eq!(env["TMPDIR"], format!("{LAUNCH_DIR}/tmp"));
    assert_eq!(env["CLAUDE_CONFIG_DIR"], "/home/dev/.claude");
    assert_eq!(env["GIT_CONFIG_GLOBAL"], format!("{LAUNCH_DIR}/git_config"));
    assert_eq!(env["ANTHROPIC_BASE_URL"], "http://10.0.2.2:4000/sandbox");
    assert_eq!(env["LANG"], "C.UTF-8");
    for windows_only in ["TEMP", "TMP", "CLAUDE_CODE_USE_POWERSHELL_TOOL"] {
        assert!(!env.contains_key(windows_only), "{windows_only}");
    }
    let all = plan
        .readwrite
        .iter()
        .chain(&plan.readonly)
        .chain(&plan.denied);
    for path in all {
        let text = path.to_str().unwrap();
        assert!(text.starts_with('/') && !text.contains('\\'), "{text}");
    }
    assert!(plan.readwrite.contains(&PathBuf::from("/ws")));
    assert!(plan.readwrite.contains(&PathBuf::from("/home/dev/.claude")));
    assert!(
        plan.denied
            .contains(&PathBuf::from("/home/dev/.claude/.credentials.json"))
    );
    assert!(plan.denied.contains(&PathBuf::from("/ws/.git/config")));
    assert!(plan.denied.contains(&PathBuf::from("/home/dev/.gitconfig")));
}

#[test]
fn the_bubblewrap_request_names_the_relay_as_the_only_way_out() {
    let plan = posix_plan(&["/ws/.git"], deny(), &["-p", "it's a test"]);
    let bytes = MxcEngine.request(&plan, "0123456789abcdef").unwrap();
    let request: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(request["version"], "1.0.0");
    assert_eq!(request["containment"], "bubblewrap");
    assert_eq!(request["containerId"], "agentdesktop-0123456789abcdef");
    assert_eq!(
        request["process"]["commandLine"],
        r"exec '/home/dev/.local/share/claude/versions/2.1.0' '-p' 'it'\''s a test'"
    );
    assert_eq!(request["process"]["cwd"], "/ws");
    assert_eq!(request["process"]["inheritDefaultEnv"], false);
    assert_eq!(
        request["network"],
        serde_json::json!({
            "egress": { "default": "deny" },
            "ingress": { "default": "deny", "hostLoopback": "deny" }
        })
    );
    assert_eq!(
        request["runtimeConfig"],
        serde_json::json!({ "networkProxy": "http://127.0.0.1:4000" })
    );
    assert!(request.get("ui").is_none());
}

#[test]
fn a_bubblewrap_request_refuses_open_egress_and_a_missing_relay() {
    let open = posix_plan(
        &[],
        agentdesktop_core::config::ContainmentEgress::Allow,
        &[],
    );
    let error = MxcEngine.request(&open, "0").unwrap_err();
    assert!(format!("{error:#}").contains("egress: allow"), "{error:#}");
    let mut no_relay = posix_plan(&[], deny(), &[]);
    no_relay.host_proxy = None;
    assert!(MxcEngine.request(&no_relay, "0").is_err());
}

#[test]
fn posix_arguments_are_single_quoted_and_control_characters_refused() {
    let quoted = check_args_for(
        PathStyle::Posix,
        &["a b".to_owned(), "$(x)".to_owned(), String::new()],
    );
    assert_eq!(
        quoted,
        Ok(vec![
            "'a b'".to_owned(),
            "'$(x)'".to_owned(),
            "''".to_owned()
        ])
    );
    assert!(check_args_for(PathStyle::Posix, &["a\nb".to_owned()]).is_err());
}

#[test]
fn the_program_has_no_exe_suffix_on_a_posix_host() {
    struct Fs;
    impl ProgramFs for Fs {
        fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
            Ok(path.to_owned())
        }
        fn is_file(&self, path: &Path) -> bool {
            path == Path::new("/home/dev/.local/bin/claude")
        }
    }
    assert_eq!(
        resolve_program_styled(
            &profile::CLAUDE_CODE,
            Path::new(HOME),
            &[],
            &Fs,
            PathStyle::Posix
        ),
        Ok(PathBuf::from("/home/dev/.local/bin/claude"))
    );
    assert!(
        resolve_program_styled(
            &profile::CLAUDE_CODE,
            Path::new(HOME),
            &[],
            &Fs,
            PathStyle::Windows
        )
        .is_err()
    );
}
