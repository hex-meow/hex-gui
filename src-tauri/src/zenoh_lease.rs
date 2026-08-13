//! 会话租约:`acquire_session` 必需的 liveliness token。
//!
//! **控制器侧 2026-08-11 起强制**:不带 `liveliness_key` 的 `acquire_session` 一律被拒,
//! 错误是 `liveliness_key is required: ...`。原因是命令看门狗只在**有命令流时**才计时 ——
//! 机械臂的 `GRAVITY_COMP`/`PASSIVE`、EE 抓着东西不动、lift 跑自主 goal 都不计时。
//! 没有 token,GUI 一崩(或被 kill、断网)会话就永久占死,直到公共 Zenoh 断开为止;
//! 机器则停在无主态,别的客户端再也取不到控。
//!
//! 用法:`acquire` 之前 [`declare`],把 key 放进请求;成功后**把 token 一直拿住**,
//! `release` 时再 drop。token 一 drop,Zenoh 立刻广播 Delete,控制器随即释放会话 ——
//! 所以它绝不能在 `acquire` 返回后就被丢掉,那等于一取控就自动放手。

use anyhow::anyhow;
use zenoh::liveliness::LivelinessToken;
use zenoh::Session;

/// 一次取控的 liveliness 租约。drop 即通知控制器"客户端没了"。
pub(crate) struct SessionLease {
    pub key: String,
    _token: LivelinessToken,
}

/// 为某个模块声明一个进程内唯一的 liveliness token。
///
/// key 里带模块名与 pid:同一个 GUI 进程可以同时持臂、底盘、EE、lift 四个会话,
/// 它们必须是**各自独立**的 token —— 共用一个的话,释放其中一台会把其余几台一起带走。
pub(crate) async fn declare(session: &Session, module: &str) -> anyhow::Result<SessionLease> {
    let key = format!(
        "hexmeow/_clients/hexmeow-gui/{}/{module}",
        std::process::id()
    );
    let token = session
        .liveliness()
        .declare_token(&key)
        .await
        .map_err(|e| anyhow!("声明 liveliness token {key} 失败:{e}"))?;
    Ok(SessionLease { key, _token: token })
}
