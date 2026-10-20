//! Writer-owned tests for the launch module: what the spec-derived baseline
//! (`tests.rs`) leaves to the implementation.

use std::path::{Path, PathBuf};

use super::*;

#[test]
fn the_user_mode_pipe_admits_system_and_the_daemon_user_only() {
    assert_eq!(
        user_mode_pipe_sddl("S-1-5-21-1-2-3-1001"),
        "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1001)"
    );
}

/// The read-only grant for `credential\` is only honoured when the directory
/// exists, so it is created before the request is built.
#[test]
fn the_credential_directory_exists_before_the_request_is_written() {
    let temp = tempfile::tempdir().unwrap();
    let launch_dir = temp.path().join("launches").join("0123456789abcdef");

    create_launch_dir(&launch_dir, "gateway-token").unwrap();

    assert!(launch_dir.join("credential").is_dir());
    assert_eq!(
        std::fs::read_to_string(launch_dir.join("credential").join("token")).unwrap(),
        "gateway-token"
    );
    assert!(launch_dir.join("tmp").is_dir());
    assert!(
        !launch_dir.join("request.json").exists(),
        "the request is built after the directories it grants exist"
    );
}

struct OnlyFiles(Vec<PathBuf>);

impl ProgramFs for OnlyFiles {
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        Ok(path.to_owned())
    }

    fn is_file(&self, path: &Path) -> bool {
        self.0.contains(&path.to_owned())
    }
}

/// A trailing `;` in PATH is an empty directory entry; it must not make every
/// path count as "under a PATH directory".
#[test]
fn an_empty_path_entry_does_not_admit_a_claude_exe_anywhere() {
    let home = PathBuf::from("/home/dev");
    let elsewhere = PathBuf::from("/opt/other/claude.exe");
    let fs = OnlyFiles(vec![elsewhere.clone()]);
    let mut dirs = vec![PathBuf::new(), PathBuf::from("/opt/other")];
    assert_eq!(
        resolve_program(&home, &dirs, &fs),
        Ok(elsewhere),
        "the real PATH entry still resolves"
    );
    dirs.pop();
    assert!(
        resolve_program(&home, &dirs, &fs).is_err(),
        "the empty entry alone admits nothing"
    );
}

#[test]
fn the_executable_configuration_rule_covers_git_commondir_and_the_ide_directory_is_denied() {
    use std::collections::{BTreeMap, BTreeSet};

    let existing: BTreeSet<String> = [r"C:\ws\.git", r"C:\ws\.git\HEAD"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let directories: BTreeSet<String> = [r"C:\ws\.git"].into_iter().map(str::to_owned).collect();
    let exists = |path: &Path| existing.contains(path.to_str().unwrap());
    let is_dir = |path: &Path| directories.contains(path.to_str().unwrap());
    let request = build_request(&LaunchInput {
        launch_id: "0123456789abcdef".into(),
        executable: PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
        client_exe_dir: PathBuf::from(r"C:\Program Files\Agentdesktop"),
        cwd: PathBuf::from(r"C:\ws"),
        args: vec![],
        home: PathBuf::from(r"C:\Users\dev"),
        state_dir: PathBuf::from(r"C:\Users\dev\.local\state\agentdesktop"),
        launch_dir: PathBuf::from(
            r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef",
        ),
        git_dir: None,
        program_files_claude: None,
        gateway_url: "https://gateway.example.com/".into(),
        base_env: BTreeMap::new(),
        exists: &exists,
        is_dir: &is_dir,
    });
    let denied = request.filesystem.denied_paths;
    assert!(denied.contains(&r"C:\ws\.git\commondir".to_owned()));
    assert!(denied.contains(&r"C:\ws\.git\config".to_owned()));
    assert!(denied.contains(&r"C:\Users\dev\.claude\ide".to_owned()));
}

#[cfg(windows)]
#[test]
fn the_current_process_is_alive_only_with_its_own_start_time() {
    use std::os::windows::io::AsRawHandle;

    let pid = std::process::id();
    let handle = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "exit 0"])
        .spawn()
        .map(|mut child| {
            let _ = child.wait();
        });
    assert!(handle.is_ok());
    // The daemon's own process: alive with the true creation time, dead with
    // any other (a reused pid is not the launch).
    let current = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "ping -n 3 127.0.0.1 >NUL"])
        .spawn()
        .unwrap();
    let start = host::windows::creation_time(current.as_raw_handle() as _).unwrap();
    assert!(host::process_alive(current.id(), start));
    assert!(!host::process_alive(current.id(), start.wrapping_add(1)));
    assert!(!host::process_alive(pid, 0));
}

// ------------------------------------------------- handler path (regression)

mod handler {
    //! `POST /v1/launch` end to end with an `exists` that sees `\`-joined
    //! paths the way Windows does: the credential directory must be granted.

    use std::{
        collections::BTreeMap,
        io,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use agentdesktop_core::model::{LaunchRequest, LlmGatewayCredential};
    use async_trait::async_trait;
    use axum::{Json, extract::State, http::StatusCode};

    use super::super::*;

    /// Maps a request path (`\`-joined, possibly mixed) back to the real
    /// filesystem of this platform.
    fn real(path: &Path) -> PathBuf {
        PathBuf::from(
            path.to_string_lossy()
                .replace('\\', std::path::MAIN_SEPARATOR_STR),
        )
    }

    struct Host {
        home: PathBuf,
        state_dir: PathBuf,
    }

    struct Fs;

    impl CwdFs for Fs {
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            std::fs::canonicalize(p)
        }
        fn kind(&self, p: &Path) -> io::Result<PathKind> {
            Ok(PathKind {
                is_dir: std::fs::metadata(p)?.is_dir(),
            })
        }
    }

    impl ProgramFs for Fs {
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            std::fs::canonicalize(p)
        }
        fn is_file(&self, p: &Path) -> bool {
            p.is_file()
        }
    }

    static FS: Fs = Fs;

    impl LaunchHost for Host {
        fn home(&self) -> PathBuf {
            self.home.clone()
        }
        fn path_dirs(&self) -> Vec<PathBuf> {
            vec![]
        }
        fn base_env(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }
        fn cwd_context(&self) -> CwdContext {
            CwdContext {
                home: self.home.clone(),
                state_dir: self.state_dir.clone(),
                system_dirs: vec![],
                denied: vec![],
                app_data: vec![],
            }
        }
        fn cwd_fs(&self) -> &dyn CwdFs {
            &FS
        }
        fn program_fs(&self) -> &dyn ProgramFs {
            &FS
        }
        fn exists(&self, path: &Path) -> bool {
            real(path).exists()
        }
        fn is_dir(&self, path: &Path) -> bool {
            real(path).is_dir()
        }
        fn git_dir(&self) -> Option<PathBuf> {
            None
        }
        fn program_files_claude(&self) -> Option<PathBuf> {
            None
        }
        fn client_exe_dir(&self) -> PathBuf {
            self.state_dir.join("bin")
        }
        fn now(&self) -> u64 {
            1_700_000_000
        }
    }

    struct Credentials;

    #[async_trait]
    impl CredentialSource for Credentials {
        async fn credential(
            &self,
            _client_id: &str,
            _interactive: bool,
        ) -> Result<LlmGatewayCredential, (StatusCode, String)> {
            Ok(LlmGatewayCredential {
                credential: "gateway-token".into(),
                expires_at_unix_seconds: 1_700_003_600,
            })
        }
    }

    pub(super) struct Recorder {
        pub(super) request_json: Mutex<Option<String>>,
        pub(super) home_claude_existed: Mutex<bool>,
        pub(super) home: PathBuf,
    }

    struct Running;

    impl LaunchChild for Running {
        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            Ok(None)
        }
        fn pid(&self) -> u32 {
            1
        }
        fn start_time(&self) -> u64 {
            1
        }
    }

    impl Executor for Recorder {
        fn spawn(
            &self,
            _launch_id: &str,
            _executor: &Path,
            request: &Path,
            _log: &Path,
        ) -> io::Result<Box<dyn LaunchChild>> {
            // Record whether ~/.claude existed at spawn time (n1: ordering).
            *self.home_claude_existed.lock().unwrap() = self.home.join(".claude").is_dir();
            *self.request_json.lock().unwrap() = std::fs::read_to_string(request).ok();
            Ok(Box::new(Running))
        }
    }

    /// A launch state over a fake host rooted in temp directories.
    pub(super) fn state(home: PathBuf, state_dir: PathBuf, executor: Arc<Recorder>) -> LaunchState {
        let daemon_info = serde_json::from_value(serde_json::json!({
            "version": "test", "scope": "user", "configPath": "", "stateDirectory": "",
            "inventoryInterval": "60s", "controller": null,
        }))
        .unwrap();
        let (sender, discovery) =
            tokio::sync::watch::channel(Arc::new(agentdesktop_core::model::Discovery {
                agents: vec![],
                model_runtimes: vec![],
            }));
        std::mem::forget(sender);
        let app = api::AppState {
            config: agentdesktop_core::config::parse_daemon(
                "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  claudeCode: {}\n",
            )
            .unwrap(),
            daemon_info,
            discovery,
            enrollment: crate::enrollment::EnrollmentState::new(false),
            controller_status: None,
            state_dir: state_dir.clone(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
            intercept: None,
        };
        LaunchState {
            app,
            executor,
            credentials: Arc::new(Credentials),
            host: Arc::new(Host { home, state_dir }),
            registry: LaunchRegistry::new(),
            availability: Some(ContainmentInfo {
                executor: "wxc-exec.exe".into(),
                available: true,
                error: None,
            }),
            daemon_sid: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_credential_directory_is_granted_read_only_to_the_box() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let home = root.join("home");
        let cwd = home.join("work");
        let state_dir = root.join("state");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(home.join(".local").join("bin")).unwrap();
        std::fs::write(home.join(".local").join("bin").join("claude.exe"), b"MZ").unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();
        let recorder = Arc::new(Recorder {
            request_json: Mutex::new(None),
            home_claude_existed: Mutex::new(false),
            home: home.clone(),
        });
        let state = state(home.clone(), state_dir.clone(), recorder.clone());

        let (status, Json(launch)) = launch(
            State(state),
            Json(LaunchRequest {
                program: "claude-code".into(),
                cwd: cwd.clone(),
                args: vec![],
            }),
        )
        .await
        .expect("the launch succeeds");

        assert_eq!(status, StatusCode::CREATED);
        let request: serde_json::Value =
            serde_json::from_str(recorder.request_json.lock().unwrap().as_deref().unwrap())
                .unwrap();
        let launch_dir = state_dir.join("launches").join(&launch.launch_id);
        let credential_dir = launch_dir
            .join("credential")
            .to_string_lossy()
            .replace('/', "\\");
        let readonly: Vec<String> = request["filesystem"]["readonlyPaths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert!(
            readonly.contains(&credential_dir),
            "credential directory {credential_dir} must be granted; got {readonly:?}"
        );
        assert!(launch_dir.join("credential").join("token").is_file());
        // `~\.claude` did not exist before the launch; its entries need it.
        assert!(
            *recorder.home_claude_existed.lock().unwrap(),
            "~\\.claude did not exist when the executor started"
        );
        assert!(
            home.join(".claude").is_dir(),
            "~\\.claude is created before the request"
        );
    }

    /// The user's git configuration is granted read-only when it exists (m-a).
    #[tokio::test]
    async fn the_git_configuration_is_granted_when_it_exists() {
        use std::collections::{BTreeMap, BTreeSet};

        let existing: BTreeSet<String> = [r"C:\Users\dev\.gitconfig"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let exists = |path: &Path| existing.contains(path.to_str().unwrap());
        let is_dir = |_: &Path| false;
        let request = build_request(&LaunchInput {
            launch_id: "0123456789abcdef".into(),
            executable: PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
            client_exe_dir: PathBuf::from(r"C:\Program Files\Agentdesktop"),
            cwd: PathBuf::from(r"C:\ws"),
            args: vec![],
            home: PathBuf::from(r"C:\Users\dev"),
            state_dir: PathBuf::from(r"C:\Users\dev\.local\state\agentdesktop"),
            launch_dir: PathBuf::from(
                r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef",
            ),
            git_dir: None,
            program_files_claude: None,
            gateway_url: "https://gateway.example.com/".into(),
            base_env: BTreeMap::new(),
            exists: &exists,
            is_dir: &is_dir,
        });
        let readonly = &request.filesystem.readonly_paths;
        assert!(
            readonly.contains(&r"C:\Users\dev\.gitconfig".to_owned()),
            "~\\.gitconfig is granted when it exists; got {readonly:?}"
        );
        assert!(
            !readonly.contains(&r"C:\Users\dev\.config\git\config".to_owned()),
            "~\\.config\\git\\config is not in readonlyPaths when it does not exist"
        );
        assert!(
            !request
                .filesystem
                .denied_paths
                .contains(&r"C:\Users\dev\.config\git\config".to_owned()),
            "~\\.config\\git\\config is not in deniedPaths when it does not exist"
        );
    }
}

#[test]
fn a_canonical_workspace_with_a_percent_sign_is_rejected() {
    use std::collections::BTreeMap;

    struct Mapped(BTreeMap<PathBuf, PathBuf>);
    impl CwdFs for Mapped {
        fn canonicalize(&self, p: &Path) -> std::io::Result<PathBuf> {
            Ok(self.0.get(p).cloned().unwrap_or_else(|| p.to_owned()))
        }
        fn kind(&self, _p: &Path) -> std::io::Result<PathKind> {
            Ok(PathKind { is_dir: true })
        }
    }
    let ctx = CwdContext {
        home: PathBuf::from("/home/dev"),
        state_dir: PathBuf::from("/state"),
        system_dirs: vec![],
        denied: vec![],
        app_data: vec![],
    };
    let fs = Mapped(
        [(
            PathBuf::from("/home/dev/link"),
            PathBuf::from("/home/dev/50%off"),
        )]
        .into_iter()
        .collect(),
    );
    assert!(validate_cwd(Path::new("/home/dev/link"), &ctx, &fs).is_err());
    assert!(validate_cwd(Path::new("/home/dev/plain"), &ctx, &fs).is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_resolved_program_path_with_a_percent_sign_is_rejected() {
    use std::sync::{Arc, Mutex};

    use agentdesktop_core::model::LaunchRequest;
    use axum::{Json, extract::State, http::StatusCode};

    // The home (and so `<home>\.local\bin\claude.exe`) carries a `%`; the
    // workspace lives outside it, so the workspace rules pass first.
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let home = root.join("h%me");
    let cwd = root.join("work");
    let state_dir = root.join("state");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(home.join(".local").join("bin")).unwrap();
    std::fs::write(home.join(".local").join("bin").join("claude.exe"), b"MZ").unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let state = handler::state(
        home.clone(),
        state_dir,
        Arc::new(handler::Recorder {
            request_json: Mutex::new(None),
            home_claude_existed: Mutex::new(false),
            home: home.clone(),
        }),
    );

    let error = launch(
        State(state),
        Json(LaunchRequest {
            program: "claude-code".into(),
            cwd,
            args: vec![],
        }),
    )
    .await
    .expect_err("a program path cmd.exe would interpret is refused");

    assert_eq!(error.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        error.1,
        "program path contains a character cmd.exe interprets"
    );
}

#[test]
fn the_environment_is_a_list_of_key_value_entries_and_a_missing_workspace_claude_folder_is_denied_whole()
 {
    use std::collections::BTreeMap;

    let exists = |_: &Path| false;
    let is_dir = |_: &Path| false;
    let request = build_request(&LaunchInput {
        launch_id: "0123456789abcdef".into(),
        executable: PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
        client_exe_dir: PathBuf::from(r"C:\Program Files\Agentdesktop"),
        cwd: PathBuf::from(r"C:\ws"),
        args: vec![],
        home: PathBuf::from(r"C:\Users\dev"),
        state_dir: PathBuf::from(r"C:\Users\dev\.local\state\agentdesktop"),
        launch_dir: PathBuf::from(
            r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef",
        ),
        git_dir: None,
        program_files_claude: None,
        gateway_url: "https://gateway.example.com/".into(),
        base_env: BTreeMap::from([("SystemRoot".to_owned(), r"C:\Windows".to_owned())]),
        exists: &exists,
        is_dir: &is_dir,
    });
    // MXC 1.0.0 takes `env` as `KEY=VALUE` strings, not an object (lab L2).
    assert!(request.process.env.iter().all(|entry| entry.contains('=')));
    assert!(
        request
            .process
            .env
            .contains(&"TERM=xterm-256color".to_owned())
    );
    let json = serde_json::to_value(&request).unwrap();
    assert!(json["process"]["env"].is_array());
    // MXC refuses a denied path under a missing folder (lab L2, 0x80070003):
    // the folder is denied instead of its entries.
    let denied = request.filesystem.denied_paths;
    assert!(denied.contains(&r"C:\ws\.claude".to_owned()));
    assert!(
        !denied
            .iter()
            .any(|path| path.starts_with(r"C:\ws\.claude\"))
    );
    // Entries whose parent is granted read-write stay individual.
    assert!(denied.contains(&r"C:\Users\dev\.claude\settings.json".to_owned()));
}

#[test]
fn the_box_environment_has_home_set_to_the_daemon_home_when_daemon_env_has_no_home() {
    use std::collections::BTreeMap;

    let exists = |_: &Path| false;
    let is_dir = |_: &Path| false;
    let request = build_request(&LaunchInput {
        launch_id: "0123456789abcdef".into(),
        executable: PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
        client_exe_dir: PathBuf::from(r"C:\Program Files\Agentdesktop"),
        cwd: PathBuf::from(r"C:\ws"),
        args: vec![],
        home: PathBuf::from(r"C:\Users\dev"),
        state_dir: PathBuf::from(r"C:\Users\dev\.local\state\agentdesktop"),
        launch_dir: PathBuf::from(
            r"C:\Users\dev\.local\state\agentdesktop\launches\0123456789abcdef",
        ),
        git_dir: None,
        program_files_claude: None,
        gateway_url: "https://gateway.example.com/".into(),
        base_env: BTreeMap::new(),
        exists: &exists,
        is_dir: &is_dir,
    });
    assert!(
        request
            .process
            .env
            .contains(&r"HOME=C:\Users\dev".to_owned()),
        "HOME must be set in box env even when daemon env has no HOME; got {:?}",
        request.process.env
    );
}
