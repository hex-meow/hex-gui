//! Controller Config(Zenoh):连 hex-controller 的 launcher,读写 `<cid>/launch.yaml`。
//! 契约见 07-controller-config.md v0.3(validate-reject + 原子写 + 恢复模式)。
//! 键是**控制器级**(cid 级),不是机器人级:
//!   - `<cid>/config`(queryable)          → [`pb::ConfigGet`](含 recovery_mode)
//!   - `<cid>/rpc/config/validate`          → [`pb::ConfigValidateResponse`]
//!   - `<cid>/rpc/config/set`               → [`pb::ConfigSetResponse`]
//!   - `<cid>/rpc/restart`                  → [`pb::RestartResponse`]
//! 发现走 `<cid>/info`([`pb::ControllerInfo`]),**不复用** robot 级 `.../description` ——
//! 恢复模式下 launcher 零 robot、无 description,但 `<cid>/info` + config RPC 照常服务。
//! 结构骨架照 [`crate::zenoh_base`],但无常驻流/订阅:config 面板只需一个 Session +
//! 请求-回复。所有 RPC 用 `query_one`;发现汇聚 `hexmeow/**/info` 的全部回复。

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

fn kind_name(kind: i32) -> &'static str {
    match kind {
        1 => "arm",
        2 => "base",
        3 => "lift",
        4 => "ee",
        _ => "unknown",
    }
}

/// ApiVersion 摊平给前端(前端据 `major` 判可否编辑,沿用 01 的版本规则)。
#[derive(Serialize, Clone)]
pub struct ApiVersionDto {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl From<pb::ApiVersion> for ApiVersionDto {
    fn from(v: pb::ApiVersion) -> Self {
        Self {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
        }
    }
}

/// 控制器挂载的一个机器人(供 apply 确认框列"受影响机器人")。
#[derive(Serialize, Clone)]
pub struct RobotRefDto {
    pub robot_index: String,
    pub kind: i32,
    pub kind_name: String,
    pub model: String,
}

/// 一个发现到的控制器(`<cid>/info`)。`cid` = 键前缀 `hexmeow/<controller_id>`,
/// 前端拿它回调全部 config 命令。
#[derive(Serialize, Clone)]
pub struct ControllerInfoDto {
    pub cid: String,
    pub controller_id: String,
    pub fw_version: String,
    pub api_version: Option<ApiVersionDto>,
    pub features: Vec<String>,
    pub robots: Vec<RobotRefDto>,
}

impl ControllerInfoDto {
    fn from_pb(cid: String, info: pb::ControllerInfo) -> Self {
        Self {
            cid,
            controller_id: info.controller_id,
            fw_version: info.fw_version,
            api_version: info.api_version.map(Into::into),
            features: info.features,
            robots: info
                .robots
                .into_iter()
                .map(|r| RobotRefDto {
                    kind_name: kind_name(r.kind).to_string(),
                    robot_index: r.robot_index,
                    kind: r.kind,
                    model: r.model,
                })
                .collect(),
        }
    }
}

/// `<cid>/config` 读回:文件原文 + 指纹 + 路径 + recovery_mode。
#[derive(Serialize, Clone)]
pub struct ConfigGetDto {
    pub yaml: String,
    pub sha256: String,
    pub path: String,
    pub mtime_unix: i64,
    pub schema_version: Option<ApiVersionDto>,
    pub recovery_mode: bool,
}

impl From<pb::ConfigGet> for ConfigGetDto {
    fn from(g: pb::ConfigGet) -> Self {
        Self {
            yaml: g.yaml,
            sha256: g.sha256,
            path: g.path,
            mtime_unix: g.mtime_unix,
            schema_version: g.schema_version.map(Into::into),
            recovery_mode: g.recovery_mode,
        }
    }
}

/// 语义红线一条(mock 翻转 / 换 CAN / 换 kind / 标定 env):GUI 红色 diff + 复述确认。
#[derive(Serialize, Clone)]
pub struct CriticalChangeDto {
    pub robot_id: String,
    pub field: String,
    pub old: String,
    pub new: String,
}

impl From<pb::CriticalChange> for CriticalChangeDto {
    fn from(c: pb::CriticalChange) -> Self {
        Self {
            robot_id: c.robot_id,
            field: c.field,
            old: c.old,
            new: c.new,
        }
    }
}

/// `rpc/config/validate` 结果。
#[derive(Serialize, Clone)]
pub struct ConfigValidateResult {
    pub ok: bool,
    pub errors: Vec<String>,
    pub critical_changes: Vec<CriticalChangeDto>,
}

/// `rpc/config/set` 结果。`ok=false` 时 errors 内联展示;sha 不符属 409 类,前端另做检测。
#[derive(Serialize, Clone)]
pub struct ConfigSetResult {
    pub ok: bool,
    pub errors: Vec<String>,
    pub critical_changes: Vec<CriticalChangeDto>,
    pub sha256: String,
    pub applied: bool,
    pub robots: Vec<String>,
}

/// `rpc/restart` 结果。
#[derive(Serialize, Clone)]
pub struct RestartResult {
    pub ok: bool,
    pub robots: Vec<String>,
}

/// 一条到控制器网络的连接(仅 Session,无常驻任务)。
pub struct ZenohConfigConn {
    session: zenoh::Session,
}

impl ZenohConfigConn {
    pub async fn open(connect: &str) -> anyhow::Result<Self> {
        let mut cfg = zenoh::Config::default();
        cfg.insert_json5("mode", "\"peer\"").unwrap();
        if !connect.is_empty() {
            cfg.insert_json5("connect/endpoints", &format!("[\"{connect}\"]"))
                .unwrap();
        }
        let session = zenoh::open(cfg)
            .await
            .map_err(|e| anyhow!("zenoh open: {e}"))?;
        // 给组播探测/建链一点时间,之后 discover 才能收到 <cid>/info 的回复。
        tokio::time::sleep(Duration::from_millis(700)).await;
        Ok(Self { session })
    }

    /// 发现网络里的控制器:query `hexmeow/**/info` 收全部回复 → 解码 ControllerInfo → 剥 `/info` 得 cid。
    /// 恢复模式下 launcher 零 robot,但 `<cid>/info` 仍在,故此路径永远可发现。
    pub async fn discover(&self) -> Vec<ControllerInfoDto> {
        let mut out = Vec::new();
        if let Ok(replies) = self.session.get("hexmeow/**/info").await {
            while let Ok(reply) = replies.recv_async().await {
                if let Ok(sample) = reply.result() {
                    if let Ok(info) = pb::ControllerInfo::decode(&*sample.payload().to_bytes()) {
                        let key = sample.key_expr().as_str();
                        let cid = key.strip_suffix("/info").unwrap_or(key).to_string();
                        out.push(ControllerInfoDto::from_pb(cid, info));
                    }
                }
            }
        }
        out
    }

    /// 只读拉配置(空 payload,同 arm/description 拉法)。
    pub async fn get(&self, cid: &str) -> anyhow::Result<ConfigGetDto> {
        let g: pb::ConfigGet = query_one(&self.session, &format!("{cid}/config"), vec![])
            .await
            .ok_or_else(|| anyhow!("config get 无回复(控制器在吗?)"))?;
        Ok(g.into())
    }

    /// 干跑校验:语法 + 结构 + 语义红线 diff。不落盘。
    pub async fn validate(&self, cid: &str, yaml: &str) -> anyhow::Result<ConfigValidateResult> {
        let req = pb::ConfigValidateRequest {
            yaml: yaml.to_string(),
        };
        let r: pb::ConfigValidateResponse = query_one(
            &self.session,
            &format!("{cid}/rpc/config/validate"),
            enc(&req),
        )
        .await
        .ok_or_else(|| anyhow!("validate 无回复"))?;
        Ok(ConfigValidateResult {
            ok: r.ok,
            errors: r.errors,
            critical_changes: r.critical_changes.into_iter().map(Into::into).collect(),
        })
    }

    /// 写入(乐观锁 expect_sha256;apply=true 写盘后立即重启子进程生效;
    /// confirm 在有红线时必须为 true;force 越过会话占用检查/相对路径拒绝)。
    pub async fn set(
        &self,
        cid: &str,
        yaml: &str,
        expect_sha256: &str,
        apply: bool,
        confirm: bool,
        force: bool,
    ) -> anyhow::Result<ConfigSetResult> {
        let req = pb::ConfigSetRequest {
            yaml: yaml.to_string(),
            expect_sha256: expect_sha256.to_string(),
            apply,
            confirm,
            force,
        };
        let r: pb::ConfigSetResponse =
            query_one(&self.session, &format!("{cid}/rpc/config/set"), enc(&req))
                .await
                .ok_or_else(|| anyhow!("set 无回复"))?;
        Ok(ConfigSetResult {
            ok: r.ok,
            errors: r.errors,
            critical_changes: r.critical_changes.into_iter().map(Into::into).collect(),
            sha256: r.sha256,
            applied: r.applied,
            robots: r.robots,
        })
    }

    /// 单独"应用"当前已保存的配置(重启全部子进程)。confirm 复述后为 true;force 越过会话检查。
    pub async fn restart(
        &self,
        cid: &str,
        confirm: bool,
        force: bool,
    ) -> anyhow::Result<RestartResult> {
        let req = pb::RestartRequest { confirm, force };
        let r: pb::RestartResponse =
            query_one(&self.session, &format!("{cid}/rpc/restart"), enc(&req))
                .await
                .ok_or_else(|| anyhow!("restart 无回复"))?;
        Ok(RestartResult {
            ok: r.ok,
            robots: r.robots,
        })
    }
}
