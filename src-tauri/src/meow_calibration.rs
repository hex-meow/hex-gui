//! Motor Control App 使用的出厂力矩系数（`0x4001` v1）。
//!
//! 这里只回答一个问题：**这台电机的 `torque_factor` 是多少**。它与 Product
//! Authenticity APP 的职责严格分开 —— 那边回答"来源是否可信"并且会联网；这边只做
//! 本地 CRC 自洽性检查，绝不验证签发 token，也绝不联网。解码本身由
//! `hex_motor::meow_motor::calibration` 完成。
//!
//! 应用方向按出厂标定的定义固定：
//!
//! ```text
//! raw_command       = desired_physical_torque * torque_factor
//! physical_feedback = raw_feedback / torque_factor
//! ```
//!
//! 摩擦力标定虽然一起读到了，但 Motor Control App 不做摩擦前馈 —— 那需要方向和
//! 速度状态机，属于控制策略而不是单位换算。这里只把"有没有摩擦标定"报告给界面。
//!
//! 缓存以 `(node_id, heartbeat session epoch)` 为键。电机掉线重上线会换 epoch，
//! 因此换上另一台电机不会沿用上一台的系数。

use std::collections::HashMap;
use std::sync::Arc;

use hex_motor::meow_motor::{MeowFactoryCalibration, MeowMotorManager};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::state::AppState;

/// 没有出厂标定时使用的中性系数：命令与显示都保持原样，也就是今天的行为。
pub(crate) const NEUTRAL_TORQUE_FACTOR: f64 = 1.0;

/// 出厂标定在界面上的最小视图。
///
/// 注意这里**不包含签发 token**：Motor Control App 不需要它，也不该把它塞进一个
/// 50 Hz 轮询的快照里。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MeowTorqueFactorView {
    /// 读到完整、CRC 自洽的 v1，`factor` 正在被应用。
    Calibrated {
        factor: f64,
        fit_rmse_nm: f64,
        /// `0x4001:05..07` 是否有摩擦标定。本 APP 只显示，不做摩擦前馈。
        friction_calibrated: bool,
    },
    /// 读成功了，但这台电机没有有效出厂标定。命令和显示都按 1.0 处理。
    Uncalibrated { detail: String },
    /// 读取本身失败（SDO 超时、掉线、身份会话变了……）。
    ///
    /// 这**不等于**"未标定"：系数未知时发力矩命令是静默的精度错误，所以命令路径
    /// 在这个状态下会直接报错，而不是退回 1.0。
    Unavailable { detail: String },
}

impl MeowTorqueFactorView {
    /// 应用到命令/显示上的系数。只有明确读到标定才不是 1.0。
    pub(crate) fn applied_factor(&self) -> Option<f64> {
        match self {
            Self::Calibrated { factor, .. } => Some(*factor),
            Self::Uncalibrated { .. } => Some(NEUTRAL_TORQUE_FACTOR),
            Self::Unavailable { .. } => None,
        }
    }

    fn is_cacheable(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }
}

#[derive(Default)]
pub struct MeowCalibrationState {
    cache: Mutex<HashMap<(u8, u64), MeowTorqueFactorView>>,
}

impl MeowCalibrationState {
    pub async fn clear(&self) {
        self.cache.lock().await.clear();
    }

    /// 单台电机的缓存作废。用户重标写完 `0x4001` 后必须调用，否则控制 APP 会继续
    /// 用旧系数换算。
    pub async fn forget_node(&self, node_id: u8) {
        self.cache
            .lock()
            .await
            .retain(|(node, _), _| *node != node_id);
    }

    async fn cached(&self, node_id: u8, session_epoch: u64) -> Option<MeowTorqueFactorView> {
        self.cache
            .lock()
            .await
            .get(&(node_id, session_epoch))
            .cloned()
    }

    async fn store(&self, node_id: u8, session_epoch: u64, view: MeowTorqueFactorView) {
        if !view.is_cacheable() {
            return;
        }
        let mut cache = self.cache.lock().await;
        // 同一节点只保留当前 heartbeat session。
        cache.retain(|(node, _), _| *node != node_id);
        cache.insert((node_id, session_epoch), view);
    }
}

/// 该节点当前 heartbeat session 的 epoch。节点离线或不是已知 Meow Motor 时报错。
fn session_epoch(manager: &MeowMotorManager, node_id: u8) -> Result<u64, String> {
    let info = manager
        .list()
        .into_iter()
        .find(|info| info.node_id == node_id)
        .ok_or_else(|| format!("node 0x{node_id:02X} has not appeared on the bus"))?;
    if !info.online {
        return Err(format!("node 0x{node_id:02X} is offline"));
    }
    Ok(info.session_epoch)
}

/// 从设备重新读取并写入缓存。identify / initialize / 界面显式刷新都走这里。
pub(crate) async fn refresh(
    state: &AppState,
    manager: &Arc<MeowMotorManager>,
    node_id: u8,
) -> MeowTorqueFactorView {
    let epoch = match session_epoch(manager, node_id) {
        Ok(epoch) => epoch,
        Err(detail) => return MeowTorqueFactorView::Unavailable { detail },
    };
    let view = match manager.read_factory_calibration(node_id).await {
        Ok(MeowFactoryCalibration::Valid(v1)) => {
            let factor = v1.torque.factor;
            if !factor.is_finite() || factor <= 0.0 {
                MeowTorqueFactorView::Uncalibrated {
                    detail: format!("0x4001 v1 torque factor {factor} is not usable"),
                }
            } else {
                MeowTorqueFactorView::Calibrated {
                    factor,
                    fit_rmse_nm: v1.torque.fit_rmse_nm,
                    friction_calibrated: v1.friction.is_some(),
                }
            }
        }
        Ok(MeowFactoryCalibration::Missing { reason, .. }) => MeowTorqueFactorView::Uncalibrated {
            detail: reason.to_string(),
        },
        Err(error) => MeowTorqueFactorView::Unavailable {
            detail: error.to_string(),
        },
    };
    state
        .meow_calibration
        .store(node_id, epoch, view.clone())
        .await;
    view
}

/// 快照轮询使用：只查缓存，永远不做 SDO I/O。
///
/// UI 以最高 50 Hz 轮询快照，这里绝不能触发总线读取。`session_epoch` 由调用方从
/// 已经取到的 `MeowMotorInfo` 传入，避免为了同一份数据再遍历一次 manager。
pub(crate) async fn cached_view(
    state: &AppState,
    node_id: u8,
    session_epoch: u64,
) -> Option<MeowTorqueFactorView> {
    state.meow_calibration.cached(node_id, session_epoch).await
}

/// 命令路径使用：命中当前 session 的缓存就直接用，否则读一次。
///
/// 读取失败返回 `Err` 而不是退回 1.0 —— 用未知系数发力矩命令是静默的精度错误。
/// 读取成功但这台电机没有标定，则返回 [`NEUTRAL_TORQUE_FACTOR`]，控制照常可用。
pub(crate) async fn factor_for(
    state: &AppState,
    manager: &Arc<MeowMotorManager>,
    node_id: u8,
) -> Result<f64, String> {
    let epoch = session_epoch(manager, node_id)?;
    if let Some(view) = state.meow_calibration.cached(node_id, epoch).await {
        if let Some(factor) = view.applied_factor() {
            return Ok(factor);
        }
    }
    refresh(state, manager, node_id)
        .await
        .applied_factor()
        .ok_or_else(|| {
            format!(
                "could not read 0x4001 on node 0x{node_id:02X}, so the factory torque factor is \
                 unknown; retry before commanding torque"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_successful_read_produces_an_applied_factor() {
        let calibrated = MeowTorqueFactorView::Calibrated {
            factor: 1.12,
            fit_rmse_nm: 0.03,
            friction_calibrated: true,
        };
        assert_eq!(calibrated.applied_factor(), Some(1.12));

        let uncalibrated = MeowTorqueFactorView::Uncalibrated {
            detail: "0x4001 does not exist on this device".into(),
        };
        assert_eq!(
            uncalibrated.applied_factor(),
            Some(NEUTRAL_TORQUE_FACTOR),
            "an uncalibrated motor must keep working exactly as it does today"
        );

        let unavailable = MeowTorqueFactorView::Unavailable {
            detail: "SDO timeout".into(),
        };
        assert_eq!(
            unavailable.applied_factor(),
            None,
            "an unknown factor must not silently degrade to 1.0"
        );
    }

    #[tokio::test]
    async fn a_failed_read_is_never_cached_and_a_new_session_replaces_the_old_entry() {
        let state = MeowCalibrationState::default();
        state
            .store(
                7,
                1,
                MeowTorqueFactorView::Unavailable {
                    detail: "SDO timeout".into(),
                },
            )
            .await;
        assert!(state.cached(7, 1).await.is_none());

        let first = MeowTorqueFactorView::Calibrated {
            factor: 1.12,
            fit_rmse_nm: 0.03,
            friction_calibrated: false,
        };
        state.store(7, 1, first).await;
        assert!(state.cached(7, 1).await.is_some());

        // 同一 node 重新上线：epoch 变了，旧条目必须消失而不是被当成当前值。
        state
            .store(
                7,
                2,
                MeowTorqueFactorView::Uncalibrated {
                    detail: "no v1".into(),
                },
            )
            .await;
        assert!(state.cached(7, 1).await.is_none());
        assert!(state.cached(7, 2).await.is_some());

        state.forget_node(7).await;
        assert!(state.cached(7, 2).await.is_none());
    }
}
