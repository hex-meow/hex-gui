//! Arm(Zenoh):连 hex-controller 暴露的机械臂,做发现 / 取控 / 状态 / GRAVITY_COMP /
//! 设重力向量 / 移动到预设位姿。镜像 [`crate::zenoh_base`],但承载关节状态与臂特有 RPC。
//! 持久:一个 Session + 常驻 50Hz 命令流(仅 Active+有目标时发,喂看门狗)+ joint_state/status 订阅。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::anyhow;
use hex_arm_dynamics::ArmDynamics;
use prost::Message;
use serde::Serialize;

use crate::diag;

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

/// 汇聚一次 query 的**全部**回复(key, payload)。用于 `.../log/recent`(每进程一个 queryable → 多回复)。
async fn query_all(session: &zenoh::Session, key: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    if let Ok(replies) = session.get(key).await {
        while let Ok(reply) = replies.recv_async().await {
            if let Ok(sample) = reply.result() {
                out.push((sample.key_expr().as_str().to_string(), sample.payload().to_bytes().to_vec()));
            }
        }
    }
    out
}

/// proto `Event` → 诊断 DTO(seq 占位 0,由 [`diag::EventBuf`] 分配;kv 排序稳定;ts 取 Header.stamp_ns)。
fn to_event(ev: pb::Event) -> diag::RobotEvent {
    let ts_ns = ev.header.as_ref().map(|h| h.stamp_ns).unwrap_or(0);
    let mut kv: Vec<(String, String)> = ev.kv.into_iter().collect();
    kv.sort();
    diag::RobotEvent { seq: 0, severity: ev.severity, code: ev.code, text: ev.text, kv, ts_ns }
}

fn op_mode_name(m: i32) -> &'static str {
    match m {
        1 => "DISABLED",
        2 => "ACTIVE",
        3 => "PASSIVE",
        4 => "GRAVITY_COMP",
        100 => "FAULT",
        101 => "CALIBRATING",
        _ => "UNSPECIFIED",
    }
}

/// 发现到的一个机械臂。
#[derive(Serialize, Clone)]
pub struct ArmInfo {
    pub prefix: String,
    pub model: String,
    pub dof: u32,
    pub has_ee: bool,       // 是否装了末端执行器(目前恒 false,夹爪后续再加)
    pub ee_model: String,
}

/// 推给前端的状态快照。
#[derive(Serialize, Clone, Default)]
pub struct ZenohArmState {
    pub controlling: bool,
    pub holder: u32,
    pub mode: String,        // 我方所设 OperatingMode 名(控制器不回传 OperatingMode)——仅取控时有意义
    pub robot_mode: String,  // 控制器 RobotMode 名(只读观察):STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR
    pub overtaken_reason: String, // OVERTAKEN 时的接管原因(human_readable 或 OvertakenMode 名),否则空
    pub model: String,
    pub prefix: String,
    pub dof: u32,
    pub joint_names: Vec<String>,
    pub pos_min: Vec<f32>,
    pub pos_max: Vec<f32>,
    pub q: Vec<f32>,
    pub dq: Vec<f32>,
    pub tau: Vec<f32>,
    pub temp: Vec<f32>,      // 各关节温度 ℃(JointState.temp;电机未上报则为空)
    pub gravity: [f32; 3],   // 我方所设 base 系重力(默认 [0,0,-9.81])
    pub has_ee: bool,
    pub ee_model: String,
    pub fatal: bool,         // RobotStatus.mode==FATAL_ERROR(电机故障/离线锁存,P1-3)→ 需 clear_fault
}

/// 一次 URDF 拉取的结果(推给前端 3D 渲染)。`assembled` 由 XML 内容判定:机器人级
/// `<prefix>/urdf` 在 EE 拼装完成前也会回退供臂-only XML,故只按是否含 `ee_mount` fixed joint 判真装配。
#[derive(Serialize, Clone, Default)]
pub struct ArmUrdf {
    pub xml: String,
    pub assembled: bool,   // 含 EE(整机)→ true;臂-only 或回退 → false
    pub tip_link: String,  // 工具安装 link 名(EE 拼接处)
}

struct Ctrl {
    prefix: StdMutex<Option<String>>,
    session_id: AtomicU32,
    target: StdMutex<Option<Vec<f32>>>, // Active 时 50Hz 流的目标位姿
    gains: StdMutex<(f32, f32)>,        // (kp, kd) —— host 侧定增益(控制器忠实执行)
    dynamics: StdMutex<Option<Arc<ArmDynamics>>>, // 取控时从 arm/urdf 建;host 端重力前馈 tau_ff=G(q) 用
    state: StdMutex<ZenohArmState>,
    // 观察视图(joint_state/status/log/events)——与取控解耦:选中即聚焦,只读也能看(设计:读永远开放,
    // 任意多客户订阅状态不需要会话,独占只针对控制)。取控隐含观察(见 acquire)。
    view_prefix: StdMutex<Option<String>>,   // 当前观察的机器 prefix(过滤 joint_state/status/events/logs)
    logs: StdMutex<VecDeque<diag::LogLine>>,
    events: StdMutex<diag::EventBuf>,        // 环形缓冲 + 单调 seq + 通知 baseline(同锁原子)
}

pub struct ZenohArmConn {
    session: zenoh::Session,
    ctrl: Arc<Ctrl>,
}

impl ZenohArmConn {
    pub async fn open(connect: &str) -> anyhow::Result<Self> {
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("mode", "\"peer\"").unwrap();
        if !connect.is_empty() {
            cfg.insert_json5("connect/endpoints", &format!("[\"{connect}\"]")).unwrap();
        }
        let session = zenoh::open(cfg).await.map_err(|e| anyhow!("zenoh open: {e}"))?;
        tokio::time::sleep(Duration::from_millis(700)).await;
        let mut s0 = ZenohArmState::default();
        s0.gravity = [0.0, 0.0, -9.81];
        let ctrl = Arc::new(Ctrl {
            prefix: StdMutex::new(None),
            session_id: AtomicU32::new(0),
            target: StdMutex::new(None),
            gains: StdMutex::new((10.0, 1.5)), // 有重力前馈后 kp=10 已够,更柔和
            dynamics: StdMutex::new(None),
            state: StdMutex::new(s0),
            view_prefix: StdMutex::new(None),
            logs: StdMutex::new(VecDeque::new()),
            events: StdMutex::new(diag::EventBuf::default()),
        });

        // 50Hz 命令流:仅在持有会话且设了目标(Active 移动)时发 JointTrajectory(喂看门狗 + 命令目标)。
        // GRAVITY_COMP/PASSIVE 不设目标 → 不发(看门狗在控制器侧只对 ACTIVE 生效)。
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
                    let Some(target) = c.target.lock().unwrap().clone() else { continue };
                    let (kp, kd) = *c.gains.lock().unwrap();
                    let n = target.len();
                    // host 端重力前馈:tau_ff = G(q_当前)。在臂**当前所在**算重力(control 在哪补哪)→
                    // Active 位置控制下不再因重力下垂/漂移。与控制器 GRAVITY_COMP **同一 G(q)**(共用 crate)。
                    // 模型未加载 / q 维度不符 → 空 tau_ff(优雅退化为纯 kp/kd)。
                    let tau_ff = {
                        let dyn_guard = c.dynamics.lock().unwrap();
                        let st = c.state.lock().unwrap();
                        match dyn_guard.as_ref() {
                            Some(d) if d.dof() == n && st.q.len() == n =>
                                d.gravity_torque_with(&st.q, st.gravity),
                            _ => vec![],
                        }
                    };
                    let jt = pb::JointTrajectory {
                        header: None,
                        session_id: sid,
                        points: vec![pb::JointSetpoint { q: target, dq: vec![], kp: vec![kp; n], kd: vec![kd; n], tau_ff }],
                        t_from_start_ns: vec![0],
                        on_timeout: pb::TimeoutBehavior::Hold as i32,
                    };
                    let _ = s.put(format!("{prefix}/arm/command"), enc(&jt)).await;
                }
            });
        }
        // joint_state 订阅(通配,按当前**观察**的 prefix 精确匹配 —— 避免 arm0 前缀吃到 arm00 的帧)。
        // 只读:据 view_prefix 过滤,不需要取控(设计:读永远开放)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/arm/joint_state").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{p}/arm/joint_state") { continue; }
                    if let Ok(js) = pb::JointState::decode(&*sample.payload().to_bytes()) {
                        let mut st = c.state.lock().unwrap();
                        st.q = js.q; st.dq = js.dq; st.tau = js.tau_est; st.temp = js.temp;
                    }
                }
            });
        }
        // status 订阅:FATAL 灯 + holder 据"当前观察的机器"(view_prefix)判定 —— 取控/只读/仅选中都能看到
        // 故障灯与谁在控,不需要会话(设计:读永远开放)。失控检测另按我们**取控**的 prefix。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Ok(s) = pb::RobotStatus::decode(&*sample.payload().to_bytes()) else { continue };
                    let key = sample.key_expr().as_str();
                    // 只读观测:FATAL 灯 + holder 按当前观察的机器刷新(holder != 0 且非我方 → 前端"被占 #N")。
                    if let Some(vp) = c.view_prefix.lock().unwrap().clone() {
                        if key == format!("{vp}/status") {
                            let mut st = c.state.lock().unwrap();
                            st.fatal = s.mode == pb::RobotMode::FatalError as i32;
                            st.holder = s.session_holder;
                            st.robot_mode = diag::robot_mode_name(s.mode).into();
                            st.overtaken_reason = s.overtaken_reason.as_ref()
                                .map(|r| diag::overtaken_text(r.mode, r.human_readable.as_deref()))
                                .unwrap_or_default();
                        }
                    }
                    // 失控判定:仅针对我们取控的 prefix —— 自以为在控但 holder 已不是我们(看门狗超时/被
                    // 接管)→ 放弃控制权(读流不受影响,仍可继续只读观察)。
                    let Some(p) = c.prefix.lock().unwrap().clone() else { continue };
                    if key != format!("{p}/status") { continue; }
                    let our_sid = c.session_id.load(Ordering::Relaxed);
                    if our_sid != 0 && s.session_holder != our_sid {
                        c.session_id.store(0, Ordering::Relaxed);
                        *c.target.lock().unwrap() = None;
                        let mut st = c.state.lock().unwrap();
                        st.controlling = false; st.holder = s.session_holder; st.mode = "DISABLED".into();
                        log::warn!("Arm: 失去控制权(当前 holder={})", s.session_holder);
                    }
                }
            });
        }
        // 日志订阅(尽力层,P1-7):hexmeow/<cid>/*/log 全进程 tee;按 view_prefix 的 cid 过滤后进环形缓冲。
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
        // 事件订阅(可靠层,P1-3):<prefix>/events 逐条;按 view_prefix 精确匹配后进环形缓冲(带单调 seq)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/events").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(dp) = c.view_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{dp}/events") { continue; }
                    if let Ok(ev) = pb::Event::decode(&*sample.payload().to_bytes()) {
                        c.events.lock().unwrap().push_live(to_event(ev));
                    }
                }
            });
        }

        Ok(Self { session, ctrl })
    }

    pub async fn discover(&self) -> Vec<ArmInfo> {
        let mut out = Vec::new();
        if let Ok(replies) = self.session.get("hexmeow/**/description").await {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    if let Ok(d) = pb::RobotDescription::decode(&*sample.payload().to_bytes()) {
                        if d.kind == pb::RobotKind::Arm as i32 {
                            let key = sample.key_expr().as_str();
                            let prefix = key.strip_suffix("/description").unwrap_or(key).to_string();
                            // EE:device_keys 里有 /ee 即视为装了 EE(目前没夹爪 → 多半 false)。
                            let has_ee = d.device_keys.iter().any(|k| k.ends_with("/ee"));
                            let dof = query_one::<pb::ArmDescription>(&self.session, &format!("{prefix}/arm/description"), vec![])
                                .await.map(|a| a.dof).unwrap_or(0);
                            out.push(ArmInfo { prefix, model: d.model, dof, has_ee, ee_model: String::new() });
                        }
                    }
                }
            }
        }
        out
    }

    pub async fn acquire(&self, prefix: &str, model: &str) -> anyhow::Result<()> {
        // 换机取控:已持有别台 → 先释放旧会话(会话跨切换保持后,换机不再要求手动释放;
        // 一个模块同时只持一台,同 kind 多持是后续项)。
        if self.ctrl.session_id.load(Ordering::Relaxed) != 0 {
            let cur = self.ctrl.prefix.lock().unwrap().clone();
            if cur.as_deref() != Some(prefix) { self.release().await; }
        }
        let req = pb::AcquireSessionRequest { client_name: Some("hex-motor-gui".into()), liveliness_key: None };
        let resp: pb::AcquireSessionResponse = query_one(&self.session, &format!("{prefix}/rpc/acquire_session"), enc(&req))
            .await.ok_or_else(|| anyhow!("acquire 无回复"))?;
        if !resp.ok {
            return Err(anyhow!("被占用:holder {} {:?}", resp.current_holder, resp.current_holder_name));
        }
        self.ctrl.session_id.store(resp.session_id, Ordering::Relaxed);
        *self.ctrl.prefix.lock().unwrap() = Some(prefix.to_string());
        // 取控隐含观察:确保 joint_state/status 读流也跟到这台(即使前端漏调 set_diag_focus)。
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
        // 取 arm/description 填关节名/限位
        let desc = query_one::<pb::ArmDescription>(&self.session, &format!("{prefix}/arm/description"), vec![]).await;
        // 取 arm/urdf 建重力前馈模型(host 端 tau_ff=G(q);失败则关闭前馈,退化为纯 kp/kd)
        let dynamics = match query_one::<pb::UrdfResource>(&self.session, &format!("{prefix}/arm/urdf"), vec![]).await {
            Some(u) => match ArmDynamics::from_urdf_string(&u.xml) {
                Ok(d) => { log::info!("Arm: 重力前馈模型已加载(dof={})", d.dof()); Some(Arc::new(d)) }
                Err(e) => { log::warn!("Arm: URDF 解析失败,重力前馈关闭: {e}"); None }
            },
            None => { log::warn!("Arm: 无 arm/urdf(控制器未配 URDF_PATH?),重力前馈关闭"); None }
        };
        *self.ctrl.dynamics.lock().unwrap() = dynamics;
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = true; st.prefix = prefix.into(); st.model = model.into(); st.mode = "DISABLED".into();
        if let Some(d) = desc {
            st.dof = d.dof; st.joint_names = d.joint_names; st.pos_min = d.pos_min; st.pos_max = d.pos_max;
        }
        Ok(())
    }

    /// 设 OperatingMode(2=ACTIVE,3=PASSIVE,4=GRAVITY_COMP,1=DISABLED)。非 Active 清目标。
    pub async fn set_mode(&self, mode: i32) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        if mode != 2 { *self.ctrl.target.lock().unwrap() = None; } // 非 Active:停命令流
        let req = pb::SetModeRequest { session_id: sid, mode };
        let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{}/rpc/set_mode", self.prefix()), enc(&req)).await;
        self.ctrl.state.lock().unwrap().mode = op_mode_name(mode).into();
        Ok(())
    }

    /// 移动到预设位姿(进 ACTIVE + 50Hz 流目标)。kp/kd 由 host(GUI)给,控制器忠实执行。
    pub async fn goto(&self, q: Vec<f32>, kp: f32, kd: f32) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        *self.ctrl.gains.lock().unwrap() = (kp, kd);
        *self.ctrl.target.lock().unwrap() = Some(q);
        let req = pb::SetModeRequest { session_id: sid, mode: 2 };
        let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{}/rpc/set_mode", self.prefix()), enc(&req)).await;
        self.ctrl.state.lock().unwrap().mode = "ACTIVE".into();
        Ok(())
    }

    pub async fn set_gravity(&self, g: [f32; 3]) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        let req = pb::SetGravityRequest { session_id: sid, gravity: Some(pb::Vec3 { x: g[0], y: g[1], z: g[2] }) };
        let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{}/rpc/set_gravity", self.prefix()), enc(&req)).await;
        self.ctrl.state.lock().unwrap().gravity = g;
        Ok(())
    }

    pub fn state(&self) -> ZenohArmState {
        self.ctrl.state.lock().unwrap().clone()
    }

    /// 取某臂的 URDF 供前端 3D 渲染(选中即拉,与取控解耦)。先试机器人级 `<prefix>/urdf`
    /// (supervisor 预拼的整机 arm+EE);无/空 xml 则退到 device 级 `<prefix>/arm/urdf`(仅臂)。
    /// `assembled` 按 XML 是否含 `ee_mount` fixed joint 判定(机器人级键在 EE 拼装完成前也回退臂-only)。
    pub async fn get_urdf(&self, prefix: &str) -> Option<ArmUrdf> {
        if let Some(u) = query_one::<pb::UrdfResource>(&self.session, &format!("{prefix}/urdf"), vec![]).await {
            if !u.xml.is_empty() {
                let assembled = u.xml.contains("<joint name=\"ee_mount\"");
                return Some(ArmUrdf { xml: u.xml, assembled, tip_link: u.tip_link });
            }
        }
        let u = query_one::<pb::UrdfResource>(&self.session, &format!("{prefix}/arm/urdf"), vec![]).await?;
        if u.xml.is_empty() { return None; }
        Some(ArmUrdf { xml: u.xml, assembled: false, tip_link: u.tip_link })
    }

    // ───────────────────────── 诊断视图(log / events)─────────────────────────

    /// 观察聚焦:选中某机器即观察它 —— joint_state/status(关节/holder/故障灯)实时刷新 + 拉 arm/description
    /// 填关节名/限位/DOF + 订阅其 events/logs(全部与取控解耦,只读/仅选中也生效;设计:读永远开放)。
    /// 清空旧缓冲、复位随机器变的观测量,再从 `.../events/recent` + `.../log/recent` 播种一次历史(事后连上也查得到,如 0x8130)。
    pub async fn set_diag_focus(&self, prefix: &str) {
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
        // 复位随机器变的只读观测量,等新机器的 joint_state / status / description 覆盖(不残留上一台的关节/限位/holder)。
        {
            let mut st = self.ctrl.state.lock().unwrap();
            st.fatal = false;   // 由 status 订阅按新 prefix 重新点亮
            st.holder = 0;      // 由 status 订阅刷新
            st.robot_mode.clear(); st.overtaken_reason.clear();
            st.q.clear(); st.dq.clear(); st.tau.clear(); st.temp.clear();
            st.dof = 0; st.joint_names.clear(); st.pos_min.clear(); st.pos_max.clear();
            // mode 是"我方所设 OperatingMode"(控制器不回传),属**取控作用域** —— 只读时清空,
            // 不把上一台受控机器的模式冒充成被观察机器的真实模式。取控时 acquire/set_mode 重填。
            st.mode.clear();
        }
        self.ctrl.events.lock().unwrap().clear();
        self.ctrl.logs.lock().unwrap().clear();
        // 拉 arm/description 填关节名/限位/DOF(只读也要:供关节表标签 + 限位 + 3D 标注)。取控时 acquire 也会填。
        if let Some(d) = query_one::<pb::ArmDescription>(&self.session, &format!("{prefix}/arm/description"), vec![]).await {
            let mut st = self.ctrl.state.lock().unwrap();
            st.dof = d.dof; st.joint_names = d.joint_names; st.pos_min = d.pos_min; st.pos_max = d.pos_max;
        }
        self.refresh_diag().await;
    }

    /// 从控制器拉取历史事件 + 日志,替换本地缓冲("刷新历史"按钮或聚焦时调)。事件经
    /// [`EventBuf::reseed`](diag::EventBuf::reseed) 原子重建 + 重置 baseline,使前端不对刚拉回的旧事件
    /// 误弹通知(仅对之后的实时事件弹),且与并发实时 push 无竞态。
    pub async fn refresh_diag(&self) {
        let Some(prefix) = self.ctrl.view_prefix.lock().unwrap().clone() else { return };
        // 事件历史:<prefix>/events/recent → EventLog(单 queryable)。先 await 拿数据,再一把锁内原子重建。
        if let Some(log) = query_one::<pb::EventLog>(&self.session, &format!("{prefix}/events/recent"), vec![]).await {
            let history: Vec<diag::RobotEvent> = log.events.into_iter().map(to_event).collect();
            self.ctrl.events.lock().unwrap().reseed(history);
        }
        // 日志历史:hexmeow/<cid>/*/log/recent → 每进程一个多行 blob。
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

    pub fn get_events(&self) -> diag::EventsSnapshot {
        self.ctrl.events.lock().unwrap().snapshot()
    }

    pub fn get_logs(&self) -> Vec<diag::LogLine> {
        self.ctrl.logs.lock().unwrap().iter().cloned().collect()
    }

    /// P1-3 clear_fault:清除机械臂锁存的 FATAL(需持有会话)。回 ok 则控制器进 IDLE_MODE;
    /// 电机仍坏则控制器如实回错并保持 Fault。
    pub async fn clear_fault(&self) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权(clear_fault 需先取控)")); }
        let req = pb::ClearFaultRequest { session_id: sid };
        let resp: pb::GenericResponse = query_one(&self.session, &format!("{}/rpc/clear_fault", self.prefix()), enc(&req))
            .await.ok_or_else(|| anyhow!("clear_fault 无回复"))?;
        if resp.ok { Ok(()) } else { Err(anyhow!(resp.error.unwrap_or_else(|| "clear_fault 失败".into()))) }
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
        *self.ctrl.dynamics.lock().unwrap() = None;
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = false; st.holder = 0; st.mode = "DISABLED".into();
    }

    fn prefix(&self) -> String {
        self.ctrl.prefix.lock().unwrap().clone().unwrap_or_default()
    }
}
