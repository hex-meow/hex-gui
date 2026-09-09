//! Shared Device Settings zero-preset transaction for CiA402 and Meow Motor.
//! Both use the original f32 0x3001:01 + "pres" 0x3001:02 writes. Only the
//! disable command/status and actual-position read depend on the motor dialect.
//! Callers hold device_settings_operation, including across all SDO awaits.

use std::time::Duration;

use can_transport::CanBus;
use hex_motor::canopen::sdo;
use hex_motor::cia402::Cia402Manager;
use hex_motor::meow_motor::{build_disable_sdo_write, SignedQ8_24};
use hex_motor::types::MotorIdentity;

use crate::device_registry::{classify, DeviceKind};

type Result<T> = std::result::Result<T, String>;
const SDO_TIMEOUT: Option<Duration> = Some(Duration::from_millis(500));
const DISABLE_TIMEOUT: Duration = Duration::from_secs(1);

// Use the same discovery session as the Device Settings frontend. No motor
// initialization, PDO configuration, NMT transition or host heartbeat is needed.
struct PositionSession<'a> {
    manager: &'a Cia402Manager,
    identity: MotorIdentity,
    epoch: u64,
}

impl<'a> PositionSession<'a> {
    fn capture(manager: &'a Cia402Manager, nid: u8, vendor: u32, product: u32) -> Result<Self> {
        if !classify(vendor, product).supports_position_preset() {
            return Err("position operation requires an exact known motor identity".into());
        }
        let info = manager
            .list()
            .into_iter()
            .find(|info| info.node_id == nid)
            .filter(|info| info.online)
            .ok_or_else(|| format!("node 0x{nid:02X} is offline or unavailable"))?;
        let identity = info
            .identity
            .filter(|identity| identity.vendor_id == vendor && identity.product_code == product)
            .ok_or_else(|| format!("node 0x{nid:02X} identity changed or is unavailable"))?;
        Ok(Self {
            manager,
            identity,
            epoch: info.session_epoch,
        })
    }

    fn check(&self) -> Result<()> {
        let current = self
            .manager
            .list()
            .into_iter()
            .find(|info| info.node_id == self.identity.node_id);
        if current.is_some_and(|info| {
            info.online
                && info.session_epoch == self.epoch
                && info.identity.as_ref() == Some(&self.identity)
        }) {
            Ok(())
        } else {
            Err("device session or identity changed during position operation".into())
        }
    }
}

pub(crate) async fn read_meow_position(
    manager: &Cia402Manager,
    nid: u8,
    vendor: u32,
    product: u32,
) -> Result<f32> {
    let session = PositionSession::capture(manager, nid, vendor, product)?;
    let bus = manager.bus();
    read_on_bus(bus.as_ref(), &session.identity, || session.check()).await
}

pub(crate) async fn set_position_preset(
    manager: &Cia402Manager,
    nid: u8,
    vendor: u32,
    product: u32,
    pos: f32,
) -> Result<()> {
    let session = PositionSession::capture(manager, nid, vendor, product)?;
    let bus = manager.bus();
    preset_on_bus(
        bus.as_ref(),
        &session.identity,
        pos,
        || session.check(),
        DISABLE_TIMEOUT,
    )
    .await
}

async fn verify_identity(
    bus: &dyn CanBus,
    identity: &MotorIdentity,
    check: &impl Fn() -> Result<()>,
) -> Result<()> {
    for (sub, expected) in [
        (1, identity.vendor_id),
        (2, identity.product_code),
        (3, identity.revision_number),
        (4, identity.serial_number),
    ] {
        check()?;
        let actual = sdo::upload_u32(bus, identity.node_id, 0x1018, sub, SDO_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?;
        check()?;
        if actual != expected {
            return Err(format!("device identity changed at 0x1018:{sub:02X}: expected 0x{expected:08X}, got 0x{actual:08X}"));
        }
    }
    Ok(())
}

async fn read_on_bus(
    bus: &dyn CanBus,
    identity: &MotorIdentity,
    check: impl Fn() -> Result<()>,
) -> Result<f32> {
    verify_identity(bus, identity, &check).await?;
    check()?;
    let raw = sdo::upload_u32(bus, identity.node_id, 0x4564, 0, SDO_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    check()?;
    Ok(SignedQ8_24::from_raw(raw as i32).to_revolutions() as f32)
}

async fn preset_on_bus(
    bus: &dyn CanBus,
    identity: &MotorIdentity,
    pos: f32,
    check: impl Fn() -> Result<()>,
    disable_timeout: Duration,
) -> Result<()> {
    if !pos.is_finite() {
        return Err("position preset must be finite".into());
    }
    let pos = pos.clamp(-0.5, 0.5);
    let nid = identity.node_id;
    verify_identity(bus, identity, &check).await?;
    let kind = classify(identity.vendor_id, identity.product_code);
    let disable = match kind {
        DeviceKind::Cia402Motor => hex_motor::canopen::tpdo_config::SdoWrite::u16(0x6040, 0, 0),
        DeviceKind::MeowMotor => build_disable_sdo_write(),
        _ => return Err("position preset is unavailable for this identity".into()),
    };
    check()?;
    sdo::download(
        bus,
        nid,
        disable.index,
        disable.subindex,
        &disable.data,
        SDO_TIMEOUT,
    )
    .await
    .map_err(|e| e.to_string())?;
    check()?;

    let deadline = tokio::time::Instant::now() + disable_timeout;
    loop {
        check()?;
        let disabled = match kind {
            DeviceKind::Cia402Motor => {
                let status = sdo::upload_u16(bus, nid, 0x6041, 0, SDO_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())?;
                check()?;
                // Original CiA402 Switch-On-Disabled mask, including 0x0250/0x0270.
                status & 0x004F == 0x0040
            }
            DeviceKind::MeowMotor => {
                let display = sdo::upload_u8(bus, nid, 0x4402, 0, SDO_TIMEOUT)
                    .await
                    .map_err(|e| e.to_string())?;
                check()?;
                if matches!(display, 0xA1..=0xA6 | 0xAF) {
                    return Err(format!(
                        "motor reports error mode 0x{display:02X}; position preset was not written"
                    ));
                }
                display == 0
            }
            _ => unreachable!(),
        };
        if disabled {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for Disabled before position preset".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The original CiA402 preset writes, shared by both motor types.
    for (sub, bytes) in [(1, pos.to_le_bytes()), (2, *b"pres")] {
        check()?;
        sdo::download(bus, nid, 0x3001, sub, &bytes, SDO_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?;
        check()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
