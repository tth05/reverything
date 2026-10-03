//! Running under the service control manager, and installing/removing the service.

use std::ffi::{OsStr, OsString};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eyre::{Context, Result};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::{logger, security, App};

pub const SERVICE_NAME: &str = "Reverything";
const DISPLAY_NAME: &str = "Reverything Index";
const DESCRIPTION: &str =
    "Keeps an index of all files on the NTFS volumes up to date for the Reverything search.";

windows_service::define_windows_service!(ffi_service_main, service_main);

/// Hands the process to the service control manager. Fails if not started as a service.
pub fn run_dispatcher() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .with_context(|| "Not started by the service control manager")
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(e) = run_service() {
        log::error!("Service failed: {:#}", e);
    }
}

fn run_service() -> Result<()> {
    let data_dir = security::data_dir()?;
    logger::init_file(&data_dir.join("service.log"));
    log::info!("Starting service {}", env!("CARGO_PKG_VERSION"));

    let (stop_tx, stop_rx) = mpsc::channel();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let report = |state, controls, wait_hint| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: controls,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint,
            process_id: None,
        })
    };

    report(
        ServiceState::Running,
        ServiceControlAccept::STOP | ServiceControlAccept::PRESHUTDOWN,
        Duration::ZERO,
    )?;
    let app = App::start(data_dir, reverything_protocol::PIPE_NAME.to_string(), false)?;

    let _ = stop_rx.recv();
    log::info!("Stopping service");
    report(
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(20),
    )?;
    // Brings the indices up to date and saves them, the only save while the service runs
    app.set.shutdown();
    report(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::ZERO,
    )?;
    Ok(())
}

/// Registers the service for the current executable and starts it.
pub fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .with_context(|| "Failed to connect to the service manager (run as administrator)")?;

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec!["--service".into()],
        dependencies: vec![],
        // LocalSystem, which is needed to read raw volumes
        account_name: None,
        account_password: None,
    };
    let access = ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::QUERY_STATUS;
    // Installing over an existing service (an upgrade) updates it instead
    let service = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            stop_and_wait(&service)?;
            service
                .change_config(&info)
                .with_context(|| "Failed to update the service")?;
            service
        }
        Err(_) => manager
            .create_service(&info, access)
            .with_context(|| "Failed to create the service")?,
    };
    service.set_description(DESCRIPTION)?;
    // Saving at shutdown can take a few seconds, the default allows only 10
    service.set_preshutdown_timeout(Duration::from_secs(30))?;
    // Restart after crashes
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            };
            3
        ]),
    })?;
    service
        .start(&[] as &[&OsStr])
        .with_context(|| "Failed to start the service")?;
    println!("Installed and started the {} service", SERVICE_NAME);
    Ok(())
}

/// Stops the service if it is installed and running. Used by the installer before upgrades.
pub fn stop() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .with_context(|| "Failed to connect to the service manager (run as administrator)")?;
    match manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP,
    ) {
        Ok(service) => stop_and_wait(&service),
        // Not installed, nothing to stop
        Err(_) => Ok(()),
    }
}

/// Stops and removes the service.
pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .with_context(|| "Failed to connect to the service manager (run as administrator)")?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .with_context(|| "The service is not installed")?;

    stop_and_wait(&service)?;
    service.delete()?;
    println!("Removed the {} service", SERVICE_NAME);
    Ok(())
}

/// Stops the service if it runs and waits until it saved its indices and exited.
fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    service.stop()?;
    let t = Instant::now();
    while service.query_status()?.current_state != ServiceState::Stopped {
        if t.elapsed() > Duration::from_secs(30) {
            eyre::bail!("The service did not stop within 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
