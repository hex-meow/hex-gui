//! Hex-arm(Raw CAN) workspace. Planning never sends CAN frames. The session owns the
//! only drive command channel and lives independently of the WebView frame rate.
mod gripper;
mod model;
mod runtime;

use crate::{
    backend,
    can_lease::{CanOwner, CanTransportGate},
};
use gripper::GripperMotion;
use hex_arm::{
    hardware::{ArmBus, AxisControl},
    ArmModel, JointState,
};
use model::{Limits, ModelInfo, Preview, Target};
use runtime::{Command, Control, Gains, GravityScale, Installation, Reply, Status};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::State;
use tokio::sync::{mpsc, oneshot, watch, Mutex};

struct Session {
    tx: mpsc::Sender<Command>,
    status: watch::Receiver<Status>,
    axes: Vec<AxisControl>,
    task: tokio::task::JoinHandle<()>,
    connection: Connection,
}

#[derive(Clone, Serialize)]
pub(crate) struct Connection {
    iface: String,
    data_bitrate: u32,
    installation: Installation,
    factory: Vec<Option<f64>>,
    arm_xml: String,
    tool_xml: Option<String>,
    model: ModelInfo,
}

impl Session {
    async fn activate(&self, control: Control, gains: Gains) -> Result<(), String> {
        let axes = gains
            .apply(&self.axes, control)
            .map_err(|e| e.to_string())?;
        self.request(|reply| Command::Activate {
            control,
            axes,
            reply,
        })
        .await
    }

    async fn request(&self, make: impl FnOnce(Reply) -> Command) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(make(tx))
            .await
            .map_err(|_| "Arm session ended")?;
        rx.await.map_err(|_| "Arm session ended")?
    }
}

#[derive(Default)]
struct Workspace {
    model: Option<(ArmModel, ModelInfo)>,
    xml: (String, Option<String>),
    session: Option<Session>,
    revision: u64,
    preview: Option<(Vec<f64>, Limits)>,
}

impl Workspace {
    fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.preview = None;
    }
    fn session(&self) -> Result<&Session, String> {
        self.session
            .as_ref()
            .ok_or_else(|| "Initialize the CAN interface first".into())
    }
}

#[derive(Default)]
pub(crate) struct RawArm {
    inner: Mutex<Workspace>,
    active: Arc<AtomicBool>,
}

impl RawArm {
    pub(crate) fn driving(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[tauri::command]
pub(crate) async fn raw_arm_model(
    state: State<'_, RawArm>,
    arm_xml: String,
    tool_xml: Option<String>,
) -> Result<ModelInfo, String> {
    let mut w = state.inner.lock().await;
    if w.session.is_some() {
        return Err("Disconnect before changing the mounted model".into());
    }
    let (model, info) = model::load(&arm_xml, tool_xml.as_deref()).map_err(|e| e.to_string())?;
    w.model = Some((model, info.clone()));
    w.xml = (arm_xml, tool_xml);
    w.invalidate();
    Ok(info)
}

#[tauri::command]
pub(crate) async fn raw_arm_connect(
    state: State<'_, RawArm>,
    gate: State<'_, CanTransportGate>,
    iface: String,
    data_bitrate: u32,
    installation: Installation,
) -> Result<Vec<Option<f64>>, String> {
    let mut w = state.inner.lock().await;
    if w.session.is_some() {
        return Err("Arm session already open".into());
    }
    let (model, model_info) = w.model.clone().ok_or("Open the arm URDF first")?;
    let kind = model_info.tool.as_ref().map(|t| t.kind);
    let config = installation.bus_config(kind).map_err(|e| e.to_string())?;
    let tool = kind.zip(config.tool.and_then(|t| t.encoder));
    let lease = gate.try_acquire(CanOwner::RawArm)?;
    let (bus, _) = backend::open_bus(&iface, data_bitrate, false, lease)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let mut arm = ArmBus::new(bus, config).map_err(|e| e.to_string())?;
    // The driver's batch initializer preflights every joint and the independent
    // tool before PDO setup and confirms Disabled feedback for the whole group.
    arm.initialize().await.map_err(|e| e.to_string())?;
    let (axes, factory) = installation.axes(&arm, &model).await;
    let (tx, rx) = mpsc::channel(8);
    let (publisher, status) = watch::channel(Status::default());
    let task = tokio::spawn(runtime::run(
        arm,
        model,
        axes.clone(),
        tool,
        rx,
        publisher,
        state.active.clone(),
    ));
    let connection = Connection {
        iface,
        data_bitrate,
        installation,
        factory: factory.clone(),
        arm_xml: w.xml.0.clone(),
        tool_xml: w.xml.1.clone(),
        model: model_info,
    };
    w.session = Some(Session {
        tx,
        status,
        axes,
        task,
        connection,
    });
    w.invalidate();
    Ok(factory)
}

/// Recover the existing control panel after a WebView reload without touching the bus.
#[tauri::command]
pub(crate) async fn raw_arm_session(
    state: State<'_, RawArm>,
) -> Result<Option<Connection>, String> {
    Ok(state
        .inner
        .lock()
        .await
        .session
        .as_ref()
        .map(|s| s.connection.clone()))
}

#[tauri::command]
pub(crate) async fn raw_arm_status(state: State<'_, RawArm>) -> Result<Status, String> {
    let w = state.inner.lock().await;
    let session = w.session()?;
    let status = session.status.borrow().clone();
    Ok(status)
}

#[tauri::command]
pub(crate) async fn raw_arm_preview(
    state: State<'_, RawArm>,
    target: Target,
    limits: Limits,
) -> Result<Preview, String> {
    let (model, start, id) = {
        let mut w = state.inner.lock().await;
        w.invalidate();
        let (model, _) = w.model.as_ref().ok_or("Open the arm URDF first")?;
        let mut model = model.clone();
        let start = if let Some(session) = &w.session {
            let status = session.status.borrow();
            let feedback = status
                .feedback
                .as_ref()
                .ok_or("Waiting for live joint feedback")?;
            if let (Some(measured), Some(tool)) = (&feedback.tool, model.tool_mut()) {
                tool.set_positions(&[measured.position])
                    .map_err(|e| e.to_string())?;
            }
            feedback.start()?
        } else {
            JointState::at_rest(&model::home(&model))
        };
        (model, start, w.revision)
    };
    // IK can take several hundred iterations for an unreachable input. It must
    // never occupy the servo task or hold the session lock while it runs.
    let preview =
        tokio::task::spawn_blocking(move || model::plan(&model, start, target, limits, id))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
    let mut w = state.inner.lock().await;
    if w.revision != id {
        return Err("Preview superseded".into());
    }
    w.preview = Some((preview.goal.clone(), limits));
    Ok(preview)
}

#[tauri::command]
pub(crate) async fn raw_arm_control(
    state: State<'_, RawArm>,
    mode: Control,
    gains: Gains,
) -> Result<(), String> {
    let mut w = state.inner.lock().await;
    w.invalidate();
    w.session()?.activate(mode, gains).await
}

#[tauri::command]
pub(crate) async fn raw_arm_set_gravity_scale(
    state: State<'_, RawArm>,
    scale: GravityScale,
) -> Result<(), String> {
    let w = state.inner.lock().await;
    w.session()?
        .request(|r| Command::SetGravityScale(scale, r))
        .await
}

#[tauri::command]
pub(crate) async fn raw_arm_execute(
    state: State<'_, RawArm>,
    id: u64,
    gains: Gains,
) -> Result<(), String> {
    let w = state.inner.lock().await;
    let (goal, limits) = w
        .preview
        .as_ref()
        .filter(|_| w.revision == id)
        .ok_or("Preview the current target first")?;
    let session = w.session()?;
    let mode = session.status.borrow().mode;
    if mode == runtime::Mode::Monitor {
        session.activate(Control::Hold, gains).await?;
    }
    // Keep precisely the IK branch shown in the viewer. The drive replans to
    // that endpoint from the newest measured position/velocity at execution.
    session
        .request(|r| Command::Move(goal.clone(), *limits, r))
        .await
}

#[tauri::command]
pub(crate) async fn raw_arm_offsets(
    state: State<'_, RawArm>,
    offsets: model::Q,
) -> Result<(), String> {
    if !offsets.iter().all(|v| v.is_finite()) {
        return Err("Invalid joint offsets".into());
    }
    let mut w = state.inner.lock().await;
    let session = w
        .session
        .as_mut()
        .ok_or("Initialize the CAN interface first")?;
    session.request(|r| Command::Offsets(offsets, r)).await?;
    for (axis, value) in session.axes.iter_mut().zip(offsets) {
        axis.zero_offset_rad = value;
    }
    session.connection.installation.offset = offsets;
    w.invalidate();
    Ok(())
}

#[tauri::command]
pub(crate) async fn raw_arm_stop(state: State<'_, RawArm>, limits: Limits) -> Result<(), String> {
    let w = state.inner.lock().await;
    w.session()?.request(|r| Command::Stop(limits, r)).await
}

#[tauri::command]
pub(crate) async fn raw_arm_move_gripper(
    state: State<'_, RawArm>,
    motion: GripperMotion,
) -> Result<(), String> {
    let mut w = state.inner.lock().await;
    w.invalidate();
    w.session()?
        .request(|r| Command::MoveGripper(motion, r))
        .await
}

#[tauri::command]
pub(crate) async fn raw_arm_disable_gripper(state: State<'_, RawArm>) -> Result<(), String> {
    let w = state.inner.lock().await;
    w.session()?.request(Command::DisableGripper).await
}

#[tauri::command]
pub(crate) async fn raw_arm_release(state: State<'_, RawArm>) -> Result<(), String> {
    let mut w = state.inner.lock().await;
    w.invalidate();
    w.session()?.request(Command::Release).await
}

#[tauri::command]
pub(crate) async fn raw_arm_disconnect(state: State<'_, RawArm>) -> Result<(), String> {
    let mut w = state.inner.lock().await;
    if let Some(session) = &w.session {
        session.request(Command::Close).await?;
        if let Some(session) = w.session.take() {
            let _ = session.task.await;
        }
    }
    w.invalidate();
    Ok(())
}
