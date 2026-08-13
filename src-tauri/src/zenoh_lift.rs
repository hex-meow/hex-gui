//! Lift(Zenoh):连 hex-controller 的升降,做发现 / 取控 / homing / 高度控制。
//! 骨架镜像 [`crate::zenoh_ee`](发现 → 观察聚焦 → 取控 → 命令流 → 释放),
//! 设计对应 robot-overall-design/12-lift-api.md。
//!
//! 与 arm/ee 的三处结构性差异,都来自首款设备 `lift_a70` 的能力集(driver OD v0.4):
//!
//! 1. **命令不是 `JointTrajectory`**,而是 `LiftCommand{oneof position|velocity}` ——
//!    该设备只有 Position/Velocity/Homing,明确不支持 Torque/MIT,也不提供力矩反馈。
//!    客户端必须读 `LiftDescription.command_modes` 再决定发什么,本模块据此禁用不支持的控件。
//! 2. **position 是自主 goal,不需要命令流**:设备自己规划轨迹、到位停机(合约明写无需
//!    keepalive)。所以只有 velocity jog 才起 50Hz 流;发完位置目标就撒手,靠 `target_reached`
//!    判完成 —— 自主 goal 没有回执,那是唯一途径。
//! 3. **homing 是一等公民**:未 homing 时 `set_mode(ACTIVE)` 会被控制器拒绝,必须先
//!    `rpc/home`;它立即回 started,结果走 `LiftStatus.homed`(要几秒~几十秒)。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::anyhow;
use prost::Message;
use serde::Serialize;

use crate::diag;
use crate::zenoh_discovery::ROBOT_DESCRIPTION_SELECTOR;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/robot_api.rs"));
}

fn enc<M: Message>(m: &M) -> Vec<u8> {
    let mut b = Vec::new();
    m.encode(&mut b).unwrap();
    b
}

async fn query_one<Resp: Message + Default>(
    session: &zenoh::Session,
    key: &str,
    payload: Vec<u8>,
) -> Option<Resp> {
    let replies = session.get(key).payload(payload).await.ok()?;
    if let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Resp::decode(&*sample.payload().to_bytes()).ok();
        }
    }
    None
}

/// 汇聚一次 query 的**全部**回复。`.../log/recent` 每进程一个 queryable,只取第一条会漏掉
/// 同一控制器下其它进程(launcher / 其它 robot)的日志。
async fn query_all(session: &zenoh::Session, key: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    if let Ok(replies) = session.get(key).await {
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                out.push((
                    sample.key_expr().as_str().to_string(),
                    sample.payload().to_bytes().to_vec(),
                ));
            }
        }
    }
    out
}

/// proto `Event` → 诊断 DTO(seq 占位 0,由 `EventBuf` 分配;kv 排序稳定)。
fn to_diag_event(ev: pb::Event) -> diag::RobotEvent {
    let ts_ns = ev.header.as_ref().map(|h| h.stamp_ns).unwrap_or(0);
    let mut kv: Vec<(String, String)> = ev.kv.into_iter().collect();
    kv.sort();
    diag::RobotEvent { seq: 0, severity: ev.severity, code: ev.code, text: ev.text, kv, ts_ns }
}

fn op_mode_name(m: i32) -> &'static str {
    match m {
        1 => "DISABLED",
        2 => "ACTIVE",
        100 => "FAULT",
        101 => "CALIBRATING",
        _ => "UNSPECIFIED",
    }
}

/// `0x453F` detailed fault(OD v0.4 §9)。只用于显示,不参与控制判定。
fn fault_name(code: u32) -> &'static str {
    match code {
        0x0000 => "",
        0x2100 => "母线过流",
        0x3210 => "母线过压",
        0x3220 => "母线欠压",
        0x5000 => "功率监测(INA238)异常",
        0x7340 => "编码器错误",
        0x8130 => "速度命令看门狗超时",
        0x8500 => "位置命令/控制错误",
        0xFF01 => "homing 超时",
        0xFF03 => "铭牌/配置无效",
        _ => "未知故障码",
    }
}

/// 发现到的一台升降。
#[derive(Serialize, Clone, Default)]
pub struct LiftInfo {
    pub prefix: String,
    pub model: String,
    pub dof: u32,
    pub joint_names: Vec<String>,
    pub pos_min: Vec<f32>,
    pub pos_max: Vec<f32>,
    pub vel_max: Vec<f32>,
    pub vel_min: Vec<f32>,
    pub needs_homing: Vec<bool>,
    pub command_modes: Vec<i32>,
    pub payload_max_kg: Option<f32>,
}

/// 推给前端的状态快照(LiftPanel 轮询)。
#[derive(Serialize, Clone, Default)]
pub struct ZenohLiftState {
    pub connected: bool,
    pub controlling: bool,
    pub holder: u32,
    pub mode: String,       // 我方所设 OperatingMode(取控作用域)
    pub robot_mode: String, // STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR(只读观察)
    pub model: String,
    pub prefix: String,

    /// 当前高度(m)。`lift/joint_state.q[0]`;未 homing 时设备严格报 0。
    pub height: f32,
    pub pos_min: f32,
    pub pos_max: f32,
    pub vel_max: f32,
    /// 速度释放死区:|dq| 小于它设备就脱力滑行。jog 滑条的下限该取它,
    /// 否则用户会发出"永远不会动"的小速度还以为是坏了。
    pub vel_min: f32,
    pub payload_max_kg: Option<f32>,

    // ── LiftStatus 直译(徽标)──
    pub homed: bool,
    /// 铭牌/型号/layout/CRC 校验。false ⇒ 设备 fail-closed 拒绝一切运动。
    pub config_valid: bool,
    /// position goal 到位。自主 goal 无回执,这是判断"动作完成"的唯一途径。
    pub target_reached: bool,
    pub moving: bool,
    pub output_limited: bool,
    pub at_lower_limit: bool,
    pub at_upper_limit: bool,
    pub estop: bool,
    pub fault_code: u32,
    pub fault_text: String,

    // ── 能力声明(决定前端禁用哪些控件)──
    pub can_position: bool,
    pub can_velocity: bool,
    pub guarded_contact_supported: bool,

    /// homing 进行中(mode==CALIBRATING 或本地已发起但状态还没跟上)。
    pub homing: bool,
    pub fatal: bool,
    /// 最近一次 RPC 的错误文本,前端弹一次就清。
    pub last_error: Option<String>,
}

struct Ctrl {
    prefix: StdMutex<Option<String>>,
    view_prefix: StdMutex<Option<String>>, // 观察聚焦(读永远开放,与取控解耦)
    session_id: AtomicU32,
    /// 取控期间必须一直拿住的 liveliness 租约(见 zenoh_lease)。
    /// drop 即等于告诉控制器"客户端没了" —— 故只在 release 时清空。
    live_lease: StdMutex<Option<crate::zenoh_lease::SessionLease>>,
    /// Some = 正在 jog:50Hz 重发 velocity(控制器 demand TTL 250ms,设备 watchdog 200ms)。
    /// position goal 不进这里 —— 它是一次性自主目标。
    jog: StdMutex<Option<f32>>,
    homing_pending: AtomicBool,
    state: StdMutex<ZenohLiftState>,
    events: StdMutex<diag::EventBuf>,
    logs: StdMutex<VecDeque<diag::LogLine>>,
}

pub struct ZenohLiftConn {
    session: zenoh::Session,
    ctrl: Arc<Ctrl>,
}

impl ZenohLiftConn {
    pub async fn open(connect: &str) -> anyhow::Result<Self> {
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("mode", "\"peer\"").unwrap();
        if !connect.is_empty() {
            cfg.insert_json5("connect/endpoints", &format!("[\"{connect}\"]"))
                .unwrap();
        }
        let session = zenoh::open(cfg).await.map_err(|e| anyhow!("zenoh open: {e}"))?;
        tokio::time::sleep(Duration::from_millis(700)).await;

        let ctrl = Arc::new(Ctrl {
            prefix: StdMutex::new(None),
            view_prefix: StdMutex::new(None),
            session_id: AtomicU32::new(0),
            live_lease: StdMutex::new(None),
            jog: StdMutex::new(None),
            homing_pending: AtomicBool::new(false),
            state: StdMutex::new(ZenohLiftState {
                connected: true,
                ..Default::default()
            }),
            events: StdMutex::new(diag::EventBuf::default()),
            logs: StdMutex::new(VecDeque::new()),
        });

        // ── 50Hz jog 流 ──
        // 只有 velocity 需要重发:它是持续 demand,控制器 250ms 内收不到刷新就停,
        // 设备侧另有 200ms watchdog。position goal 一次性下发,不在这里。
        {
            let s = session.clone();
            let c = ctrl.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                loop {
                    tick.tick().await;
                    let sid = c.session_id.load(Ordering::Relaxed);
                    if sid == 0 {
                        continue;
                    }
                    let Some(prefix) = c.prefix.lock().unwrap().clone() else {
                        continue;
                    };
                    let Some(dq) = *c.jog.lock().unwrap() else {
                        continue;
                    };
                    let cmd = pb::LiftCommand {
                        header: None,
                        session_id: sid,
                        on_timeout: pb::TimeoutBehavior::Hold as i32,
                        cmd: Some(pb::lift_command::Cmd::Velocity(pb::LiftVelocity {
                            dq: vec![dq],
                        })),
                    };
                    let _ = s.put(format!("{prefix}/lift/command"), enc(&cmd)).await;
                }
            });
        }

        // ── lift/joint_state(100Hz):高度 ──
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/lift/joint_state").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else {
                        continue;
                    };
                    if sample.key_expr().as_str() != format!("{p}/lift/joint_state") {
                        continue;
                    }
                    if let Ok(js) = pb::JointState::decode(&*sample.payload().to_bytes()) {
                        if let Some(q) = js.q.first() {
                            c.state.lock().unwrap().height = *q;
                        }
                    }
                }
            });
        }

        // ── lift/status(~10Hz):statusword 徽标 + homing 完成判定 ──
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/lift/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else {
                        continue;
                    };
                    if sample.key_expr().as_str() != format!("{p}/lift/status") {
                        continue;
                    }
                    let Ok(s) = pb::LiftStatus::decode(&*sample.payload().to_bytes()) else {
                        continue;
                    };
                    let calibrating = s.mode == pb::OperatingMode::Calibrating as i32;
                    // homing 结束的判据是设备状态,不是"我方发过 rpc/home" ——
                    // 中途失败(急停/超时)也必须让按钮解锁,否则面板会永久卡在 homing 中。
                    if !calibrating && c.homing_pending.load(Ordering::Relaxed) {
                        c.homing_pending.store(false, Ordering::Relaxed);
                    }
                    let mut st = c.state.lock().unwrap();
                    st.homed = s.homed;
                    st.estop = s.estop;
                    st.fault_code = s.fault_code;
                    st.fault_text = fault_name(s.fault_code).into();
                    st.config_valid = s.config_valid.unwrap_or(false);
                    st.target_reached = s.target_reached.unwrap_or(false);
                    st.moving = s.moving.unwrap_or(false);
                    st.output_limited = s.output_limited.unwrap_or(false);
                    st.at_lower_limit = s.at_lower_limit.unwrap_or(false);
                    st.at_upper_limit = s.at_upper_limit.unwrap_or(false);
                    st.homing = calibrating || c.homing_pending.load(Ordering::Relaxed);
                }
            });
        }

        // ── robot 级 status:FATAL 灯 / holder / 失控判定(同 arm/ee)──
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Ok(s) = pb::RobotStatus::decode(&*sample.payload().to_bytes()) else {
                        continue;
                    };
                    let key = sample.key_expr().as_str();
                    if let Some(vp) = c.view_prefix.lock().unwrap().clone() {
                        if key == format!("{vp}/status") {
                            let mut st = c.state.lock().unwrap();
                            st.fatal = s.mode == pb::RobotMode::FatalError as i32;
                            st.holder = s.session_holder;
                            st.robot_mode = crate::diag::robot_mode_name(s.mode).into();
                        }
                    }
                    let Some(p) = c.prefix.lock().unwrap().clone() else {
                        continue;
                    };
                    if key != format!("{p}/status") {
                        continue;
                    }
                    // 被别人接管:立刻停 jog 并落回未取控,别让面板继续假装在控。
                    let our_sid = c.session_id.load(Ordering::Relaxed);
                    if our_sid != 0 && s.session_holder != our_sid {
                        c.session_id.store(0, Ordering::Relaxed);
                        *c.jog.lock().unwrap() = None;
                        let mut st = c.state.lock().unwrap();
                        st.controlling = false;
                        st.holder = s.session_holder;
                        st.mode = "DISABLED".into();
                        log::warn!("Lift: 失去控制权(当前 holder={})", s.session_holder);
                    }
                }
            });
        }

        // ── 事件流 <prefix>/events ──
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/events").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{p}/events") { continue; }
                    if let Ok(ev) = pb::Event::decode(&*sample.payload().to_bytes()) {
                        c.events.lock().unwrap().push_live(to_diag_event(ev));
                    }
                }
            });
        }
        // ── 日志流 hexmeow/<cid>/*/log ──
        // 按 **cid** 过滤而不是按 robot prefix:同一控制器下 launcher 与各 robot 各有自己的
        // log key,只收 lift 自己那条会看不到 launcher 的启动/失败信息 —— 而那恰恰是
        // "为什么没起来" 最有用的一段。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/log").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(dp) = c.view_prefix.lock().unwrap().clone() else { continue };
                    let Some(cid) = diag::cid_prefix(&dp) else { continue };
                    let key = sample.key_expr().as_str();
                    if !key.starts_with(&format!("{cid}/")) || !key.ends_with("/log") { continue; }
                    let proc = diag::proc_of_log_key(key);
                    let raw = String::from_utf8_lossy(&sample.payload().to_bytes()).into_owned();
                    let line = diag::parse_log_line(&proc, &raw);
                    diag::push_capped(&mut c.logs.lock().unwrap(), line, diag::LOG_RING_CAP);
                }
            });
        }

        Ok(Self { session, ctrl })
    }

    /// 发现所有 kind==LIFT 的 robot,并逐个补 `lift/description`。
    ///
    /// 分两阶段:先把 robot 级 query 的回复排空成 Vec,再逐个发 `lift/description` 的第二次
    /// query。与 `zenoh_ee::discover`(先 `discover_all().await` 收完再查)保持一致。
    ///
    /// 注:2026-08-08 排查"面板列不出设备"时一度怀疑嵌套 query 是元凶,**实测证伪** ——
    /// 嵌套写法在同样环境下也能正常发现。保留两阶段只是为了与 EE 同构、少一层嵌套借用,
    /// 不要把它当成那次故障的修复。
    pub async fn discover(&self) -> Vec<LiftInfo> {
        // 阶段一:排空 robot 级 description。
        let mut found: Vec<(String, String)> = Vec::new(); // (prefix, model)
        if let Ok(replies) = self.session.get(ROBOT_DESCRIPTION_SELECTOR).await {
            while let Ok(reply) = replies.recv_async().await {
                let Ok(sample) = reply.result() else { continue };
                let key = sample.key_expr().as_str().to_string();
                let Ok(d) = pb::RobotDescription::decode(&*sample.payload().to_bytes()) else {
                    continue;
                };
                if d.kind != pb::RobotKind::Lift as i32 {
                    continue;
                }
                let Some(prefix) = key.strip_suffix("/description") else {
                    continue;
                };
                found.push((prefix.to_string(), d.model));
            }
        }

        // 阶段二:外层回复已收完,可以安全地逐个查设备级 description。
        let mut out = Vec::new();
        for (prefix, model) in found {
            let mut info = LiftInfo {
                prefix: prefix.clone(),
                model,
                ..Default::default()
            };
            if let Some(ld) = query_one::<pb::LiftDescription>(
                &self.session,
                &format!("{prefix}/lift/description"),
                vec![],
            )
            .await
            {
                info.dof = ld.dof;
                info.joint_names = ld.joint_names;
                info.pos_min = ld.pos_min;
                info.pos_max = ld.pos_max;
                info.vel_max = ld.vel_max;
                info.vel_min = ld.vel_min;
                info.needs_homing = ld.needs_homing;
                info.command_modes = ld.command_modes;
                info.payload_max_kg = ld.payload_max_kg;
            }
            out.push(info);
        }
        log::info!("Lift 发现 {} 台", out.len());
        out
    }

    /// 观察聚焦(只读,与取控解耦):选中即观察,joint_state/lift_status/status 按此过滤。
    pub async fn set_focus(&self, prefix: &str) {
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
        {
            let mut st = self.ctrl.state.lock().unwrap();
            let connected = st.connected;
            let controlling = st.controlling;
            let held_prefix = st.prefix.clone();
            *st = ZenohLiftState {
                connected,
                controlling,
                prefix: held_prefix,
                ..Default::default()
            };
        }
        self.load_description(prefix).await;
    }

    /// 拉 `lift/description` 填限位与能力声明。限位来自设备派生的 98% 软限位,不是型号常量。
    async fn load_description(&self, prefix: &str) {
        let Some(d) = query_one::<pb::LiftDescription>(
            &self.session,
            &format!("{prefix}/lift/description"),
            vec![],
        )
        .await
        else {
            return;
        };
        let mut st = self.ctrl.state.lock().unwrap();
        st.pos_min = d.pos_min.first().copied().unwrap_or(0.0);
        st.pos_max = d.pos_max.first().copied().unwrap_or(0.0);
        st.vel_max = d.vel_max.first().copied().unwrap_or(0.0);
        st.vel_min = d.vel_min.first().copied().unwrap_or(0.0);
        st.payload_max_kg = d.payload_max_kg;
        st.guarded_contact_supported = d.guarded_contact_supported.unwrap_or(false);
        st.can_position = d
            .command_modes
            .contains(&(pb::LiftCommandMode::Position as i32));
        st.can_velocity = d
            .command_modes
            .contains(&(pb::LiftCommandMode::Velocity as i32));
    }

    pub async fn acquire(&self, prefix: &str, model: &str) -> anyhow::Result<()> {
        if self.ctrl.session_id.load(Ordering::Relaxed) != 0 {
            let cur = self.ctrl.prefix.lock().unwrap().clone();
            if cur.as_deref() != Some(prefix) {
                self.release().await;
            }
        }
        // 控制器强制要求 liveliness_key(见 zenoh_lease):token 必须活到 release,
        // 提前 drop 等于一取控就自动放手。
        let lease = crate::zenoh_lease::declare(&self.session, "lift").await?;
        let req = pb::AcquireSessionRequest {
            client_name: Some("hexmeow-gui".into()),
            liveliness_key: Some(lease.key.clone()),
        };
        let resp: pb::AcquireSessionResponse = query_one(
            &self.session,
            &format!("{prefix}/rpc/acquire_session"),
            enc(&req),
        )
        .await
        .ok_or_else(|| anyhow!("acquire 无回复"))?;
        if !resp.ok {
            return Err(anyhow!(
                "被占用:holder {} {:?}",
                resp.current_holder,
                resp.current_holder_name
            ));
        }
        self.ctrl.session_id.store(resp.session_id, Ordering::Relaxed);
        *self.ctrl.live_lease.lock().unwrap() = Some(lease);
        *self.ctrl.prefix.lock().unwrap() = Some(prefix.to_string());
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
        {
            let mut st = self.ctrl.state.lock().unwrap();
            st.controlling = true;
            st.prefix = prefix.into();
            st.model = model.into();
            st.mode = "DISABLED".into();
        }
        self.load_description(prefix).await;
        Ok(())
    }

    /// `rpc/home`:立即回 started,结果走 `LiftStatus.homed`。未 homing 时 ACTIVE 会被拒,
    /// 所以这是每次设备复位后的第一步。
    pub async fn home(&self) -> anyhow::Result<()> {
        let sid = self.require_session()?;
        *self.ctrl.jog.lock().unwrap() = None;
        let req = pb::HomeRequest { session_id: sid };
        let resp: pb::GenericResponse = query_one(
            &self.session,
            &format!("{}/rpc/home", self.prefix()),
            enc(&req),
        )
        .await
        .ok_or_else(|| anyhow!("home 无回复"))?;
        if !resp.ok {
            return Err(anyhow!(resp.error.unwrap_or_else(|| "home 被拒".into())));
        }
        self.ctrl.homing_pending.store(true, Ordering::Relaxed);
        self.ctrl.state.lock().unwrap().homing = true;
        Ok(())
    }

    /// 去某个高度(自主 goal)。发完即撒手 —— 设备自己规划并停机,靠 `target_reached` 判完成。
    pub async fn goto(&self, height_m: f32) -> anyhow::Result<()> {
        let sid = self.require_session()?;
        // 位置与速度互斥:先停 jog,免得 50Hz 流把刚下的位置目标顶掉。
        *self.ctrl.jog.lock().unwrap() = None;
        self.ensure_active().await?;
        let cmd = pb::LiftCommand {
            header: None,
            session_id: sid,
            on_timeout: pb::TimeoutBehavior::Hold as i32,
            cmd: Some(pb::lift_command::Cmd::Position(pb::LiftPositionGoal {
                q: vec![height_m],
            })),
        };
        self.session
            .put(format!("{}/lift/command", self.prefix()), enc(&cmd))
            .await
            .map_err(|e| anyhow!("发送位置目标: {e}"))?;
        Ok(())
    }

    /// 点动:`Some(dq)` 起 50Hz 流,`None` 停(显式发一帧 0 让设备回 Disabled 并自锁保持)。
    pub async fn jog(&self, dq: Option<f32>) -> anyhow::Result<()> {
        let sid = self.require_session()?;
        match dq {
            Some(v) => {
                self.ensure_active().await?;
                *self.ctrl.jog.lock().unwrap() = Some(v);
            }
            None => {
                *self.ctrl.jog.lock().unwrap() = None;
                let cmd = pb::LiftCommand {
                    header: None,
                    session_id: sid,
                    on_timeout: pb::TimeoutBehavior::Hold as i32,
                    cmd: Some(pb::lift_command::Cmd::Velocity(pb::LiftVelocity {
                        dq: vec![0.0],
                    })),
                };
                let _ = self
                    .session
                    .put(format!("{}/lift/command", self.prefix()), enc(&cmd))
                    .await;
            }
        }
        Ok(())
    }

    /// 幂等地进 ACTIVE。`set_mode` 自己会取会话号,所以这里不必传。
    async fn ensure_active(&self) -> anyhow::Result<()> {
        if self.ctrl.state.lock().unwrap().mode == "ACTIVE" {
            return Ok(());
        }
        self.set_mode(2).await
    }

    /// v1 只支持 DISABLED(1)/ACTIVE(2);自锁机构没有 PASSIVE/GRAVITY_COMP。
    /// 未 homing 时控制器会拒绝 ACTIVE —— 把错误如实带回前端,不要假装成功。
    pub async fn set_mode(&self, mode: i32) -> anyhow::Result<()> {
        let sid = self.require_session()?;
        if mode != 2 {
            *self.ctrl.jog.lock().unwrap() = None;
        }
        let req = pb::SetModeRequest {
            session_id: sid,
            mode,
        };
        let resp: pb::GenericResponse = query_one(
            &self.session,
            &format!("{}/rpc/set_mode", self.prefix()),
            enc(&req),
        )
        .await
        .ok_or_else(|| anyhow!("set_mode 无回复"))?;
        if !resp.ok {
            return Err(anyhow!(resp.error.unwrap_or_else(|| "set_mode 被拒".into())));
        }
        self.ctrl.state.lock().unwrap().mode = op_mode_name(mode).into();
        Ok(())
    }

    pub async fn clear_fault(&self) -> anyhow::Result<()> {
        let sid = self.require_session()?;
        let req = pb::ClearFaultRequest { session_id: sid };
        let resp: pb::GenericResponse = query_one(
            &self.session,
            &format!("{}/rpc/clear_fault", self.prefix()),
            enc(&req),
        )
        .await
        .ok_or_else(|| anyhow!("clear_fault 无回复"))?;
        if !resp.ok {
            return Err(anyhow!(resp
                .error
                .unwrap_or_else(|| "clear_fault 失败".into())));
        }
        self.ctrl.state.lock().unwrap().mode = "DISABLED".into();
        Ok(())
    }

    /// 收紧软限位/速度上限。只收紧不放宽 —— 控制器会把越界值夹回设备能力。
    pub async fn set_limits(
        &self,
        pos_min: Option<f32>,
        pos_max: Option<f32>,
        vel_max: Option<f32>,
    ) -> anyhow::Result<()> {
        let sid = self.require_session()?;
        let req = pb::LiftLimits {
            session_id: sid,
            pos_min: pos_min.map(|v| vec![v]).unwrap_or_default(),
            pos_max: pos_max.map(|v| vec![v]).unwrap_or_default(),
            vel_max: vel_max.map(|v| vec![v]).unwrap_or_default(),
            ..Default::default()
        };
        let resp: pb::GenericResponse = query_one(
            &self.session,
            &format!("{}/rpc/set_limits", self.prefix()),
            enc(&req),
        )
        .await
        .ok_or_else(|| anyhow!("set_limits 无回复"))?;
        if !resp.ok {
            return Err(anyhow!(resp
                .error
                .unwrap_or_else(|| "set_limits 失败".into())));
        }
        self.load_description(&self.prefix()).await;
        Ok(())
    }

    pub fn get_events(&self) -> diag::EventsSnapshot {
        self.ctrl.events.lock().unwrap().snapshot()
    }

    pub fn get_logs(&self) -> Vec<diag::LogLine> {
        self.ctrl.logs.lock().unwrap().iter().cloned().collect()
    }

    /// 从 `<prefix>/events/recent` 与 `<cid>/*/log/recent` 播种一次历史 ——
    /// 事后才连上 GUI 也能看到之前发生的事(如 homing 失败、0x8130)。
    pub async fn refresh_diag(&self) {
        let Some(prefix) = self.ctrl.view_prefix.lock().unwrap().clone() else { return };
        if let Some(log) =
            query_one::<pb::EventLog>(&self.session, &format!("{prefix}/events/recent"), vec![]).await
        {
            let history: Vec<diag::RobotEvent> = log.events.into_iter().map(to_diag_event).collect();
            self.ctrl.events.lock().unwrap().reseed(history);
        }
        if let Some(cid) = diag::cid_prefix(&prefix) {
            let blobs = query_all(&self.session, &format!("{cid}/*/log/recent")).await;
            let mut ring = VecDeque::new();
            for (key, payload) in blobs {
                let proc = diag::proc_of_log_key(&key);
                let text = String::from_utf8_lossy(&payload);
                for raw in text.lines().filter(|l| !l.is_empty()) {
                    diag::push_capped(&mut ring, diag::parse_log_line(&proc, raw), diag::LOG_RING_CAP);
                }
            }
            *self.ctrl.logs.lock().unwrap() = ring;
        }
    }

    pub fn state(&self) -> ZenohLiftState {
        self.ctrl.state.lock().unwrap().clone()
    }

    pub async fn release(&self) {
        let sid = self.ctrl.session_id.swap(0, Ordering::Relaxed);
        // 先撤 token 再发 release_session:两条路径都会让控制器释放会话,
        // 先撤的那条保证即使 release RPC 发不出去(网络已断)会话也不会留下。
        self.ctrl.live_lease.lock().unwrap().take();
        *self.ctrl.jog.lock().unwrap() = None;
        let prefix = self.ctrl.prefix.lock().unwrap().clone();
        if let (Some(prefix), true) = (prefix, sid != 0) {
            let req = pb::ReleaseSessionRequest { session_id: sid };
            let _: Option<pb::GenericResponse> = query_one(
                &self.session,
                &format!("{prefix}/rpc/release_session"),
                enc(&req),
            )
            .await;
        }
        *self.ctrl.prefix.lock().unwrap() = None;
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = false;
        st.holder = 0;
        st.mode = "DISABLED".into();
    }

    fn require_session(&self) -> anyhow::Result<u32> {
        match self.ctrl.session_id.load(Ordering::Relaxed) {
            0 => Err(anyhow!("未持有控制权(先取控)")),
            sid => Ok(sid),
        }
    }

    fn prefix(&self) -> String {
        self.ctrl.prefix.lock().unwrap().clone().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机/在线冒烟测试:跑 GUI **自己的**发现路径,而不是另写一个客户端 ——
    /// 2026-08-08 面板列不出设备的原因正是 `discover()` 在外层 query 的回复循环里嵌套了
    /// 第二次 query;那种 bug 只有走同一段代码才抓得到。
    ///
    /// 需要现网有一台 lift 控制器,故默认 ignore:
    /// `cargo test -p hex-motor-gui --lib -- --ignored discover_finds_a_live_lift --nocapture`
    // zenoh runtime 不支持 current-thread 调度器,必须多线程。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a running lift controller + zenoh router on tcp/127.0.0.1:7447"]
    async fn discover_finds_a_live_lift() {
        // 端点由环境变量给,方便对比"填了 tcp/…" 与 "留空(组播扫描)"两种用法。
        let ep = std::env::var("SMOKE_ENDPOINT").unwrap_or_else(|_| "tcp/127.0.0.1:7447".into());
        println!("connecting with endpoint = {ep:?}");
        let conn = ZenohLiftConn::open(&ep).await.expect("open zenoh");
        let lifts = conn.discover().await;
        println!("discovered {} lift(s)", lifts.len());
        for l in &lifts {
            println!(
                "  {} model={} pos=[{:?},{:?}] vel_max={:?} vel_min={:?} modes={:?} payload={:?}",
                l.prefix, l.model, l.pos_min, l.pos_max, l.vel_max, l.vel_min,
                l.command_modes, l.payload_max_kg
            );
        }
        assert!(!lifts.is_empty(), "没发现 lift —— 发现路径回归了");
        // description 必须被补齐:只有 robot 级回复而没有设备级细节 = 第二阶段 query 挂了。
        assert!(!lifts[0].pos_max.is_empty(), "lift/description 未补齐");
    }

    /// 在线冒烟:诊断历史播种。事件来自控制器的 `<prefix>/events/recent`,
    /// 日志来自同一 cid 下**每个进程**的 `.../log/recent`(含 launcher —— 排查"没起来"时
    /// 最有用的恰恰是它,只订 lift 自己那条会看不到)。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires a running lift controller + zenoh router on tcp/127.0.0.1:7447"]
    async fn refresh_diag_seeds_logs_and_events() {
        let conn = ZenohLiftConn::open("tcp/127.0.0.1:7447").await.expect("open zenoh");
        let lifts = conn.discover().await;
        assert!(!lifts.is_empty(), "先确保有 lift 在跑");
        conn.set_focus(&lifts[0].prefix).await;
        conn.refresh_diag().await;
        let logs = conn.get_logs();
        let events = conn.get_events();
        println!("logs={} events={}", logs.len(), events.events.len());
        for l in logs.iter().rev().take(3) {
            println!("  [{}] {} {}", l.proc, l.level, l.msg);
        }
        for e in events.events.iter().rev().take(5) {
            println!("  event {} {}", e.code, e.text);
        }
        assert!(!logs.is_empty(), "日志历史应非空");
    }

    #[test]
    fn fault_codes_match_od_v04_table() {
        assert_eq!(fault_name(0x0000), "");
        assert_eq!(fault_name(0x8130), "速度命令看门狗超时");
        assert_eq!(fault_name(0xFF03), "铭牌/配置无效");
        assert_eq!(fault_name(0x1234), "未知故障码");
    }

    #[test]
    fn operating_mode_names_cover_lift_states() {
        assert_eq!(op_mode_name(1), "DISABLED");
        assert_eq!(op_mode_name(2), "ACTIVE");
        assert_eq!(op_mode_name(100), "FAULT");
        // homing 期间控制器报 CALIBRATING,面板据此显示"回零中"。
        assert_eq!(op_mode_name(101), "CALIBRATING");
    }
}
