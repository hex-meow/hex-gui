//! Per-motor CSV recording at full TPDO rate.
//!
//! The frontend polls `get_status` at ~30 Hz for the live panel / charts, but
//! that decimates away most of the ~1 kHz TPDO1 stream. CSV logging instead
//! subscribes to [`Cia402Manager::subscribe_status`] directly, so it captures
//! **every** published `LiveState`. Each time the user toggles logging on for a
//! motor we open a fresh file (`logs/motor_0xNN_<localtime>.csv`); toggling off
//! flushes and closes it.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;

use hex_motor::cia402::{Cia402Manager, LiveState, Logic, StatusStreamItem, StreamOptions};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Flush the buffered writer every this many rows so a crash loses at most this
/// much data (and the file is reasonably fresh while the user watches it grow).
const FLUSH_EVERY_ROWS: u32 = 200;

/// Handle to a running CSV recorder. Stored in `AppState`; dropping the map
/// entry (via `stop`/disconnect) signals the task to flush and exit.
pub struct LogHandle {
    /// Absolute path of the CSV file (surfaced to the UI).
    pub path: String,
    /// Send `()` to ask the task to flush + close.
    pub stop: oneshot::Sender<()>,
    /// The recorder task; awaited on stop so the final flush completes.
    pub task: JoinHandle<()>,
}

/// Open a new CSV file and spawn the recorder task for `nid`.
pub async fn start(mgr: Arc<Cia402Manager>, nid: u8) -> anyhow::Result<LogHandle> {
    let dir = PathBuf::from("logs");
    fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f");
    let path = dir.join(format!("motor_0x{nid:02X}_{stamp}.csv"));

    let file = File::create(&path)?;
    let mut writer = BufWriter::new(file);
    writeln!(
        writer,
        "host_iso,motor_ts_us,position_rev,velocity_rev_per_s,torque_nm,\
         status_word,mode_display,driver_temp_c,motor_temp_c,error_register,logic"
    )?;
    writer.flush()?;

    let abs = fs::canonicalize(&path).unwrap_or(path);
    let path_str = abs.to_string_lossy().into_owned();

    // Full-rate subscription, independent of the chart polling path.
    let mut stream = mgr.subscribe_status(nid, StreamOptions::default())?;
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        let mut rows: u32 = 0;
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                item = stream.recv() => match item {
                    Some(StatusStreamItem::Sample(ls)) => {
                        if write_row(&mut writer, &ls).is_err() {
                            break;
                        }
                        rows += 1;
                        if rows >= FLUSH_EVERY_ROWS {
                            let _ = writer.flush();
                            rows = 0;
                        }
                    }
                    // Logged data is just a stream; a lag means we dropped some
                    // very-high-rate samples. Note it but keep going.
                    Some(StatusStreamItem::Lagged { dropped }) => {
                        log::warn!("csv log nid 0x{nid:02X}: lagged, dropped {dropped} samples");
                    }
                    None => break, // manager/motor gone
                },
            }
        }
        let _ = writer.flush();
        log::info!("csv log nid 0x{nid:02X}: stopped");
    });

    Ok(LogHandle {
        path: path_str,
        stop: stop_tx,
        task,
    })
}

/// Signal the task to stop and await its final flush.
pub async fn stop(handle: LogHandle) {
    let _ = handle.stop.send(());
    let _ = handle.task.await;
}

fn write_row(w: &mut impl Write, ls: &LiveState) -> std::io::Result<()> {
    let m = &ls.measurements;
    let host = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let f32opt = |o: Option<f32>| o.map(|v| v.to_string()).unwrap_or_default();
    let u32opt = |o: Option<u32>| o.map(|v| v.to_string()).unwrap_or_default();
    let sw = m
        .status_word
        .map(|w| format!("0x{w:04X}"))
        .unwrap_or_default();
    let mode = m.mode_display.map(|v| v.to_string()).unwrap_or_default();
    let err = m.error_register.map(|v| v.to_string()).unwrap_or_default();

    writeln!(
        w,
        "{host},{ts},{pos},{vel},{tau},{sw},{mode},{drv},{mtr},{err},{logic}",
        ts = u32opt(m.timestamp_us),
        pos = f32opt(m.position_rev),
        vel = f32opt(m.velocity_rev_per_s),
        tau = f32opt(m.torque_nm),
        drv = f32opt(m.driver_temp_c),
        mtr = f32opt(m.motor_temp_c),
        logic = logic_str(ls.logic.as_ref()),
    )
}

fn logic_str(l: Option<&Logic>) -> String {
    match l {
        None => String::new(),
        Some(Logic::Disabled) => "Disabled".into(),
        Some(Logic::Enabled(m)) => format!("Enabled({})", m.name()),
        Some(Logic::Error { kind, raw_code }) => format!("Error({kind:?}:0x{raw_code:04X})"),
    }
}
