//! EE(Zenoh):连 hex-controller 的末端执行器(夹爪),做发现 / 取控 / 开合流 / grasp_state。
//! 镜像 [`crate::zenoh_arm`] 的骨架,但更简:1-DOF driver 关节、无重力前馈、
//! 额外订阅 `ee/status`(EeStatus:grasp_state 四态 + estop_behavior)。
//! 兼任机器人控制台的**全量发现**(discover_all:一次 query 拿所有 kind 的 robot,供设备树)。
//! 设计对应 robot-overall-design/11-ee-api.md;EE 复用 arm 的 Joint*(空 kp/kd = 控制器默认增益)。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::anyhow;
use prost::Message;
use serde::Serialize;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/robot_api.rs"));
}

fn enc<M: Message>(m: &M) -> Vec<u8> {
    let mut b = Vec::new();
    m.encode(&mut b).unwrap();
    b
}

async fn query_one<Resp: Message + Default>(session: &zenoh::Session, key: &str, payload: Vec<u8>) -> Option<Resp> {
    let replies = session.get(key).payload(payload).await.ok()?;
    if let Ok(reply) = replies.recv_async().await {
        if let Ok(sample) = reply.result() {
            return Resp::decode(&*sample.payload().to_bytes()).ok();
        }
    }
    None
}

fn op_mode_name(m: i32) -> &'static str {
    match m {
        1 => "DISABLED", 2 => "ACTIVE", 3 => "PASSIVE", 4 => "GRAVITY_COMP",
        100 => "FAULT", 101 => "CALIBRATING", _ => "UNSPECIFIED",
    }
}

fn grasp_name(v: i32) -> &'static str {
    match v { 1 => "MOVING", 2 => "AT_POSITION", 3 => "HOLDING", 4 => "LOST", _ => "" }
}

pub fn kind_name(k: i32) -> &'static str {
    match k { 1 => "arm", 2 => "base", 3 => "lift", 4 => "ee", _ => "?" } // HAND→EE 改名(11 §0.3)
}

/// 发现到的一个 EE。
#[derive(Serialize, Clone, Default)]
pub struct EeInfo {
    pub prefix: String,
    pub model: String,
    pub dof: u32,
    pub joint_names: Vec<String>,
    pub pos_min: Vec<f32>,
    pub pos_max: Vec<f32>,
    pub tau_max: Vec<f32>,
    pub opening_poly: Vec<f32>, // width(q)=Σ poly[i]·q^i;空 = 无宽度映射
    pub width_max: f32,
}

/// 设备树节点(机器人控制台的全量发现;所有 kind)。
#[derive(Serialize, Clone)]
pub struct RobotNode {
    pub prefix: String,      // hexmeow/<cid>/<idx>
    pub cid: String,
    pub robot_index: String,
    pub kind: i32,
    pub kind_name: String,
    pub model: String,
}

/// 推给前端的状态快照(EePanel 33ms 轮询)。
#[derive(Serialize, Clone, Default)]
pub struct ZenohEeState {
    pub controlling: bool,
    pub holder: u32,
    pub mode: String,            // 我方所设 OperatingMode(取控作用域)
    pub robot_mode: String,      // STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR(只读观察)
    pub model: String,
    pub prefix: String,
    pub q: Vec<f32>,
    pub dq: Vec<f32>,
    pub tau: Vec<f32>,
    pub grasp_state: String,     // MOVING/AT_POSITION/HOLDING/LOST(EeStatus,设备侧 1kHz 判定)
    pub estop_behavior: i32,     // 1=保位 2=松开 3=抗拒张开(EeStatus 回传当前生效值)
    pub pos_min: Vec<f32>,
    pub pos_max: Vec<f32>,
    pub opening_poly: Vec<f32>,
    pub width_max: f32,
    pub fatal: bool,
}

struct Ctrl {
    prefix: StdMutex<Option<String>>,
    session_id: AtomicU32,
    target: StdMutex<Option<f32>>, // driver 关节目标 q;Some=50Hz 流(喂看门狗)
    kp: StdMutex<Option<f32>>,     // None = 发空 kp → 控制器填型号默认增益
    state: StdMutex<ZenohEeState>,
    view_prefix: StdMutex<Option<String>>, // 观察聚焦(读永远开放,与取控解耦,同 arm)
}

pub struct ZenohEeConn {
    session: zenoh::Session,
    ctrl: Arc<Ctrl>,
}

impl ZenohEeConn {
    pub async fn open(connect: &str) -> anyhow::Result<Self> {
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("mode", "\"peer\"").unwrap();
        if !connect.is_empty() {
            cfg.insert_json5("connect/endpoints", &format!("[\"{connect}\"]")).unwrap();
        }
        let session = zenoh::open(cfg).await.map_err(|e| anyhow!("zenoh open: {e}"))?;
        tokio::time::sleep(Duration::from_millis(700)).await;
        let ctrl = Arc::new(Ctrl {
            prefix: StdMutex::new(None),
            session_id: AtomicU32::new(0),
            target: StdMutex::new(None),
            kp: StdMutex::new(None),
            state: StdMutex::new(ZenohEeState::default()),
            view_prefix: StdMutex::new(None),
        });

        // 50Hz 命令流:持有会话且有目标时发单点 JointTrajectory(断流 → 控制器 HOLD 保持抓握,11 §2)。
        {
            let s = session.clone();
            let c = ctrl.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                loop {
                    tick.tick().await;
                    let sid = c.session_id.load(Ordering::Relaxed);
                    if sid == 0 { continue; }
                    let Some(prefix) = c.prefix.lock().unwrap().clone() else { continue };
                    let Some(q) = *c.target.lock().unwrap() else { continue };
                    let kp = *c.kp.lock().unwrap();
                    let jt = pb::JointTrajectory {
                        header: None,
                        session_id: sid,
                        points: vec![pb::JointSetpoint {
                            q: vec![q], dq: vec![],
                            kp: kp.map(|k| vec![k]).unwrap_or_default(), // 空 = 控制器默认增益
                            kd: vec![], tau_ff: vec![],
                        }],
                        t_from_start_ns: vec![20_000_000], // 一阶保持(发送周期),滑条拖动更平滑
                        on_timeout: pb::TimeoutBehavior::Hold as i32,
                    };
                    let _ = s.put(format!("{prefix}/ee/command"), enc(&jt)).await;
                }
            });
        }
        // ee/joint_state 订阅(按观察 prefix 精确匹配;读永远开放)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/ee/joint_state").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{p}/ee/joint_state") { continue; }
                    if let Ok(js) = pb::JointState::decode(&*sample.payload().to_bytes()) {
                        let mut st = c.state.lock().unwrap();
                        st.q = js.q; st.dq = js.dq; st.tau = js.tau_est;
                    }
                }
            });
        }
        // ee/status 订阅:grasp_state 四态 + estop_behavior(EeStatus,11 §4)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/ee/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{p}/ee/status") { continue; }
                    if let Ok(es) = pb::EeStatus::decode(&*sample.payload().to_bytes()) {
                        let mut st = c.state.lock().unwrap();
                        st.grasp_state = grasp_name(es.grasp_state).into();
                        st.estop_behavior = es.estop_behavior;
                    }
                }
            });
        }
        // robot 级 status:FATAL 灯 / holder / 失控判定(同 arm 的双重语义)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Ok(s) = pb::RobotStatus::decode(&*sample.payload().to_bytes()) else { continue };
                    let key = sample.key_expr().as_str();
                    if let Some(vp) = c.view_prefix.lock().unwrap().clone() {
                        if key == format!("{vp}/status") {
                            let mut st = c.state.lock().unwrap();
                            st.fatal = s.mode == pb::RobotMode::FatalError as i32;
                            st.holder = s.session_holder;
                            st.robot_mode = crate::diag::robot_mode_name(s.mode).into();
                        }
                    }
                    let Some(p) = c.prefix.lock().unwrap().clone() else { continue };
                    if key != format!("{p}/status") { continue; }
                    let our_sid = c.session_id.load(Ordering::Relaxed);
                    if our_sid != 0 && s.session_holder != our_sid {
                        c.session_id.store(0, Ordering::Relaxed);
                        *c.target.lock().unwrap() = None;
                        let mut st = c.state.lock().unwrap();
                        st.controlling = false; st.holder = s.session_holder; st.mode = "DISABLED".into();
                        log::warn!("EE: 失去控制权(当前 holder={})", s.session_holder);
                    }
                }
            });
        }

        Ok(Self { session, ctrl })
    }

    /// 发现 EE(kind==EE),并逐个拉 ee/description 补细节。
    pub async fn discover(&self) -> Vec<EeInfo> {
        let mut out = Vec::new();
        for n in self.discover_all().await {
            if n.kind != pb::RobotKind::Ee as i32 { continue; }
            let mut info = EeInfo { prefix: n.prefix.clone(), model: n.model, ..Default::default() };
            if let Some(d) = query_one::<pb::EeDescription>(&self.session, &format!("{}/ee/description", n.prefix), vec![]).await {
                info.dof = d.dof; info.joint_names = d.joint_names;
                info.pos_min = d.pos_min; info.pos_max = d.pos_max; info.tau_max = d.tau_max;
                if let Some(m) = d.opening_map { info.opening_poly = m.poly; info.width_max = m.width_max; }
            }
            out.push(info);
        }
        out
    }

    /// 全量发现(机器人控制台设备树):一次 query 拿所有 kind 的 robot。
    pub async fn discover_all(&self) -> Vec<RobotNode> {
        let mut out = Vec::new();
        if let Ok(replies) = self.session.get("hexmeow/**/description").await {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    if let Ok(d) = pb::RobotDescription::decode(&*sample.payload().to_bytes()) {
                        let key = sample.key_expr().as_str();
                        let prefix = key.strip_suffix("/description").unwrap_or(key).to_string();
                        let parts: Vec<&str> = prefix.split('/').collect(); // hexmeow/<cid>/<idx>
                        let cid = parts.get(1).unwrap_or(&"").to_string();
                        out.push(RobotNode {
                            prefix, cid,
                            robot_index: d.robot_index.clone(),
                            kind: d.kind,
                            kind_name: kind_name(d.kind).into(),
                            model: d.model,
                        });
                    }
                }
            }
        }
        out.sort_by(|a, b| (&a.cid, &a.robot_index).cmp(&(&b.cid, &b.robot_index)));
        out
    }

    pub async fn acquire(&self, prefix: &str, model: &str) -> anyhow::Result<()> {
        let req = pb::AcquireSessionRequest { client_name: Some("hex-motor-gui".into()), liveliness_key: None };
        let resp: pb::AcquireSessionResponse = query_one(&self.session, &format!("{prefix}/rpc/acquire_session"), enc(&req))
            .await.ok_or_else(|| anyhow!("acquire 无回复"))?;
        if !resp.ok {
            return Err(anyhow!("被占用:holder {} {:?}", resp.current_holder, resp.current_holder_name));
        }
        self.ctrl.session_id.store(resp.session_id, Ordering::Relaxed);
        *self.ctrl.prefix.lock().unwrap() = Some(prefix.to_string());
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string()); // 取控隐含观察
        let desc = query_one::<pb::EeDescription>(&self.session, &format!("{prefix}/ee/description"), vec![]).await;
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = true; st.prefix = prefix.into(); st.model = model.into(); st.mode = "DISABLED".into();
        if let Some(d) = desc {
            st.pos_min = d.pos_min; st.pos_max = d.pos_max;
            if let Some(m) = d.opening_map { st.opening_poly = m.poly; st.width_max = m.width_max; }
        }
        Ok(())
    }

    /// 观察聚焦(只读,与取控解耦):选中即观察,joint_state/ee_status/status 按此过滤。
    pub async fn set_focus(&self, prefix: &str) {
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
        {
            let mut st = self.ctrl.state.lock().unwrap();
            st.fatal = false; st.holder = 0; st.robot_mode.clear(); st.grasp_state.clear();
            st.q.clear(); st.dq.clear(); st.tau.clear();
            st.pos_min.clear(); st.pos_max.clear(); st.opening_poly.clear(); st.width_max = 0.0;
            st.mode.clear();
        }
        if let Some(d) = query_one::<pb::EeDescription>(&self.session, &format!("{prefix}/ee/description"), vec![]).await {
            let mut st = self.ctrl.state.lock().unwrap();
            st.pos_min = d.pos_min; st.pos_max = d.pos_max;
            if let Some(m) = d.opening_map { st.opening_poly = m.poly; st.width_max = m.width_max; }
        }
    }

    /// 开合到 q(进 ACTIVE + 50Hz 流)。kp=None → 控制器默认增益;Some(k) → 限力/柔顺抓取用小 kp。
    pub async fn goto(&self, q: f32, kp: Option<f32>) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        *self.ctrl.kp.lock().unwrap() = kp;
        *self.ctrl.target.lock().unwrap() = Some(q);
        let req = pb::SetModeRequest { session_id: sid, mode: 2 };
        let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{}/rpc/set_mode", self.prefix()), enc(&req)).await;
        self.ctrl.state.lock().unwrap().mode = "ACTIVE".into();
        Ok(())
    }

    pub async fn set_mode(&self, mode: i32) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        if mode != 2 { *self.ctrl.target.lock().unwrap() = None; }
        let req = pb::SetModeRequest { session_id: sid, mode };
        let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{}/rpc/set_mode", self.prefix()), enc(&req)).await;
        self.ctrl.state.lock().unwrap().mode = op_mode_name(mode).into();
        Ok(())
    }

    /// estop 期间姿态(11 §10):1=HOLD_POSITION 2=RELEASE 3=KEEP_GRIP。
    pub async fn set_estop_behavior(&self, behavior: i32) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        let req = pb::SetEstopBehaviorRequest { session_id: sid, behavior };
        let resp: pb::GenericResponse = query_one(&self.session, &format!("{}/ee/rpc/set_estop_behavior", self.prefix()), enc(&req))
            .await.ok_or_else(|| anyhow!("set_estop_behavior 无回复"))?;
        if resp.ok { Ok(()) } else { Err(anyhow!(resp.error.unwrap_or_else(|| "失败".into()))) }
    }

    pub async fn clear_fault(&self) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权(clear_fault 需先取控)")); }
        // EE 托管侧 clear_fault 复用 EnableRequest 形状(只看 session_id)。
        let req = pb::EnableRequest { session_id: sid, on: false };
        let resp: pb::GenericResponse = query_one(&self.session, &format!("{}/rpc/clear_fault", self.prefix()), enc(&req))
            .await.ok_or_else(|| anyhow!("clear_fault 无回复"))?;
        if resp.ok { Ok(()) } else { Err(anyhow!(resp.error.unwrap_or_else(|| "clear_fault 失败".into()))) }
    }

    pub fn state(&self) -> ZenohEeState {
        self.ctrl.state.lock().unwrap().clone()
    }

    pub async fn release(&self) {
        let sid = self.ctrl.session_id.swap(0, Ordering::Relaxed);
        *self.ctrl.target.lock().unwrap() = None;
        let prefix = self.ctrl.prefix.lock().unwrap().clone();
        if let (Some(prefix), true) = (prefix, sid != 0) {
            let req = pb::ReleaseSessionRequest { session_id: sid };
            let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{prefix}/rpc/release_session"), enc(&req)).await;
        }
        *self.ctrl.prefix.lock().unwrap() = None;
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = false; st.holder = 0; st.mode = "DISABLED".into();
    }

    fn prefix(&self) -> String {
        self.ctrl.prefix.lock().unwrap().clone().unwrap_or_default()
    }
}
