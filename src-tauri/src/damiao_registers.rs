//! DM-J4310-2EC V1.2 manual V1.4 (2026-09-14), pp. 17–19, 26.
//! This catalog is also sent to the UI; access and bounds are enforced here.
use anyhow::{anyhow, bail, Result};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum RegisterType {
    Float32,
    Uint32,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct DamiaoRegisterDefinition {
    pub address: u8,
    pub name: &'static str,
    pub label_zh: &'static str,
    pub label_en: &'static str,
    pub group: &'static str,
    pub data_type: RegisterType,
    pub writable: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub min_inclusive: bool,
    pub max_inclusive: bool,
    pub range: &'static str,
    pub unit: &'static str,
    pub note_zh: &'static str,
    pub note_en: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct DamiaoRegisterReading {
    pub address: u8,
    /// None preserves an unrepresentable float as raw bits, instead of NaN JSON.
    pub value: Option<f64>,
    pub raw: u32,
}

impl DamiaoRegisterDefinition {
    pub fn encode(&self, value: f64) -> Result<u32> {
        if !self.writable {
            bail!(
                "{} (0x{:02X}) is read-only according to the motor manual",
                self.name,
                self.address
            );
        }
        if !value.is_finite() {
            bail!("{} must be finite", self.name);
        }
        let (encoded, rounded) = match self.data_type {
            RegisterType::Uint32 => {
                if value.fract() != 0.0 || !(0.0..=u32::MAX as f64).contains(&value) {
                    bail!("{} requires a uint32 integer", self.name);
                }
                (value as u32, value)
            }
            RegisterType::Float32 => {
                if value.abs() > f32::MAX as f64 {
                    bail!("{} exceeds finite float32 range", self.name);
                }
                let rounded = value as f32;
                (rounded.to_bits(), rounded as f64)
            }
        };
        // Check both the entered value and the on-wire float. Rounding a value
        // just above an open bound back onto that bound must not bypass it.
        for candidate in [value, rounded] {
            if self
                .min
                .is_some_and(|min| candidate < min || (!self.min_inclusive && candidate == min))
                || self
                    .max
                    .is_some_and(|max| candidate > max || (!self.max_inclusive && candidate == max))
            {
                bail!(
                    "{} must be in {} (after float32 encoding)",
                    self.name,
                    self.range
                );
            }
        }
        Ok(encoded)
    }

    pub fn decode(&self, bytes: [u8; 4]) -> DamiaoRegisterReading {
        let raw = u32::from_le_bytes(bytes);
        let value = match self.data_type {
            RegisterType::Uint32 => Some(raw as f64),
            RegisterType::Float32 => {
                let value = f32::from_bits(raw);
                value.is_finite().then_some(value as f64)
            }
        };
        DamiaoRegisterReading {
            address: self.address,
            value,
            raw,
        }
    }
}

const FMAX: f64 = f32::MAX as f64;
macro_rules! rw {
    ($addr:expr, $name:literal, $zh:literal, $en:literal, $group:literal, $ty:ident,
     $min:expr, $max:expr, $li:expr, $hi:expr, $range:literal, $unit:literal, $nz:literal, $ne:literal) => {
        DamiaoRegisterDefinition {
            address: $addr,
            name: $name,
            label_zh: $zh,
            label_en: $en,
            group: $group,
            data_type: RegisterType::$ty,
            writable: true,
            min: Some($min),
            max: Some($max),
            min_inclusive: $li,
            max_inclusive: $hi,
            range: $range,
            unit: $unit,
            note_zh: $nz,
            note_en: $ne,
        }
    };
}
macro_rules! ro {
    ($addr:expr, $name:literal, $zh:literal, $en:literal, $group:literal, $ty:ident, $unit:literal, $nz:literal, $ne:literal) => {
        DamiaoRegisterDefinition {
            address: $addr,
            name: $name,
            label_zh: $zh,
            label_en: $en,
            group: $group,
            data_type: RegisterType::$ty,
            writable: false,
            min: None,
            max: None,
            min_inclusive: false,
            max_inclusive: false,
            range: "—",
            unit: $unit,
            note_zh: $nz,
            note_en: $ne,
        }
    };
}

pub static REGISTERS: &[DamiaoRegisterDefinition] = &[
    rw!(0x00, "UV_Value", "低压保护值", "Undervoltage protection", "protection", Float32, 10.0, FMAX, false, true, "(10, fmax]", "V", "", ""),
    rw!(0x01, "KT_Value", "扭矩系数", "Torque coefficient", "motor", Float32, 0.0, FMAX, true, true, "[0, fmax]", "", "", ""),
    rw!(0x02, "OT_Value", "过温保护值", "Overtemperature protection", "protection", Float32, 80.0, 200.0, true, false, "[80, 200)", "°C", "手册建议不超过 100°C。", "The manual recommends no more than 100°C."),
    rw!(0x03, "OC_Value", "过流保护值", "Overcurrent protection", "protection", Float32, 0.0, 1.0, false, false, "(0, 1)", "p.u.", "最大相电流的比例，0.5 表示 50%。", "Fraction of maximum phase current; 0.5 means 50%."),
    rw!(0x04, "ACC", "加速度", "Acceleration", "motion", Float32, 0.0, FMAX, false, false, "(0, fmax)", "krad/s²", "转子侧，非 MIT 模式使用。", "Rotor side; used outside MIT mode."),
    rw!(0x05, "DEC", "减速度", "Deceleration", "motion", Float32, -FMAX, 0.0, true, false, "[-fmax, 0)", "krad/s²", "转子侧，必须为负数。", "Rotor side; must be negative."),
    rw!(0x06, "MAX_SPD", "最大速度", "Maximum speed", "motion", Float32, 0.0, FMAX, false, true, "(0, fmax]", "rad/s", "转子侧（减速前），不是输出轴速度。", "Rotor side, before the gearbox."),
    rw!(0x07, "MST_ID", "反馈 ID", "Feedback ID", "communication", Uint32, 0.0, 2047.0, true, true, "[0, 0x7FF]", "", "修改后校验新反馈地址。0x7FF 与请求共用帧 ID，无法区分保存应答和回显；保存到 Flash 前需改为其他值。", "New feedback address is verified. At 0x7FF, save replies cannot be distinguished from request echoes; select another ID before saving Flash."),
    rw!(0x08, "ESC_ID", "接收 ID", "Motor ID", "communication", Uint32, 0.0, 2047.0, true, true, "[0, 0x7FF]", "", "手册建议小于 16；修改后校验新地址，自动切换连接。", "IDs below 16 are recommended; the new address is verified and the session follows it."),
    rw!(0x09, "TIMEOUT", "超时警报时间", "CAN timeout", "communication", Uint32, 0.0, 4294967295.0, true, true, "[0, 4294967295]", "×50 µs", "每个计数周期 50 µs，仅使能时生效。", "50 µs per count; active only while enabled."),
    rw!(0x0A, "CTRL_MODE", "控制模式", "Control mode", "motion", Uint32, 0.0, 4.0, true, true, "[0, 4]", "", "1 MIT；2 位置速度；3 速度；4 力位混控。0 的行为未定义，写入后禁止运动直到选择有效模式。", "1 MIT; 2 position/velocity; 3 velocity; 4 force/position. Mode 0 is undefined; select a defined mode before enabling."),
    ro!(0x0B, "Damp", "电机粘滞系数", "Motor damping", "motor", Float32, "", "", ""),
    ro!(0x0C, "Inertia", "电机转动惯量", "Motor inertia", "motor", Float32, "", "", ""),
    ro!(0x0D, "hw_ver", "保留（hw_ver）", "Reserved (hw_ver)", "version", Uint32, "", "手册标注保留，仅显示原值。", "Reserved in the manual; displayed without interpretation."),
    ro!(0x0E, "sw_ver", "软件版本号", "Software version", "version", Uint32, "", "", ""),
    ro!(0x0F, "SN", "保留（SN）", "Reserved (SN)", "version", Uint32, "", "手册标注保留，仅显示原值。", "Reserved in the manual; displayed without interpretation."),
    ro!(0x10, "NPP", "电机极对数", "Pole pairs", "motor", Uint32, "", "自动辨识参数，只读。", "Identified by the drive; read-only."),
    ro!(0x11, "Rs", "电机相电阻", "Phase resistance", "motor", Float32, "", "", ""),
    ro!(0x12, "Ls", "电机相电感", "Phase inductance", "motor", Float32, "", "", ""),
    ro!(0x13, "Flux", "电机磁链值", "Flux linkage", "motor", Float32, "", "", ""),
    ro!(0x14, "Gr", "齿轮减速比", "Gear ratio", "motor", Float32, "", "自动辨识参数，只读。", "Identified by the drive; read-only."),
    rw!(0x15, "PMAX", "位置映射范围", "Position mapping", "mapping", Float32, 0.0, FMAX, false, true, "(0, fmax]", "rad", "写入后同步命令编码和反馈解码。", "Command encoding and feedback decoding are synchronized after writing."),
    rw!(0x16, "VMAX", "速度映射范围", "Velocity mapping", "mapping", Float32, 0.0, FMAX, false, true, "(0, fmax]", "rad/s", "协议映射幅值，不代表机械限速。", "Protocol scale, not a mechanical speed limit."),
    rw!(0x17, "TMAX", "扭矩映射范围", "Torque mapping", "mapping", Float32, 0.0, FMAX, false, true, "(0, fmax]", "Nm", "协议映射幅值，不代表额定扭矩。", "Protocol scale, not rated torque."),
    rw!(0x18, "I_BW", "电流环控制带宽", "Current loop bandwidth", "tuning", Float32, 100.0, 10000.0, true, true, "[100, 10000]", "", "手册默认值 1000。", "Manual default: 1000."),
    rw!(0x19, "KP_ASR", "速度环 Kp", "Velocity loop Kp", "tuning", Float32, 0.0, FMAX, true, true, "[0, fmax]", "", "", ""),
    rw!(0x1A, "KI_ASR", "速度环 Ki", "Velocity loop Ki", "tuning", Float32, 0.0, FMAX, true, true, "[0, fmax]", "", "", ""),
    rw!(0x1B, "KP_APR", "位置环 Kp", "Position loop Kp", "tuning", Float32, 0.0, FMAX, true, true, "[0, fmax]", "", "", ""),
    rw!(0x1C, "KI_APR", "位置环 Ki", "Position loop Ki", "tuning", Float32, 0.0, FMAX, true, true, "[0, fmax]", "", "", ""),
    rw!(0x1D, "OV_Value", "过压保护值", "Overvoltage protection", "protection", Float32, -FMAX, FMAX, true, true, "TBD", "V", "手册未规定范围（TBD），仅校验有限 float32。", "Range is TBD in the manual; only finite float32 is enforced."),
    rw!(0x1E, "GREF", "齿轮力矩效率", "Gear torque efficiency", "motor", Float32, 0.0, 1.0, false, true, "(0, 1]", "", "", ""),
    rw!(0x1F, "Deta", "速度环阻尼系数", "Velocity loop damping", "tuning", Float32, 1.0, 30.0, true, true, "[1, 30]", "", "", ""),
    rw!(0x20, "V_BW", "速度环滤波带宽", "Velocity filter bandwidth", "tuning", Float32, 0.0, 500.0, false, false, "(0, 500)", "Hz", "", ""),
    rw!(0x21, "IQ_c1", "电流环增强系数", "Current loop enhancement", "tuning", Float32, 100.0, 10000.0, true, true, "[100, 10000]", "", "手册不建议修改。", "The manual advises against changing this value."),
    rw!(0x22, "VL_c1", "速度环增强系数", "Velocity loop enhancement", "tuning", Float32, 0.0, 10000.0, false, true, "(0, 10000]", "", "手册建议调节范围 200–800。", "Recommended tuning range: 200–800."),
    rw!(0x23, "can_br", "CAN 波特率代码", "CAN bitrate code", "communication", Uint32, 0.0, 9.0, true, true, "[0, 9]", "", "写入后按新速率重新连接。大于 1 Mbps 使用 CAN FD。", "Reconnect at the new rate after writing. Rates above 1 Mbps use CAN FD."),
    ro!(0x24, "sub_ver", "子版本号", "Subversion", "version", Uint32, "", "", ""),
    ro!(0x25, "Boot_ver", "Boot 版本号", "Boot version", "version", Uint32, "", "", ""),
    ro!(0x37, "dir", "方向", "Direction", "motor", Float32, "", "手册类型为 float，只读。", "Documented as a read-only float."),
    ro!(0x38, "m_off", "电机侧角度偏移", "Motor angle offset", "motor", Float32, "", "", ""),
    ro!(0x3B, "Imax", "驱动板最大电流", "Maximum drive current", "telemetry", Float32, "A", "", ""),
    ro!(0x3C, "VBus", "电源电压", "Bus voltage", "telemetry", Float32, "V", "", ""),
    ro!(0x3D, "Tpcb", "驱动温度", "Driver temperature", "telemetry", Float32, "°C", "", ""),
    ro!(0x3E, "Tmtr", "电机线圈温度", "Winding temperature", "telemetry", Float32, "°C", "", ""),
    ro!(0x3F, "I_U_OFF", "U 相电流偏置", "U phase current offset", "telemetry", Float32, "", "", ""),
    ro!(0x40, "I_V_OFF", "V 相电流偏置", "V phase current offset", "telemetry", Float32, "", "", ""),
    ro!(0x41, "I_W_OFF", "W 相电流偏置", "W phase current offset", "telemetry", Float32, "", "", ""),
    ro!(0x50, "p_m", "电机当前位置", "Control position", "telemetry", Float32, "rad", "转子折算到输出轴的多圈控制位置。", "Multi-turn rotor position referred to the output shaft."),
    ro!(0x51, "xout", "输出轴位置", "Output encoder position", "telemetry", Float32, "rad", "输出轴编码器测量位置。", "Measured by the output encoder."),
];

pub fn register_definition(address: u8) -> Result<&'static DamiaoRegisterDefinition> {
    REGISTERS
        .iter()
        .find(|entry| entry.address == address)
        .ok_or_else(|| anyhow!("Register 0x{address:02X} is not listed in the DM-J4310-2EC manual"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_documented_addresses_and_keeps_telemetry_read_only() {
        let expected: Vec<u8> = (0x00..=0x25)
            .chain([
                0x37, 0x38, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x50, 0x51,
            ])
            .collect();
        assert_eq!(
            REGISTERS
                .iter()
                .map(|entry| entry.address)
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(REGISTERS.len(), 49);
        for address in [0x0B, 0x10, 0x14, 0x37, 0x50, 0x51] {
            assert!(register_definition(address).unwrap().encode(1.0).is_err());
        }
        assert!(register_definition(0x26).is_err());
        assert!(register_definition(0xFF).is_err());
    }

    #[test]
    fn communication_registers_require_bounded_integers() {
        for address in [0x07, 0x08] {
            let definition = register_definition(address).unwrap();
            assert_eq!(definition.encode(2047.0).unwrap(), 2047);
            for value in [-1.0, 1.5, 2048.0, f64::NAN, f64::INFINITY] {
                assert!(definition.encode(value).is_err());
            }
        }
        assert_eq!(
            register_definition(0x09)
                .unwrap()
                .encode(u32::MAX as f64)
                .unwrap(),
            u32::MAX
        );
        assert!(register_definition(0x09)
            .unwrap()
            .encode(u32::MAX as f64 + 1.0)
            .is_err());
        assert_eq!(register_definition(0x0A).unwrap().encode(4.0).unwrap(), 4);
        assert!(register_definition(0x0A).unwrap().encode(5.0).is_err());
        assert_eq!(register_definition(0x23).unwrap().encode(9.0).unwrap(), 9);
        assert!(register_definition(0x23).unwrap().encode(10.0).is_err());
    }

    #[test]
    fn protection_ranges_are_enforced_after_float32_rounding() {
        let undervoltage = register_definition(0x00).unwrap();
        assert_eq!(undervoltage.encode(24.0).unwrap(), 24.0f32.to_bits());
        assert!(undervoltage.encode(10.0).is_err());
        assert!(undervoltage.encode(10.0 + 1e-9).is_err());
        assert!(register_definition(0x03)
            .unwrap()
            .encode(1.0 - 1e-12)
            .is_err());
        assert!(register_definition(0x15)
            .unwrap()
            .encode(f64::MIN_POSITIVE)
            .is_err());
        assert!(register_definition(0x05).unwrap().encode(1.0).is_err());
        assert_eq!(
            register_definition(0x05).unwrap().encode(-1.0).unwrap(),
            (-1.0f32).to_bits()
        );
        assert!(register_definition(0x02).unwrap().encode(200.0).is_err());
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX] {
            assert!(undervoltage.encode(value).is_err());
        }
    }

    #[test]
    fn nonfinite_float_reads_preserve_raw_bits_without_invalid_json_numbers() {
        let definition = register_definition(0x50).unwrap();
        for raw in [f32::NAN.to_bits(), f32::INFINITY.to_bits()] {
            let reading = definition.decode(raw.to_le_bytes());
            assert_eq!(reading.value, None);
            assert_eq!(reading.raw, raw);
            let json = serde_json::to_value(&reading).unwrap();
            assert!(json["value"].is_null());
            assert_eq!(json["raw"], raw);
        }
        assert_eq!(
            definition.decode((-31.25f32).to_le_bytes()).value,
            Some(-31.25)
        );
    }
}
