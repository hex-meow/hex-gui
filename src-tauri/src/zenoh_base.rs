//! Base(Zenoh):通过 Zenoh 连接 hex-controller 暴露的底盘,做发现 / 取控 / 移动 / 读 odom。
//! 逻辑同 hex-controller 的 base_client,但持久化:一个 Session + 常驻
//! 20Hz cmd_vel 流(喂控制器看门狗)+ odom/status 订阅。

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

/// 发现到的一个底盘。
#[derive(Serialize, Clone)]
pub struct BaseInfo {
    pub prefix: String,
    pub model: String,
}

/// 推给前端的状态快照。
#[derive(Serialize, Clone, Default)]
pub struct ZenohBaseState {
    pub controlling: bool, // 我们是否持有会话
    pub holder: u32,       // 当前 holder(0=无)
    pub running: bool,     // RobotMode==RUNNING
    pub model: String,
    pub prefix: String,
    pub pose_x: f64,
    pub pose_y: f64,
    pub pose_theta: f64,
    pub vx: f64,
    pub vy: f64,
    pub wz: f64,
}

struct Ctrl {
    prefix: StdMutex<Option<String>>,
    session_id: AtomicU32, // 0 = 未持有
    cmd: StdMutex<(f64, f64, f64)>,
    state: StdMutex<ZenohBaseState>,
}

/// 一条到控制器网络的连接(持久 Session + 常驻任务)。
pub struct ZenohConn {
    session: zenoh::Session,
    ctrl: Arc<Ctrl>,
}

impl ZenohConn {
    pub async fn open(connect: &str) -> anyhow::Result<Self> {
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("mode", "\"peer\"").unwrap();
        if !connect.is_empty() {
            cfg.insert_json5("connect/endpoints", &format!("[\"{connect}\"]")).unwrap();
        }
        let session = zenoh::open(cfg).await.map_err(|e| anyhow!("zenoh open: {e}"))?;
        let ctrl = Arc::new(Ctrl {
            prefix: StdMutex::new(None),
            session_id: AtomicU32::new(0),
            cmd: StdMutex::new((0.0, 0.0, 0.0)),
            state: StdMutex::new(ZenohBaseState::default()),
        });

        // 20Hz cmd_vel 流(喂看门狗)。
        {
            let s = session.clone();
            let c = ctrl.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(50));
                loop {
                    tick.tick().await;
                    let sid = c.session_id.load(Ordering::Relaxed);
                    if sid == 0 { continue; }
                    let Some(prefix) = c.prefix.lock().unwrap().clone() else { continue };
                    let (vx, vy, wz) = *c.cmd.lock().unwrap();
                    let cmd = pb::BaseCommand {
                        session_id: sid,
                        twist: Some(pb::Twist { vx: vx as f32, vy: vy as f32, wz: wz as f32 }),
                    };
                    let _ = s.put(format!("{prefix}/base/cmd_vel"), enc(&cmd)).await;
                }
            });
        }
        // odom 订阅(通配,按当前 prefix 过滤)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/base/odom").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let cur = c.prefix.lock().unwrap().clone();
                    let Some(p) = cur else { continue };
                    if !sample.key_expr().as_str().starts_with(&p) { continue; }
                    if let Ok(o) = pb::Odometry::decode(&*sample.payload().to_bytes()) {
                        let t = o.twist.unwrap_or_default();
                        let mut st = c.state.lock().unwrap();
                        st.pose_x = o.x as f64; st.pose_y = o.y as f64; st.pose_theta = o.theta as f64;
                        st.vx = t.vx as f64; st.vy = t.vy as f64; st.wz = t.wz as f64;
                    }
                }
            });
        }
        // status 订阅 → holder / running。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let cur = c.prefix.lock().unwrap().clone();
                    let Some(p) = cur else { continue };
                    if !sample.key_expr().as_str().starts_with(&p) { continue; }
                    if let Ok(s) = pb::RobotStatus::decode(&*sample.payload().to_bytes()) {
                        let mut st = c.state.lock().unwrap();
                        st.holder = s.session_holder;
                        st.running = s.mode == pb::RobotMode::Running as i32;
                    }
                }
            });
        }

        Ok(Self { session, ctrl })
    }

    pub async fn discover(&self) -> Vec<BaseInfo> {
        let mut out = Vec::new();
        if let Ok(replies) = self.session.get("hexmeow/**/description").await {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    if let Ok(d) = pb::RobotDescription::decode(&*sample.payload().to_bytes()) {
                        if d.kind == pb::RobotKind::Base as i32 {
                            let key = sample.key_expr().as_str();
                            let prefix = key.strip_suffix("/description").unwrap_or(key).to_string();
                            out.push(BaseInfo { prefix, model: d.model });
                        }
                    }
                }
            }
        }
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
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = true; st.prefix = prefix.into(); st.model = model.into();
        Ok(())
    }

    pub async fn set_active(&self, on: bool) -> anyhow::Result<()> {
        let sid = self.ctrl.session_id.load(Ordering::Relaxed);
        if sid == 0 { return Err(anyhow!("未持有控制权")); }
        let req = pb::SetModeRequest {
            session_id: sid,
            mode: if on { pb::OperatingMode::Active as i32 } else { pb::OperatingMode::Disabled as i32 },
        };
        let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{}/rpc/set_mode", self.prefix()), enc(&req)).await;
        if !on { *self.ctrl.cmd.lock().unwrap() = (0.0, 0.0, 0.0); }
        Ok(())
    }

    pub fn set_cmd(&self, vx: f64, vy: f64, wz: f64) {
        *self.ctrl.cmd.lock().unwrap() = (vx, vy, wz);
    }

    pub fn state(&self) -> ZenohBaseState {
        self.ctrl.state.lock().unwrap().clone()
    }

    pub async fn release(&self) {
        let sid = self.ctrl.session_id.swap(0, Ordering::Relaxed);
        *self.ctrl.cmd.lock().unwrap() = (0.0, 0.0, 0.0);
        let prefix = self.ctrl.prefix.lock().unwrap().clone();
        if let (Some(prefix), true) = (prefix, sid != 0) {
            let req = pb::ReleaseSessionRequest { session_id: sid };
            let _: Option<pb::GenericResponse> = query_one(&self.session, &format!("{prefix}/rpc/release_session"), enc(&req)).await;
        }
        *self.ctrl.prefix.lock().unwrap() = None;
        let mut st = self.ctrl.state.lock().unwrap();
        st.controlling = false; st.holder = 0; st.running = false;
    }

    fn prefix(&self) -> String {
        self.ctrl.prefix.lock().unwrap().clone().unwrap_or_default()
    }
}
