//! Base(Zenoh):通过 Zenoh 连接 hex-controller 暴露的底盘,做发现 / 取控 / 移动 / 读 odom。
//! 逻辑同 hex-controller 的 base_client,但持久化:一个 Session + 常驻
//! 20Hz cmd_vel 流(喂控制器看门狗)+ odom/status 订阅。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::anyhow;
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
    pub fatal: bool,       // RobotStatus.mode==FATAL_ERROR(电机故障/离线锁存,P1-3)→ 需 clear_fault
}

struct Ctrl {
    prefix: StdMutex<Option<String>>,
    session_id: AtomicU32, // 0 = 未持有
    cmd: StdMutex<(f64, f64, f64)>,
    state: StdMutex<ZenohBaseState>,
    // 诊断视图(log/events 查看)——与取控解耦:选中即聚焦,只读也能看。
    diag_prefix: StdMutex<Option<String>>,   // 当前聚焦的机器 prefix(过滤 events/logs)
    logs: StdMutex<VecDeque<diag::LogLine>>,
    events: StdMutex<diag::EventBuf>,        // 环形缓冲 + 单调 seq + 通知 baseline(同锁原子)
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
        // 给组播探测/建链一点时间,之后 discover 才能发现局域网内的控制器。
        tokio::time::sleep(Duration::from_millis(700)).await;
        let ctrl = Arc::new(Ctrl {
            prefix: StdMutex::new(None),
            session_id: AtomicU32::new(0),
            cmd: StdMutex::new((0.0, 0.0, 0.0)),
            state: StdMutex::new(ZenohBaseState::default()),
            diag_prefix: StdMutex::new(None),
            logs: StdMutex::new(VecDeque::new()),
            events: StdMutex::new(diag::EventBuf::default()),
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
        // odom 订阅(通配,按当前 prefix 精确匹配 —— 避免 base0 前缀吃到 base00 的帧)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/base/odom").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let cur = c.prefix.lock().unwrap().clone();
                    let Some(p) = cur else { continue };
                    if sample.key_expr().as_str() != format!("{p}/base/odom") { continue; }
                    if let Ok(o) = pb::Odometry::decode(&*sample.payload().to_bytes()) {
                        let t = o.twist.unwrap_or_default();
                        let mut st = c.state.lock().unwrap();
                        st.pose_x = o.x as f64; st.pose_y = o.y as f64; st.pose_theta = o.theta as f64;
                        st.vx = t.vx as f64; st.vy = t.vy as f64; st.wz = t.wz as f64;
                    }
                }
            });
        }
        // status 订阅 → holder/running(按取控 prefix)+ FATAL_ERROR 灯(按诊断聚焦 prefix,只读也可见)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Ok(s) = pb::RobotStatus::decode(&*sample.payload().to_bytes()) else { continue };
                    let key = sample.key_expr().as_str();
                    // P1-3:FATAL_ERROR 锁存据当前聚焦机器判定,取控/只读/仅选中都能看到故障灯。
                    let dp = c.diag_prefix.lock().unwrap().clone();
                    if let Some(dp) = dp {
                        if key == format!("{dp}/status") {
                            c.state.lock().unwrap().fatal = s.mode == pb::RobotMode::FatalError as i32;
                        }
                    }
                    // holder/running:按我们取控的 prefix。
                    let Some(p) = c.prefix.lock().unwrap().clone() else { continue };
                    if key != format!("{p}/status") { continue; }
                    let mut st = c.state.lock().unwrap();
                    st.holder = s.session_holder;
                    st.running = s.mode == pb::RobotMode::Running as i32;
                }
            });
        }
        // 日志订阅(尽力层,P1-7):hexmeow/<cid>/*/log 全进程 tee;按 diag_prefix 的 cid 过滤后进环形缓冲。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/log").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(dp) = c.diag_prefix.lock().unwrap().clone() else { continue };
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
        // 事件订阅(可靠层,P1-3):<prefix>/events 逐条;按 diag_prefix 精确匹配后进环形缓冲(带单调 seq)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/events").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(dp) = c.diag_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{dp}/events") { continue; }
                    if let Ok(ev) = pb::Event::decode(&*sample.payload().to_bytes()) {
                        c.events.lock().unwrap().push_live(to_event(ev));
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

    // ───────────────────────── 诊断视图(log / events)─────────────────────────

    /// 诊断聚焦:选中某机器即订阅其 events/logs(与取控解耦,只读/仅选中也生效)。清空旧缓冲并从
    /// `.../events/recent` + `.../log/recent` 播种一次历史(事后连上也查得到,如底盘拔轮)。
    pub async fn set_diag_focus(&self, prefix: &str) {
        *self.ctrl.diag_prefix.lock().unwrap() = Some(prefix.to_string());
        self.ctrl.state.lock().unwrap().fatal = false; // 由 status 订阅按新 prefix 重新点亮
        self.ctrl.events.lock().unwrap().clear();
        self.ctrl.logs.lock().unwrap().clear();
        self.refresh_diag().await;
    }

    /// 从控制器拉取历史事件 + 日志,替换本地缓冲("刷新历史"按钮或聚焦时调)。事件经
    /// [`EventBuf::reseed`](diag::EventBuf::reseed) 原子重建 + 重置 baseline,使前端不对刚拉回的旧事件
    /// 误弹通知(仅对之后的实时事件弹),且与并发实时 push 无竞态。
    pub async fn refresh_diag(&self) {
        let Some(prefix) = self.ctrl.diag_prefix.lock().unwrap().clone() else { return };
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

    /// P1-3 clear_fault:清除底盘锁存的 FATAL(需持有会话)。回 ok 则控制器进 IDLE_MODE;
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
