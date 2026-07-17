//! Tauri entry point for the hex-motor GUI.
//!
//! Wires the [`AppState`] into Tauri-managed state and registers every
//! `#[tauri::command]` defined in [`commands`].

mod analyzer;
mod backend;
mod commands;
mod device_registry;
mod diag;
mod dto;
mod hopea3;
mod imu;
mod lift;
mod lift_commission;
mod logging;
mod rollercan;
mod sdo_client;
mod smartknob;
mod state;
mod unified_smartknob;
mod zenoh_arm;
mod zenoh_base;
mod zenoh_config;
mod zenoh_ee;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use state::AppState;
use tauri::Manager;

/// Time budget for the best-effort safe stop on window close. Long enough for a
/// clean confirmed detach on a healthy bus, short enough that a dead bus doesn't
/// make closing the GUI feel stuck.
const LIFT_CLOSE_STOP_BUDGET: Duration = Duration::from_millis(1_500);
const SAFE_SHUTDOWN_BUDGET: Duration = Duration::from_secs(30);
const SHUTDOWN_IDLE: u8 = 0;
const SHUTDOWN_RUNNING: u8 = 1;
const SHUTDOWN_COMPLETE: u8 = 2;

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
            match tokio::time::timeout(
                LIFT_CLOSE_STOP_BUDGET,
                commands::stop_lift_session(&state),
            )
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
        .invoke_handler(tauri::generate_handler![
            commands::connect,
            commands::disconnect,
            commands::is_connected,
            commands::list_devices,
            commands::identify,
            commands::initialize,
            commands::initialize_all,
            commands::set_mode,
            commands::set_target,
            commands::set_max_torque,
            commands::disable,
            commands::clear_error,
            commands::change_node_id,
            commands::forget_offline,
            commands::set_position_preset,
            commands::read_position,
            commands::get_status,
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
            commands::config_connect,
            commands::config_disconnect,
            commands::config_discover,
            commands::config_get,
            commands::config_validate,
            commands::config_set,
            commands::config_restart,
        ])
        .on_window_event(move |window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if close_phase.load(Ordering::SeqCst) != SHUTDOWN_COMPLETE {
                    api.prevent_close();
                    begin_safe_shutdown(window.app_handle(), &close_phase);
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
                begin_safe_shutdown(app_handle, &run_phase);
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
