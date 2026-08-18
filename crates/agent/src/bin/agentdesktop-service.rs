#[cfg(windows)]
mod windows {
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    use agentdesktop_agent::daemon::{self, DaemonArgs};
    use agentdesktop_core::DEFAULT_SOCKET_PATH;
    use anyhow::Context;
    use clap::Parser;
    use tokio::sync::watch;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

    const SERVICE_NAME: &str = "AgentDesktop";

    #[derive(Parser)]
    #[command(about = "Agent Desktop Windows service")]
    struct Args {
        #[arg(long, default_value = DEFAULT_SOCKET_PATH)]
        socket: PathBuf,

        #[command(flatten)]
        daemon: DaemonArgs,
    }

    define_windows_service!(ffi_service_main, service_main);

    pub fn run() -> anyhow::Result<()> {
        service_dispatcher::start(SERVICE_NAME, ffi_service_main)
            .context("start Windows service dispatcher")
    }

    fn service_main(_arguments: Vec<OsString>) {
        if let Err(error) = run_service() {
            eprintln!("Agent Desktop service failed: {error:#}");
        }
    }

    fn run_service() -> anyhow::Result<()> {
        let args = Args::try_parse().context("parse Windows service arguments")?;
        let (shutdown_sender, mut shutdown_receiver) = watch::channel(false);
        let event_handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = shutdown_sender.send(true);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("register Windows service control handler")?;

        status_handle
            .set_service_status(service_status(
                ServiceState::StartPending,
                ServiceControlAccept::empty(),
                ServiceExitCode::Win32(0),
                1,
                Duration::from_secs(10),
            ))
            .context("report Windows service startup")?;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("create Windows service runtime")?;
        status_handle
            .set_service_status(service_status(
                ServiceState::Running,
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                ServiceExitCode::Win32(0),
                0,
                Duration::default(),
            ))
            .context("report Windows service running")?;

        let stopping_status = status_handle.clone();
        let shutdown = async move {
            if !*shutdown_receiver.borrow() {
                shutdown_receiver
                    .changed()
                    .await
                    .context("wait for Windows service stop control")?;
            }
            stopping_status
                .set_service_status(service_status(
                    ServiceState::StopPending,
                    ServiceControlAccept::empty(),
                    ServiceExitCode::Win32(0),
                    1,
                    Duration::from_secs(10),
                ))
                .context("report Windows service stopping")?;
            Ok(())
        };
        let daemon_result = runtime.block_on(daemon::run_until_shutdown(
            args.daemon,
            args.socket,
            shutdown,
        ));
        let exit_code = if daemon_result.is_ok() {
            ServiceExitCode::Win32(0)
        } else {
            ServiceExitCode::ServiceSpecific(1)
        };
        status_handle
            .set_service_status(service_status(
                ServiceState::Stopped,
                ServiceControlAccept::empty(),
                exit_code,
                0,
                Duration::default(),
            ))
            .context("report Windows service stopped")?;
        daemon_result
    }

    fn service_status(
        current_state: ServiceState,
        controls_accepted: ServiceControlAccept,
        exit_code: ServiceExitCode,
        checkpoint: u32,
        wait_hint: Duration,
    ) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state,
            controls_accepted,
            exit_code,
            checkpoint,
            wait_hint,
            process_id: None,
        }
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("agentdesktop-service is only supported on Windows")
}
