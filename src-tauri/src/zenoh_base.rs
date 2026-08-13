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
use crate::zenoh_discovery::{
    robot_prefix_from_description_reply, ROBOT_DESCRIPTION_SELECTOR,
};

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

/// Optional capability query with strict reply decoding.
///
/// A receiver closing without a reply means that no queryable matched (the
/// expected result when talking to a pre-0.2 controller). Once a reply exists,
/// however, a remote error, wrong reply key, or malformed protobuf is a broken
/// contract and must not be silently downgraded to "unsupported".
async fn query_optional_strict<Resp: Message + Default>(
    session: &zenoh::Session,
    key: &str,
    payload: Vec<u8>,
) -> anyhow::Result<Option<Resp>> {
    let replies = session
        .get(key)
        .payload(payload)
        .timeout(Duration::from_secs(2))
        .await
        .map_err(|e| anyhow!("查询 {key}: {e}"))?;
    let reply = match replies.recv_async().await {
        Ok(reply) => reply,
        Err(_) => return Ok(None),
    };
    let sample = reply
        .result()
        .map_err(|e| anyhow!("{key} 返回 Zenoh 错误: {e}"))?;
    if sample.key_expr().as_str() != key {
        return Err(anyhow!(
            "{key} 返回了意外的 reply key {}",
            sample.key_expr().as_str()
        ));
    }
    Resp::decode(&*sample.payload().to_bytes())
        .map(Some)
        .map_err(|e| anyhow!("{key} 返回了畸形 protobuf: {e}"))
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

/// One runtime-settable base acceleration axis.
///
/// All four numbers come from the controller: `default_value`, `min`, and
/// `max` from `base/description`; `current` from `base/limits` (or the
/// `rpc/set_limits` response). The GUI therefore never bakes model constants
/// into its controls.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BaseLimitAxisDto {
    pub current: f64,
    pub default_value: f64,
    pub min: f64,
    pub max: f64,
}

/// Runtime-settable acceleration capabilities of a base.
///
/// An axis is `None` when that controller does not advertise it as settable.
/// The whole query returns `None` for old controllers that expose neither the
/// range declaration nor the current-value queryable.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BaseLimitsDto {
    pub linear: Option<BaseLimitAxisDto>,
    pub angular: Option<BaseLimitAxisDto>,
}

#[derive(Clone, Copy, Debug)]
struct BaseLimitAxisSchema {
    default_value: f64,
    min: f64,
    max: f64,
}

#[derive(Clone, Debug)]
struct BaseLimitsSchema {
    linear: Option<BaseLimitAxisSchema>,
    angular: Option<BaseLimitAxisSchema>,
}

fn parse_limit_axis_schema(
    name: &str,
    default_value: Option<f32>,
    min: Option<f32>,
    max: Option<f32>,
) -> anyhow::Result<Option<BaseLimitAxisSchema>> {
    let (min, max) = match (min, max) {
        (None, None) => return Ok(None),
        (Some(min), Some(max)) => (f64::from(min), f64::from(max)),
        _ => {
            return Err(anyhow!(
                "base/description 的 {name} 范围不完整(settable_min/max 必须成对出现)"
            ));
        }
    };
    let default_value = default_value
        .map(f64::from)
        .ok_or_else(|| anyhow!("base/description 缺少 {name} 的默认值"))?;
    for (field, value) in [
        ("default", default_value),
        ("settable_min", min),
        ("settable_max", max),
    ] {
        if !value.is_finite() {
            return Err(anyhow!(
                "base/description 的 {name}.{field} 不是有限值: {value}"
            ));
        }
    }
    if min <= 0.0 || min >= max {
        return Err(anyhow!(
            "base/description 的 {name} 范围非法: [{min}, {max}](要求 0 < min < max)"
        ));
    }
    if !(min..=max).contains(&default_value) {
        return Err(anyhow!(
            "base/description 的 {name} 默认值 {default_value} 不在 [{min}, {max}] 内"
        ));
    }
    Ok(Some(BaseLimitAxisSchema {
        default_value,
        min,
        max,
    }))
}

/// Decode the static capability declaration. `Ok(None)` is deliberately
/// reserved for a well-formed old/unsupported description; partial new fields
/// are errors rather than a reason to hide a broken controller response.
fn parse_limits_schema(
    description: &pb::BaseDescription,
) -> anyhow::Result<Option<BaseLimitsSchema>> {
    let (min, max) = match (
        description.settable_min.as_ref(),
        description.settable_max.as_ref(),
    ) {
        (None, None) => return Ok(None),
        (Some(min), Some(max)) => (min, max),
        _ => {
            return Err(anyhow!(
                "base/description 的 settable_min/settable_max 必须成对出现"
            ));
        }
    };
    if min.session_id != 0 || max.session_id != 0 {
        return Err(anyhow!("base/description 的范围错误地携带了 session_id"));
    }

    let defaults = description.default_limits.as_ref();
    if defaults.is_some_and(|limits| limits.session_id != 0) {
        return Err(anyhow!(
            "base/description 的默认限位错误地携带了 session_id"
        ));
    }
    let linear = parse_limit_axis_schema(
        "linear",
        defaults.and_then(|v| v.accel_max),
        min.accel_max,
        max.accel_max,
    )?;
    let angular = parse_limit_axis_schema(
        "angular",
        defaults.and_then(|v| v.angular_accel_max),
        min.angular_accel_max,
        max.angular_accel_max,
    )?;

    if linear.is_none() && angular.is_none() {
        return Ok(None);
    }
    Ok(Some(BaseLimitsSchema { linear, angular }))
}

fn build_limit_axis_dto(
    source: &str,
    name: &str,
    schema: Option<BaseLimitAxisSchema>,
    current: Option<f32>,
) -> anyhow::Result<Option<BaseLimitAxisDto>> {
    let Some(schema) = schema else {
        return Ok(None);
    };
    let current = current
        .map(f64::from)
        .ok_or_else(|| anyhow!("{source} 缺少已声明可设置的 {name} 当前值"))?;
    if !current.is_finite() {
        return Err(anyhow!("{source} 的 {name} 当前值不是有限值: {current}"));
    }
    if !(schema.min..=schema.max).contains(&current) {
        return Err(anyhow!(
            "{source} 的 {name} 当前值 {current} 不在声明范围 [{}, {}] 内",
            schema.min,
            schema.max
        ));
    }
    Ok(Some(BaseLimitAxisDto {
        current,
        default_value: schema.default_value,
        min: schema.min,
        max: schema.max,
    }))
}

fn build_limits_dto(
    source: &str,
    schema: &BaseLimitsSchema,
    current: &pb::BaseLimits,
) -> anyhow::Result<BaseLimitsDto> {
    if current.session_id != 0 {
        return Err(anyhow!("{source} 响应错误地携带了 session_id"));
    }
    Ok(BaseLimitsDto {
        linear: build_limit_axis_dto(source, "linear", schema.linear, current.accel_max)?,
        angular: build_limit_axis_dto(
            source,
            "angular",
            schema.angular,
            current.angular_accel_max,
        )?,
    })
}

fn validate_requested_limit(
    name: &str,
    requested: Option<f64>,
    schema: Option<BaseLimitAxisSchema>,
) -> anyhow::Result<Option<f32>> {
    let Some(value) = requested else {
        return Ok(None);
    };
    let schema =
        schema.ok_or_else(|| anyhow!("unsupported:控制器未声明 {name} 加速度为可运行时设置"))?;
    if !value.is_finite() {
        return Err(anyhow!("{name} 加速度必须是有限值,收到 {value}"));
    }

    // The contract is f32, including its advertised bounds. Validate the
    // value after narrowing so a natural decimal such as 0.2 is accepted at
    // an advertised 0.2_f32 boundary, while the exact bytes sent on the wire
    // can never land outside the controller's range.
    let wire = value as f32;
    let wire_value = f64::from(wire);
    if !wire.is_finite() || !(schema.min..=schema.max).contains(&wire_value) {
        return Err(anyhow!(
            "{name} 加速度 {value} 无法在协议精度下表示为范围 [{}, {}] 内的值",
            schema.min,
            schema.max
        ));
    }
    Ok(Some(wire))
}

fn limits_session_id(held_session: u32, held_prefix: Option<&str>, target_prefix: &str) -> u32 {
    if held_session != 0 && held_prefix == Some(target_prefix) {
        held_session
    } else {
        0
    }
}

fn validate_set_limits_response(
    schema: &BaseLimitsSchema,
    response: &pb::SetBaseLimitsResponse,
    requested_linear: Option<f32>,
    requested_angular: Option<f32>,
) -> anyhow::Result<BaseLimitsDto> {
    let applied = response
        .applied
        .as_ref()
        .ok_or_else(|| anyhow!("rpc/set_limits 响应缺少 applied 当前值"))?;
    let dto = build_limits_dto("rpc/set_limits.applied", schema, applied)?;

    if !response.ok {
        let error = response
            .error
            .as_deref()
            .map(str::trim)
            .filter(|error| !error.is_empty())
            .ok_or_else(|| anyhow!("rpc/set_limits 拒绝请求但未提供 error"))?;
        return Err(anyhow!("rpc/set_limits 被拒绝:{error}"));
    }
    if response.error.is_some() {
        return Err(anyhow!("rpc/set_limits 成功响应不应携带 error"));
    }

    for (name, requested, axis) in [
        ("linear", requested_linear, dto.linear.as_ref()),
        ("angular", requested_angular, dto.angular.as_ref()),
    ] {
        if let Some(requested) = requested {
            let applied = axis
                .ok_or_else(|| anyhow!("rpc/set_limits.applied 缺少请求的 {name} 当前值"))?
                .current;
            if applied != f64::from(requested) {
                return Err(anyhow!(
                    "rpc/set_limits.applied 的 {name}={applied} 与请求值 {} 不一致",
                    f64::from(requested)
                ));
            }
        }
    }
    Ok(dto)
}

fn decode_base_info_reply(key: &str, payload: &[u8]) -> Option<BaseInfo> {
    let description = pb::RobotDescription::decode(payload).ok()?;
    if description.kind != pb::RobotKind::Base as i32 {
        return None;
    }
    let prefix =
        robot_prefix_from_description_reply(key, &description.robot_index)?;
    Some(BaseInfo { prefix: prefix.to_string(), model: description.model })
}

/// 推给前端的状态快照。
#[derive(Serialize, Clone, Default)]
pub struct ZenohBaseState {
    pub controlling: bool,       // 我们是否持有会话
    pub holder: u32,             // 当前 holder(0=无)
    pub running: bool,           // RobotMode==RUNNING(便捷布尔;完整模式见 robot_mode)
    pub robot_mode: String,      // 控制器 RobotMode 名(只读观察):STANDBY/RUNNING/OVERTAKEN/FATAL_ERROR
    pub overtaken_reason: String, // OVERTAKEN 时的接管原因(human_readable 或 OvertakenMode 名),否则空
    pub model: String,
    pub prefix: String,
    pub pose_x: f64,
    pub pose_y: f64,
    pub pose_theta: f64,
    pub vx: f64,
    pub vy: f64,
    pub wz: f64,
    pub fatal: bool,       // RobotStatus.mode==FATAL_ERROR(机器人故障锁存)→ 原因查看 Events
}

struct Ctrl {
    prefix: StdMutex<Option<String>>,
    session_id: AtomicU32, // 0 = 未持有
    /// 取控期间必须一直拿住的 liveliness 租约(见 zenoh_lease)。
    /// drop 即等于告诉控制器"客户端没了" —— 故只在 release 时清空。
    live_lease: StdMutex<Option<crate::zenoh_lease::SessionLease>>,
    cmd: StdMutex<(f64, f64, f64)>,
    state: StdMutex<ZenohBaseState>,
    // 观察视图(odom/status/log/events)——与取控解耦:选中即聚焦,只读也能看(设计:读永远开放,
    // 任意多客户订阅状态不需要会话,独占只针对控制)。取控隐含观察(见 acquire)。
    view_prefix: StdMutex<Option<String>>,   // 当前观察的机器 prefix(过滤 odom/status/events/logs)
    logs: StdMutex<VecDeque<diag::LogLine>>,
    events: StdMutex<diag::EventBuf>,        // 环形缓冲 + 单调 seq + 通知 baseline(同锁原子)
}

/// 一条到控制器网络的连接(持久 Session + 常驻任务)。
#[derive(Clone)]
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
            live_lease: StdMutex::new(None),
            cmd: StdMutex::new((0.0, 0.0, 0.0)),
            state: StdMutex::new(ZenohBaseState::default()),
            view_prefix: StdMutex::new(None),
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
        // odom 订阅(通配,按当前**观察**的 prefix 精确匹配 —— 避免 base0 前缀吃到 base00 的帧)。
        // 只读:据 view_prefix 过滤,不需要取控(设计:读永远开放)。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/base/odom").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Some(p) = c.view_prefix.lock().unwrap().clone() else { continue };
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
        // status 订阅:holder / running / FATAL 灯都据"当前观察的机器"(view_prefix)判定 ——
        // 取控/只读/仅选中都能看到谁在控、是否 RUNNING、故障灯,不需要会话(设计:读永远开放)。
        // holder != 0 且不是我们 → 前端显示"被占 #N",让第二个操作者知道正被别人控制。
        if let Ok(sub) = session.declare_subscriber("hexmeow/**/status").await {
            let c = ctrl.clone();
            tokio::spawn(async move {
                while let Ok(sample) = sub.recv_async().await {
                    let Ok(s) = pb::RobotStatus::decode(&*sample.payload().to_bytes()) else { continue };
                    let Some(vp) = c.view_prefix.lock().unwrap().clone() else { continue };
                    if sample.key_expr().as_str() != format!("{vp}/status") { continue; }
                    let mut st = c.state.lock().unwrap();
                    st.fatal = s.mode == pb::RobotMode::FatalError as i32;
                    st.holder = s.session_holder;
                    st.running = s.mode == pb::RobotMode::Running as i32;
                    st.robot_mode = diag::robot_mode_name(s.mode).into();
                    st.overtaken_reason = s.overtaken_reason.as_ref()
                        .map(|r| diag::overtaken_text(r.mode, r.human_readable.as_deref()))
                        .unwrap_or_default();
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

    pub async fn discover(&self) -> Vec<BaseInfo> {
        let mut out = Vec::new();
        if let Ok(replies) = self.session.get(ROBOT_DESCRIPTION_SELECTOR).await {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    let payload = sample.payload().to_bytes();
                    if let Some(info) =
                        decode_base_info_reply(sample.key_expr().as_str(), &payload)
                    {
                        out.push(info);
                    }
                }
            }
        }
        out
    }

    /// Fetch the static acceleration capability and its current value as one
    /// coherent frontend snapshot. Missing capability/queryables are normal
    /// for old controllers and return `None`; malformed replies are errors.
    async fn fetch_limits_snapshot(
        &self,
        prefix: &str,
    ) -> anyhow::Result<Option<(BaseLimitsSchema, BaseLimitsDto)>> {
        if prefix.is_empty() {
            return Err(anyhow!("底盘 prefix 不能为空"));
        }
        let description_key = format!("{prefix}/base/description");
        let Some(description) = query_optional_strict::<pb::BaseDescription>(
            &self.session,
            &description_key,
            Vec::new(),
        )
        .await?
        else {
            return Ok(None);
        };
        let Some(schema) = parse_limits_schema(&description)? else {
            return Ok(None);
        };

        let current_key = format!("{prefix}/base/limits");
        let current = query_optional_strict::<pb::BaseLimits>(
            &self.session,
            &current_key,
            Vec::new(),
        )
        .await?
        .ok_or_else(|| {
            anyhow!(
                "{current_key} 无回复，但 base/description 已声明运行时可设置范围"
            )
        })?;
        let dto = build_limits_dto("base/limits", &schema, &current)?;
        Ok(Some((schema, dto)))
    }

    /// Read controller-advertised defaults/ranges plus the live acceleration
    /// limits. `None` means that this (usually old) controller does not support
    /// the runtime acceleration-limit API.
    pub async fn get_limits(&self, prefix: &str) -> anyhow::Result<Option<BaseLimitsDto>> {
        Ok(self
            .fetch_limits_snapshot(prefix)
            .await?
            .map(|(_, dto)| dto))
    }

    /// Apply one or both runtime acceleration limits.
    ///
    /// When this connection holds `prefix`, the holder session is sent. When
    /// it does not hold that base, session 0 is sent so an idle controller can
    /// still be configured. An occupied controller will reject session 0 and
    /// its error is returned unchanged to the frontend.
    pub async fn set_limits(
        &self,
        prefix: &str,
        linear: Option<f64>,
        angular: Option<f64>,
    ) -> anyhow::Result<BaseLimitsDto> {
        if linear.is_none() && angular.is_none() {
            return Err(anyhow!("set_limits 至少需要 linear 或 angular 中的一项"));
        }
        let Some((schema, _before)) = self.fetch_limits_snapshot(prefix).await? else {
            return Err(anyhow!("unsupported:控制器不支持运行时底盘加速度限制"));
        };
        let requested_linear = validate_requested_limit("linear", linear, schema.linear)?;
        let requested_angular = validate_requested_limit("angular", angular, schema.angular)?;

        let held_session = self.ctrl.session_id.load(Ordering::Relaxed);
        let held_prefix = self.ctrl.prefix.lock().unwrap().clone();
        let session_id = limits_session_id(held_session, held_prefix.as_deref(), prefix);
        let request = pb::BaseLimits {
            accel_max: requested_linear,
            angular_accel_max: requested_angular,
            session_id,
            ..Default::default()
        };
        let rpc_key = format!("{prefix}/rpc/set_limits");
        let Some(response) = query_optional_strict::<pb::SetBaseLimitsResponse>(
            &self.session,
            &rpc_key,
            enc(&request),
        )
        .await?
        else {
            return Err(anyhow!("unsupported:控制器没有响应 rpc/set_limits"));
        };
        validate_set_limits_response(&schema, &response, requested_linear, requested_angular)
    }

    pub async fn acquire(&self, prefix: &str, model: &str) -> anyhow::Result<()> {
        // 换机取控:已持有别台 → 先释放旧会话(会话跨切换保持后,换机不再要求手动释放;
        // 一个模块同时只持一台,同 kind 多持是后续项)。
        if self.ctrl.session_id.load(Ordering::Relaxed) != 0 {
            let cur = self.ctrl.prefix.lock().unwrap().clone();
            if cur.as_deref() != Some(prefix) { self.release().await; }
        }
        // 控制器强制要求 liveliness_key(见 zenoh_lease):token 必须活到 release,
        // 提前 drop 等于一取控就自动放手。
        let lease = crate::zenoh_lease::declare(&self.session, "base").await?;
        let req = pb::AcquireSessionRequest {
            client_name: Some("hexmeow-gui".into()),
            liveliness_key: Some(lease.key.clone()),
        };
        let resp: pb::AcquireSessionResponse = query_one(&self.session, &format!("{prefix}/rpc/acquire_session"), enc(&req))
            .await.ok_or_else(|| anyhow!("acquire 无回复"))?;
        if !resp.ok {
            return Err(anyhow!("被占用:holder {} {:?}", resp.current_holder, resp.current_holder_name));
        }
        self.ctrl.session_id.store(resp.session_id, Ordering::Relaxed);
        *self.ctrl.live_lease.lock().unwrap() = Some(lease);
        *self.ctrl.prefix.lock().unwrap() = Some(prefix.to_string());
        // 取控隐含观察:确保 odom/status 读流也跟到这台(即使前端漏调 set_diag_focus)。
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
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

    /// 观察聚焦:选中某机器即观察它 —— odom/status(位姿/holder/RUNNING/故障灯)实时刷新 + 订阅其
    /// events/logs(全部与取控解耦,只读/仅选中也生效;设计:读永远开放)。清空旧缓冲、复位随机器
    /// 变的观测量,再从 `.../events/recent` + `.../log/recent` 播种一次历史(事后连上也查得到,如底盘拔轮)。
    pub async fn set_diag_focus(&self, prefix: &str) {
        *self.ctrl.view_prefix.lock().unwrap() = Some(prefix.to_string());
        // 复位随机器变的只读观测量,等新机器的 odom / status 覆盖(不残留上一台的位姿/holder)。
        {
            let mut st = self.ctrl.state.lock().unwrap();
            st.fatal = false;   // 由 status 订阅按新 prefix 重新点亮
            st.holder = 0;      // 由 status 订阅刷新
            st.running = false;
            st.robot_mode.clear(); st.overtaken_reason.clear();
            st.pose_x = 0.0; st.pose_y = 0.0; st.pose_theta = 0.0;
            st.vx = 0.0; st.vy = 0.0; st.wz = 0.0;
            // 身份(model/prefix)是**取控作用域**的量,只读时清空 —— 否则上一台受控机器的身份会贴到
            // 另一台的实时位姿上(观察对象由前端据发现列表 + 选中项标注)。取控时 acquire 重填。
            st.model.clear(); st.prefix.clear();
        }
        self.ctrl.events.lock().unwrap().clear();
        self.ctrl.logs.lock().unwrap().clear();
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
        // 先撤 token 再发 release_session:两条路径都会让控制器释放会话,
        // 先撤的那条保证即使 release RPC 发不出去(网络已断)会话也不会留下。
        self.ctrl.live_lease.lock().unwrap().take();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conway_device_description_cannot_be_discovered_as_a_second_base() {
        let nested = pb::BaseDescription {
            kinematics: "diff2".into(),
            wheel_count: 2,
            default_limits: Some(pb::BaseLimits::default()),
            ..Default::default()
        };
        let mut payload = Vec::new();
        nested.encode(&mut payload).unwrap();

        assert!(decode_base_info_reply(
            "hexmeow/controller-1/base0/base/description",
            &payload
        )
        .is_none());
    }

    #[test]
    fn robot_description_decodes_to_the_expected_base() {
        let robot = pb::RobotDescription {
            robot_index: "base0".into(),
            kind: pb::RobotKind::Base as i32,
            model: "conway_a2".into(),
            ..Default::default()
        };
        let mut payload = Vec::new();
        robot.encode(&mut payload).unwrap();

        let info =
            decode_base_info_reply("hexmeow/controller-1/base0/description", &payload)
                .expect("valid robot description");
        assert_eq!(info.prefix, "hexmeow/controller-1/base0");
        assert_eq!(info.model, "conway_a2");
    }

    fn accel_limits(linear: Option<f32>, angular: Option<f32>) -> pb::BaseLimits {
        pb::BaseLimits {
            accel_max: linear,
            angular_accel_max: angular,
            ..Default::default()
        }
    }

    fn valid_limits_description() -> pb::BaseDescription {
        pb::BaseDescription {
            default_limits: Some(accel_limits(Some(2.0), Some(6.0))),
            settable_min: Some(accel_limits(Some(0.2), Some(0.5))),
            settable_max: Some(accel_limits(Some(10.0), Some(30.0))),
            ..Default::default()
        }
    }

    fn valid_limits_schema() -> BaseLimitsSchema {
        parse_limits_schema(&valid_limits_description())
            .expect("valid description")
            .expect("supported limits")
    }

    #[test]
    fn old_base_description_is_a_cleanly_unsupported_capability() {
        let old = pb::BaseDescription {
            default_limits: Some(accel_limits(Some(2.0), None)),
            ..Default::default()
        };
        assert!(parse_limits_schema(&old).unwrap().is_none());
    }

    #[test]
    fn controller_values_build_the_complete_limits_dto() {
        let schema = valid_limits_schema();
        let dto =
            build_limits_dto("base/limits", &schema, &accel_limits(Some(1.25), Some(3.5))).unwrap();

        assert_eq!(
            dto.linear,
            Some(BaseLimitAxisDto {
                current: 1.25,
                default_value: 2.0,
                min: f64::from(0.2_f32),
                max: 10.0,
            })
        );
        assert_eq!(
            dto.angular,
            Some(BaseLimitAxisDto {
                current: 3.5,
                default_value: 6.0,
                min: 0.5,
                max: 30.0,
            })
        );
    }

    #[test]
    fn a_controller_may_advertise_only_one_settable_axis() {
        let description = pb::BaseDescription {
            default_limits: Some(accel_limits(Some(2.0), Some(6.0))),
            settable_min: Some(accel_limits(Some(0.2), None)),
            settable_max: Some(accel_limits(Some(10.0), None)),
            ..Default::default()
        };
        let schema = parse_limits_schema(&description)
            .unwrap()
            .expect("linear-only capability");
        let dto = build_limits_dto(
            "base/limits",
            &schema,
            &accel_limits(Some(1.0), None),
        )
        .unwrap();
        assert!(dto.linear.is_some());
        assert!(dto.angular.is_none());
    }

    #[test]
    fn malformed_description_ranges_are_not_downgraded_to_unsupported() {
        let mut missing_pair = valid_limits_description();
        missing_pair.settable_max = None;
        assert!(parse_limits_schema(&missing_pair)
            .unwrap_err()
            .to_string()
            .contains("必须成对出现"));

        let mut inverted = valid_limits_description();
        inverted.settable_min.as_mut().unwrap().accel_max = Some(10.0);
        inverted.settable_max.as_mut().unwrap().accel_max = Some(0.2);
        assert!(parse_limits_schema(&inverted)
            .unwrap_err()
            .to_string()
            .contains("范围非法"));

        let mut non_finite = valid_limits_description();
        non_finite.settable_max.as_mut().unwrap().angular_accel_max = Some(f32::NAN);
        assert!(parse_limits_schema(&non_finite)
            .unwrap_err()
            .to_string()
            .contains("不是有限值"));
    }

    #[test]
    fn malformed_current_limits_are_rejected() {
        let schema = valid_limits_schema();
        for current in [
            accel_limits(Some(1.0), None),
            accel_limits(Some(11.0), Some(3.0)),
            accel_limits(Some(f32::NAN), Some(3.0)),
        ] {
            assert!(build_limits_dto("base/limits", &schema, &current).is_err());
        }

        let mut carries_session = accel_limits(Some(1.0), Some(3.0));
        carries_session.session_id = 7;
        assert!(build_limits_dto("base/limits", &schema, &carries_session).is_err());
    }

    #[test]
    fn requested_values_follow_the_advertised_axis_and_range() {
        let schema = valid_limits_schema();
        assert_eq!(
            validate_requested_limit("linear", Some(1.5), schema.linear).unwrap(),
            Some(1.5)
        );
        assert!(validate_requested_limit("linear", Some(0.0), schema.linear).is_err());
        assert!(validate_requested_limit("linear", Some(f64::NAN), schema.linear).is_err());
        assert!(validate_requested_limit("angular", Some(1.0), None)
            .unwrap_err()
            .to_string()
            .contains("unsupported"));
    }

    #[test]
    fn natural_decimal_bounds_are_validated_at_wire_precision() {
        let schema = valid_limits_schema();
        assert_eq!(
            validate_requested_limit("linear", Some(0.2_f64), schema.linear).unwrap(),
            Some(0.2_f32)
        );
        assert_eq!(
            validate_requested_limit("angular", Some(0.5_f64), schema.angular).unwrap(),
            Some(0.5_f32)
        );
        assert!(validate_requested_limit("linear", Some(0.19), schema.linear).is_err());
        assert!(validate_requested_limit("angular", Some(30.01), schema.angular).is_err());
    }

    #[test]
    fn set_limits_uses_our_session_only_for_the_held_base() {
        assert_eq!(limits_session_id(0, None, "base-a"), 0);
        assert_eq!(limits_session_id(42, Some("base-a"), "base-a"), 42);
        assert_eq!(limits_session_id(42, Some("base-b"), "base-a"), 0);
        assert_eq!(limits_session_id(0, Some("base-a"), "base-a"), 0);
    }

    #[test]
    fn set_limits_response_must_be_complete_and_echo_the_applied_value() {
        let schema = valid_limits_schema();
        let ok = pb::SetBaseLimitsResponse {
            ok: true,
            error: None,
            applied: Some(accel_limits(Some(1.5), Some(4.0))),
        };
        let dto = validate_set_limits_response(&schema, &ok, Some(1.5), None).unwrap();
        assert_eq!(dto.linear.unwrap().current, 1.5);

        let missing_applied = pb::SetBaseLimitsResponse {
            ok: true,
            ..Default::default()
        };
        assert!(validate_set_limits_response(&schema, &missing_applied, Some(1.5), None).is_err());

        let wrong_applied = pb::SetBaseLimitsResponse {
            ok: true,
            error: None,
            applied: Some(accel_limits(Some(1.0), Some(4.0))),
        };
        assert!(validate_set_limits_response(&schema, &wrong_applied, Some(1.5), None).is_err());

        let rejection_without_error = pb::SetBaseLimitsResponse {
            ok: false,
            error: None,
            applied: Some(accel_limits(Some(1.0), Some(4.0))),
        };
        assert!(
            validate_set_limits_response(&schema, &rejection_without_error, Some(1.5), None)
                .is_err()
        );
    }
}
