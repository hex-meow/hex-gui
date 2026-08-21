//! Tauri entry point for hexmeow GUI.
//!
//! Wires the [`AppState`] into Tauri-managed state and registers every
//! `#[tauri::command]` defined in [`commands`].

mod analyzer;
mod authenticity;
mod backend;
mod calibration_transport;
mod calibration_update;
mod can_lease;
mod cobs_can_iap_profiles;
mod commands;
mod damiao;
mod device_registry;
mod dfu_gate;
mod diag;
mod dto;
mod friction_calibration;
mod hopea3;
mod hpm_dfu;
mod imu;
mod lift;
mod lift_commission;
mod logging;
mod meow_calibration;
mod motor_factory_backup;
mod rollercan;
mod rollercan_control;
mod sdo_client;
mod smartknob;
mod state;
mod stm32_can_dfu;
mod stm32_can_profiles;
mod torque_calibration;
mod unified_smartknob;
mod zenoh_arm;
mod zenoh_base;
mod zenoh_config;
mod zenoh_discovery;
mod zenoh_ee;
mod zenoh_hw;
mod zenoh_lease;
mod zenoh_lift;
mod zenoh_linklocal;
mod zenoh_mdns;
mod zenoh_wifi;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use state::AppState;
use tauri::{Emitter, Manager};

/// Time budget for the best-effort safe stop on window close. Long enough for a
/// clean confirmed detach on a healthy bus, short enough that a dead bus doesn't
/// make closing the GUI feel stuck.
const LIFT_CLOSE_STOP_BUDGET: Duration = Duration::from_millis(1_500);
const SAFE_SHUTDOWN_BUDGET: Duration = Duration::from_secs(30);
const SHUTDOWN_IDLE: u8 = 0;
const SHUTDOWN_RUNNING: u8 = 1;
const SHUTDOWN_COMPLETE: u8 = 2;

#[derive(Clone, Copy)]
enum ShutdownBlocker {
    Dfu,
    DeviceSettings,
}

fn shutdown_blocker<R: tauri::Runtime>(
    app_handle: &tauri::AppHandle<R>,
) -> Option<ShutdownBlocker> {
    let mutation_gate = app_handle.state::<dfu_gate::DfuMutationGate>();
    let hpm_dfu = app_handle.state::<hpm_dfu::DfuState>();
    let can_dfu = app_handle.state::<stm32_can_dfu::CanDfuState>();
    if mutation_gate.is_active() || hpm_dfu.is_active() || can_dfu.is_active() {
        return Some(ShutdownBlocker::Dfu);
    }
    if app_handle
        .state::<AppState>()
        .device_settings_operation
        .is_active()
    {
        return Some(ShutdownBlocker::DeviceSettings);
    }
    None
}

/// Stop every active hardware application before the native window exits.
/// Lift keeps its upstream 1.5 s fail-safe budget; the remaining CANopen and
/// RollerCAN cleanup is bounded by the shared 30 s last-resort guard.
fn begin_safe_shutdown<R: tauri::Runtime>(app_handle: &tauri::AppHandle<R>, phase: &Arc<AtomicU8>) {
    if phase
        .compare_exchange(
            SHUTDOWN_IDLE,
            SHUTDOWN_RUNNING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return;
    }

    app_handle
        .state::<AppState>()
        .shutdown_requested
        .store(true, Ordering::SeqCst);

    let app_handle = app_handle.clone();
    let phase = phase.clone();
    tauri::async_runtime::spawn(async move {
        let state = app_handle.state::<AppState>();
        let cleanup = async {
            match tokio::time::timeout(LIFT_CLOSE_STOP_BUDGET, commands::stop_lift_session(&state))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!(
                    "lift stop on close reported {error}; continuing remaining safe cleanup"
                ),
                Err(_) => log::warn!(
                    "lift stop on close timed out after {} ms; continuing remaining safe cleanup",
                    LIFT_CLOSE_STOP_BUDGET.as_millis()
                ),
            }
            commands::disconnect_state(&state).await;
        };
        if tokio::time::timeout(SAFE_SHUTDOWN_BUDGET, cleanup)
            .await
            .is_err()
        {
            log::error!(
                "safe shutdown timed out after {} seconds; forcing application exit",
                SAFE_SHUTDOWN_BUDGET.as_secs()
            );
        }
        phase.store(SHUTDOWN_COMPLETE, Ordering::SeqCst);
        app_handle.exit(0);
    });
}

fn request_safe_close(window: tauri::Window, phase: &Arc<AtomicU8>) {
    match shutdown_blocker(window.app_handle()) {
        Some(ShutdownBlocker::Dfu) => {
            log::warn!("window close blocked while a DFU command is active");
            let _ = window.emit("dfu-close-blocked", ());
        }
        Some(ShutdownBlocker::DeviceSettings) => {
            log::warn!("window close blocked while a device-settings command is active");
            let _ = window.emit("device-settings-close-blocked", ());
        }
        None => begin_safe_shutdown(window.app_handle(), phase),
    }
}

pub fn run() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,hex_motor=info,hex_motor_gui_lib=info"),
    )
    .try_init();
    let _timer_resolution = request_timer_resolution();

    let shutdown_phase = Arc::new(AtomicU8::new(SHUTDOWN_IDLE));
    let close_phase = shutdown_phase.clone();
    let app = tauri::Builder::default()
        .manage(AppState::default())
        .manage(can_lease::CanTransportGate::default())
        .manage(dfu_gate::DfuMutationGate::default())
        .manage(hpm_dfu::DfuState::default())
        .manage(stm32_can_dfu::CanDfuState::default())
        .invoke_handler(tauri::generate_handler![
            commands::connect,
            commands::disconnect,
            commands::is_connected,
            commands::list_devices,
            authenticity::authenticity_inspect,
            authenticity::authenticity_verify_online,
            authenticity::authenticity_register,
            calibration_update::calibration_update_prepare,
            calibration_update::calibration_update_preview,
            calibration_update::calibration_update_write,
            calibration_update::calibration_update_verify_persisted,
            commands::identify,
            commands::initialize,
            commands::initialize_all,
            commands::set_mode,
            commands::set_target,
            commands::set_max_torque,
            commands::disable,
            commands::clear_error,
            commands::meow_identify,
            commands::meow_get_status,
            commands::meow_initialize,
            commands::meow_read_torque_factor,
            commands::meow_activate_target,
            commands::meow_set_target,
            commands::meow_set_max_torque,
            commands::meow_set_profile_limits,
            commands::meow_disable,
            commands::meow_clear_error,
            commands::meow_start_log,
            commands::meow_apply_can_settings,
            commands::friction_calibration_start,
            commands::friction_calibration_get,
            commands::friction_calibration_stop,
            commands::torque_calibration_start,
            commands::torque_calibration_acceptance_start,
            commands::torque_calibration_get,
            commands::torque_calibration_stop,
            commands::apply_device_settings,
            commands::forget_offline,
            commands::set_position_preset,
            commands::read_position,
            commands::get_status,
            commands::damiao_list_devices,
            commands::damiao_safe_rescan,
            commands::damiao_attach,
            commands::damiao_detach,
            commands::damiao_get_state,
            commands::damiao_set_mode,
            commands::damiao_enable,
            commands::damiao_disable,
            commands::damiao_disable_all,
            commands::damiao_clear_fault,
            commands::damiao_set_zero,
            commands::damiao_send_target,
            commands::damiao_stop_stream,
            commands::rollercan_control_list_devices,
            commands::rollercan_control_rescan,
            commands::rollercan_control_attach,
            commands::rollercan_control_detach,
            commands::rollercan_control_get_state,
            commands::rollercan_control_set_mode,
            commands::rollercan_control_enable,
            commands::rollercan_control_disable,
            commands::rollercan_control_release_stall,
            commands::rollercan_control_send_target,
            commands::rollercan_control_set_current_limit,
            commands::rollercan_control_refresh,
            commands::start_log,
            commands::stop_log,
            commands::hopea3_start,
            commands::hopea3_init_progress,
            commands::hopea3_stop,
            commands::hopea3_set_cmd,
            commands::hopea3_set_max_torque,
            commands::hopea3_set_kd,
            commands::hopea3_set_limits,
            commands::hopea3_set_accel_limits,
            commands::hopea3_clear_errors,
            commands::hopea3_reinit_motor,
            commands::hopea3_reset_odom,
            commands::hopea3_get_state,
            commands::lift_start,
            commands::lift_stop,
            commands::lift_get_state,
            commands::lift_refresh,
            commands::lift_set_nmt,
            commands::lift_disable,
            commands::lift_home,
            commands::lift_clear_fault,
            commands::lift_set_velocity,
            commands::lift_renew_velocity,
            commands::lift_set_position,
            commands::lift_factory_calibration_arm,
            commands::lift_factory_calibration_seek_lower,
            commands::lift_factory_calibration_seek_upper,
            commands::lift_factory_calibration_abort,
            commands::lift_factory_calibration_clear_fault,
            commands::lift_factory_calibration_commit,
            commands::lift_commission_arm,
            commands::lift_commission_hold,
            commands::lift_commission_renew,
            commands::lift_commission_release,
            commands::lift_commission_disarm,
            commands::lift_commission_clear_fault,
            commands::lift_commission_epoch_service,
            commands::lift_commission_estop,
            commands::lift_commission_csv,
            commands::smartknob_configs,
            commands::smartknob_monitor_start,
            commands::smartknob_monitor_stop,
            commands::smartknob_list_devices,
            commands::smartknob_get_profile,
            commands::smartknob_probe,
            commands::smartknob_start,
            commands::smartknob_stop,
            commands::smartknob_set_config,
            commands::smartknob_set_tuning,
            commands::smartknob_clear_error,
            commands::smartknob_get_state,
            commands::smartknob_set_custom_config,
            commands::smartknob_set_telemetry,
            commands::imu_start,
            commands::imu_stop,
            commands::imu_get_state,
            commands::imu_bias_trim,
            commands::imu_yaw_reset,
            commands::analyzer_start,
            commands::analyzer_stop,
            commands::analyzer_bus_state,
            commands::analyzer_get_trace,
            commands::analyzer_get_aggregates,
            commands::analyzer_get_status,
            commands::analyzer_clear,
            commands::analyzer_send,
            commands::analyzer_sdo_read,
            commands::analyzer_sdo_write,
            commands::zenoh_connect,
            commands::zenoh_disconnect,
            commands::zenoh_discover,
            commands::zenoh_acquire,
            commands::zenoh_set_active,
            commands::zenoh_set_cmd,
            commands::zenoh_get_state,
            commands::zenoh_get_limits,
            commands::zenoh_set_limits,
            commands::zenoh_release,
            commands::zenoh_set_diag_focus,
            commands::zenoh_refresh_diag,
            commands::zenoh_get_events,
            commands::zenoh_get_logs,
            commands::zenoh_clear_fault,
            commands::ee_connect,
            commands::ee_disconnect,
            commands::ee_discover,
            commands::ee_discover_all,
            commands::hardware_snapshot,
            commands::ee_acquire,
            commands::ee_set_focus,
            commands::ee_goto,
            commands::ee_set_mode,
            commands::ee_set_estop_behavior,
            commands::ee_clear_fault,
            commands::ee_get_state,
            commands::ee_release,
            commands::ee_scene,
            commands::console_get_urdf,
            commands::ee_machines,
            commands::zlift_connect,
            commands::zlift_disconnect,
            commands::zlift_discover,
            commands::zlift_set_focus,
            commands::zlift_acquire,
            commands::zlift_home,
            commands::zlift_goto,
            commands::zlift_jog,
            commands::zlift_set_mode,
            commands::zlift_set_limits,
            commands::zlift_clear_fault,
            commands::zlift_get_state,
            commands::zlift_release,
            commands::zlift_refresh_diag,
            commands::zlift_get_events,
            commands::zlift_get_logs,
            commands::wifi_discover,
            commands::wifi_status,
            commands::wifi_scan,
            commands::wifi_networks,
            commands::wifi_validate,
            commands::wifi_set,
            commands::wifi_forget,
            commands::wifi_forget_all,
            commands::wifi_job,
            commands::arm_connect,
            commands::arm_disconnect,
            commands::arm_discover,
            commands::arm_acquire,
            commands::arm_set_mode,
            commands::arm_set_gravity,
            commands::arm_goto,
            commands::arm_get_state,
            commands::arm_get_urdf,
            commands::arm_release,
            commands::arm_set_diag_focus,
            commands::arm_refresh_diag,
            commands::arm_get_events,
            commands::arm_get_logs,
            commands::arm_clear_fault,
            commands::discover_direct_controllers,
            commands::local_scope_map,
            commands::config_connect,
            commands::config_disconnect,
            commands::config_discover,
            commands::config_get,
            commands::config_validate,
            commands::config_set,
            commands::config_restart,
            hpm_dfu::hpm_dfu_probe,
            hpm_dfu::hpm_dfu_prepare,
            hpm_dfu::hpm_dfu_start,
            hpm_dfu::hpm_dfu_cancel,
            hpm_dfu::hpm_dfu_leave,
            stm32_can_dfu::stm32_can_dfu_discover,
            stm32_can_dfu::stm32_can_dfu_select,
            stm32_can_dfu::stm32_can_dfu_prepare,
            stm32_can_dfu::stm32_can_dfu_acknowledge_manual,
            stm32_can_dfu::stm32_can_dfu_prepare_latest,
            stm32_can_dfu::stm32_can_dfu_start,
            stm32_can_dfu::stm32_can_dfu_cancel,
            stm32_can_dfu::stm32_can_dfu_leave,
        ])
        .on_window_event(move |window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if close_phase.load(Ordering::SeqCst) != SHUTDOWN_COMPLETE {
                    api.prevent_close();
                    request_safe_close(window.clone(), &close_phase);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    let run_phase = shutdown_phase;
    app.run(move |app_handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if run_phase.load(Ordering::SeqCst) != SHUTDOWN_COMPLETE {
                api.prevent_exit();
                match shutdown_blocker(app_handle) {
                    Some(ShutdownBlocker::Dfu) => {
                        log::warn!("application exit blocked while a DFU command is active");
                    }
                    Some(ShutdownBlocker::DeviceSettings) => {
                        log::warn!(
                            "application exit blocked while a device-settings command is active"
                        );
                    }
                    None => begin_safe_shutdown(app_handle, &run_phase),
                }
            }
        }
    });
}

#[cfg(windows)]
struct TimerResolutionGuard;

#[cfg(windows)]
impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}

#[cfg(windows)]
fn request_timer_resolution() -> Option<TimerResolutionGuard> {
    let result = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) };
    if result == 0 {
        log::info!("Windows timer resolution requested at 1 ms");
        Some(TimerResolutionGuard)
    } else {
        log::warn!("Windows timeBeginPeriod(1) failed: {result}");
        None
    }
}

#[cfg(not(windows))]
fn request_timer_resolution() {}
