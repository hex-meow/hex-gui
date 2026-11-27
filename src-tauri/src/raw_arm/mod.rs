//! Hex-arm(Raw CAN) workspace. Planning never sends CAN frames. The session owns the
//! only drive command channel and lives independently of the WebView frame rate.
mod commands;
mod gripper;
mod model;
mod runtime;

use crate::{
    backend,
    can_lease::{CanOwner, CanTransportGate},
};
use commands::{Command, Sender};
use gripper::GripperMotion;
use hex_arm::{hardware::ArmBus, ArmModel, JointState};
use model::{Limits, ModelInfo, Preview, Target};
use runtime::{
    ArrivalTolerance, Control, FactoryReading, Gains, GravityScale, Installation, Status,
};
use serde::Serialize;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tauri::State;
use tokio::sync::{watch, Mutex};

struct Session {
    tx: Sender,
    status: watch::Receiver<Status>,
    task: tokio::task::JoinHandle<()>,
    connection: Connection,
}

#[derive(Clone, Serialize)]
pub(crate) struct Connection {
    iface: String,
    data_bitrate: u32,
    installation: Installation,
    goal_tolerance: ArrivalTolerance,
    gravity_scale: GravityScale,
    factory: Vec<FactoryReading>,
    gains: Option<Gains>,
    limits: Option<Limits>,
    arm_xml: String,
    tool_xml: Option<String>,
    model: ModelInfo,
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

    async fn change_motion(
        &self,
        make: impl FnOnce(commands::Reply) -> Command,
    ) -> Result<(), String> {
        let (tx, reply) = {
            let mut w = self.inner.lock().await;
            let tx = w.session()?.tx.clone();
            w.invalidate();
            let reply = tx.request(make);
            (tx, reply)
        };
        let result = reply.await;
        // A concurrent planner may have used the feedback from before this command.
        let mut w = self.inner.lock().await;
        if w.session.as_ref().is_some_and(|s| s.tx.same_session(&tx)) {
            w.invalidate();
        }
        result
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
    goal_tolerance: Option<ArrivalTolerance>,
    gains: Option<Gains>,
    limits: Option<Limits>,
    gravity_scale: Option<GravityScale>,
) -> Result<Vec<FactoryReading>, String> {
    let mut w = state.inner.lock().await;
    if w.session.is_some() {
        return Err("Arm session already open".into());
    }
    let (model, model_info) = w.model.clone().ok_or("Open the arm URDF first")?;
    let kind = model_info.tool.as_ref().map(|t| t.kind);
    let config = installation.bus_config(kind).map_err(|e| e.to_string())?;
    let torque_limits = installation
        .torque_limits_by_node()
        .map_err(|e| e.to_string())?;
    let tool = kind.zip(config.tool.and_then(|t| t.encoder));
    let lease = gate.try_acquire(CanOwner::RawArm)?;
    let (bus, _) = backend::open_bus(&iface, data_bitrate, false, lease)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let mut arm = ArmBus::new_with_arm_torque_limits(bus, config, &torque_limits)
        .map_err(|e| e.to_string())?;
    // The driver's batch initializer preflights every joint and the independent
    // tool before PDO setup and confirms Disabled feedback for the whole group.
    arm.initialize().await.map_err(|e| e.to_string())?;
    let (axes, factory) = installation.axes(&arm, &model).await;
    let (tx, rx) = commands::channel();
    let goal_tolerance = goal_tolerance.unwrap_or_default();
    let gravity_scale = gravity_scale.unwrap_or_default();
    let (publisher, status) = watch::channel(Status::new(
        goal_tolerance,
        gravity_scale,
        installation.offset,
        factory.clone(),
        gains,
        limits,
    ));
    let task = tokio::spawn(runtime::run(
        arm,
        model,
        axes,
        installation.factor,
        tool,
        rx,
        publisher,
        state.active.clone(),
    ));
    let connection = Connection {
        iface,
        data_bitrate,
        installation,
        goal_tolerance,
        gravity_scale,
        gains,
        limits,
        factory: factory.clone(),
        arm_xml: w.xml.0.clone(),
        tool_xml: w.xml.1.clone(),
        model: model_info,
    };
    w.session = Some(Session {
        tx,
        status,
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
    Ok(state.inner.lock().await.session.as_ref().map(|s| {
        let status = s.status.borrow();
        let mut conn = s.connection.clone();
        conn.installation.offset = status.offsets;
        conn.goal_tolerance = status.goal_tolerance;
        conn.gravity_scale = status.gravity_scale;
        conn.gains = status.gains;
        conn.limits = status.limits;
        conn.factory.clone_from(&status.factory);
        conn
    }))
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
        let (model, info) = w.model.as_ref().ok_or("Open the arm URDF first")?;
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
            JointState::at_rest(&info.home)
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
    state
        .change_motion(|r| Command::Activate(mode, gains, r))
        .await
}

#[tauri::command]
pub(crate) async fn raw_arm_set_goal_tolerance(
    state: State<'_, RawArm>,
    tolerance: ArrivalTolerance,
) -> Result<(), String> {
    let reply = state
        .inner
        .lock()
        .await
        .session()?
        .tx
        .request(move |r| Command::SetGoalTolerance(tolerance, r));
    reply.await
}

#[tauri::command]
pub(crate) async fn raw_arm_set_gravity_scale(
    state: State<'_, RawArm>,
    scale: GravityScale,
) -> Result<(), String> {
    let reply = state
        .inner
        .lock()
        .await
        .session()?
        .tx
        .request(move |r| Command::SetGravityScale(scale, r));
    reply.await
}

#[tauri::command]
pub(crate) async fn raw_arm_execute(
    state: State<'_, RawArm>,
    id: u64,
    gains: Gains,
) -> Result<(), String> {
    let (tx, epoch, goal, limits, activate) = {
        let w = state.inner.lock().await;
        let (goal, limits) = w
            .preview
            .as_ref()
            .filter(|_| w.revision == id)
            .ok_or("Preview the current target first")?;
        let session = w.session()?;
        let epoch = session.tx.epoch();
        let activate = (session.status.borrow().mode == runtime::Mode::Monitor).then(|| {
            session
                .tx
                .request_at(epoch, move |r| Command::Activate(Control::Hold, gains, r))
        });
        (session.tx.clone(), epoch, goal.clone(), *limits, activate)
    };
    if let Some(reply) = activate {
        reply.await?;
    }
    // Keep precisely the IK branch shown in the viewer. The drive replans to
    // that endpoint from the newest measured position/velocity at execution.
    tx.request_at(epoch, move |r| Command::Move(goal, limits, r))
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
    state.change_motion(|r| Command::Offsets(offsets, r)).await
}

#[tauri::command]
pub(crate) async fn raw_arm_reread_factory_calibration(
    state: State<'_, RawArm>,
    node_id: u8,
) -> Result<FactoryReading, String> {
    let reply = state
        .inner
        .lock()
        .await
        .session()?
        .tx
        .request(move |r| Command::ReadFactory(node_id, r));
    reply.await
}

#[tauri::command]
pub(crate) async fn raw_arm_stop(state: State<'_, RawArm>, limits: Limits) -> Result<(), String> {
    state.change_motion(|r| Command::Stop(limits, r)).await
}

#[tauri::command]
pub(crate) async fn raw_arm_move_gripper(
    state: State<'_, RawArm>,
    motion: GripperMotion,
) -> Result<(), String> {
    state
        .change_motion(|r| Command::MoveGripper(motion, r))
        .await
}

#[tauri::command]
pub(crate) async fn raw_arm_disable_gripper(state: State<'_, RawArm>) -> Result<(), String> {
    let reply = state
        .inner
        .lock()
        .await
        .session()?
        .tx
        .request(Command::DisableGripper);
    reply.await
}

#[tauri::command]
pub(crate) async fn raw_arm_release(state: State<'_, RawArm>) -> Result<(), String> {
    state.change_motion(Command::Release).await
}

#[tauri::command]
pub(crate) async fn raw_arm_disconnect(state: State<'_, RawArm>) -> Result<(), String> {
    let (tx, reply) = {
        let w = state.inner.lock().await;
        let Some(session) = w.session.as_ref() else {
            return Ok(());
        };
        if state.driving() {
            return Err("Support the arm and release the drives before disconnecting".into());
        }
        (session.tx.clone(), session.tx.request(Command::Close))
    };
    reply.await?;
    let mut w = state.inner.lock().await;
    if let Some(session) = w.session.take_if(|s| s.tx.same_session(&tx)) {
        let _ = session.task.await;
        w.invalidate();
    }
    Ok(())
}
