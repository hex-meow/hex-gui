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
mod sdo_client;
mod smartknob;
mod state;
mod zenoh_arm;
mod zenoh_base;
mod zenoh_config;
mod zenoh_ee;

use std::sync::atomic::Ordering;
use std::time::Duration;

use state::AppState;
use tauri::Manager;

/// Time budget for the best-effort safe stop on window close. Long enough for a
/// clean confirmed detach on a healthy bus, short enough that a dead bus doesn't
/// make closing the GUI feel stuck.
const LIFT_CLOSE_STOP_BUDGET: Duration = Duration::from_millis(1_500);

/// A normal window close must *always* succeed. The firmware fails safe on its
/// own — the velocity RPDO watchdog coasts the bridge when the stream stops,
/// autonomous Position/Homing moves are soft-limit bounded and end in coast, and
/// IWDG + the LOCKUP hardware break cover a firmware crash — so closing the GUI
/// must never be held hostage by a CAN handshake. A pulled CAN cable would
/// otherwise trap the window open. We make a time-boxed best-effort safe detach
/// (which sends a directed NMT Stop first thing, dropping the node to coast) and
/// then exit unconditionally whether or not it was acknowledged.
fn request_safe_close(window: tauri::Window) {
    let handle = window.app_handle().clone();
    let state = handle.state::<AppState>();
    if state.lift_close_in_progress.swap(true, Ordering::SeqCst) {
        return;
    }

    tauri::async_runtime::spawn(async move {
        let state = handle.state::<AppState>();
        match tokio::time::timeout(LIFT_CLOSE_STOP_BUDGET, commands::stop_lift_session(&state)).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!("lift stop on close reported {error}; exiting anyway (firmware fails safe)")
            }
            Err(_) => log::warn!(
                "lift stop on close timed out after {} ms; exiting anyway (firmware fails safe)",
                LIFT_CLOSE_STOP_BUDGET.as_millis()
            ),
        }
        handle.exit(0);
    });
}

pub fn run() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,hex_motor=info,hex_motor_gui_lib=info"),
    )
    .try_init();

    tauri::Builder::default()
        .manage(AppState::default())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                request_safe_close(window.clone());
            }
        })
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
            commands::smartknob_start,
            commands::smartknob_stop,
            commands::smartknob_set_config,
            commands::smartknob_set_tuning,
            commands::smartknob_clear_error,
            commands::smartknob_get_state,
            commands::smartknob_set_custom_config,
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
