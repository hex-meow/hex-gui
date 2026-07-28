use crate::{
    DfuError, Result, APP0_ADDRESS, APP0_MAX_SIZE, FLASH_PAGE_SIZE, FLASH_SECTOR_SIZE,
    PLACEHOLDER_KEY_FINGERPRINT, PLACEHOLDER_PUBKEY_FINGERPRINT,
};

pub const LEGACY_BL_VERSION: u16 = 0x0100;
pub const GCAN_PRODUCT_CODE: u32 = u32::from_be_bytes(*b"gcan");

pub(crate) const HEADER_SIZE: usize = 8;
pub(crate) const WRITE_DATA_MAX: usize = 4096;
pub(crate) const MAX_PAYLOAD: usize = WRITE_DATA_MAX + 4;
pub(crate) const MAX_PACKET: usize = HEADER_SIZE + MAX_PAYLOAD;

pub(crate) const CMD_PING: u8 = 0x00;
pub(crate) const CMD_GET_INFO: u8 = 0x01;
pub(crate) const CMD_ERASE: u8 = 0x10;
pub(crate) const CMD_WRITE: u8 = 0x11;
pub(crate) const CMD_VERIFY: u8 = 0x12;
pub(crate) const CMD_WRITE_KN_DATA: u8 = 0x20;
pub(crate) const CMD_JUMP_APP: u8 = 0xE0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityMode {
    Development,
    ProductionConfidential,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub uid: [u8; 16],
    pub chip_family_id: u32,
    /// Host-side legacy mapping. This field is not present in GET_INFO v1.
    pub product_code: u32,
    pub hw_version: u32,
    pub hw_version_valid: bool,
    pub bl_version: u16,
    pub app0_addr: u32,
    pub app0_max_size: u32,
    pub sector_size: u32,
    pub page_size: u32,
    pub key_fingerprint: u32,
    pub pubkey_fingerprint: u32,
    pub security: SecurityMode,
    /// Informational only. Current firmware reads and reports word 71 but
    /// neither compares nor advances this value.
    pub otp_app_arv_floor: u32,
}

pub fn product_code_for_bl_version(bl_version: u16) -> Option<u32> {
    (bl_version == LEGACY_BL_VERSION).then_some(GCAN_PRODUCT_CODE)
}

pub fn parse_chip_info(payload: &[u8]) -> Result<DeviceInfo> {
    if payload.len() != 128 {
        return Err(DfuError::Protocol(format!(
            "GET_INFO payload must be exactly 128 bytes, got {}",
            payload.len()
        )));
    }

    let mut uid = [0u8; 16];
    uid.copy_from_slice(&payload[0..16]);
    let chip_family_id = read_u32(payload, 16);
    let hw_version = read_u32(payload, 20);
    let bl_version = read_u16(payload, 24);
    let hw_valid = payload[26];
    if payload[27] != 0 {
        return Err(DfuError::Protocol(
            "GET_INFO reserved0 is non-zero for legacy layout".into(),
        ));
    }
    if hw_valid > 1 {
        return Err(DfuError::Protocol(format!(
            "GET_INFO hw_version_valid must be 0 or 1, got {hw_valid}"
        )));
    }

    let app0_addr = read_u32(payload, 28);
    let app0_max_size = read_u32(payload, 32);
    let sector_size = read_u32(payload, 36);
    let page_size = read_u32(payload, 40);
    let key_fingerprint = read_u32(payload, 44);
    let pubkey_fingerprint = read_u32(payload, 48);
    let security = match payload[52] {
        0 => SecurityMode::Development,
        1 => SecurityMode::ProductionConfidential,
        value => {
            return Err(DfuError::Protocol(format!(
                "GET_INFO encrypt_xip must be 0 or 1, got {value}"
            )))
        }
    };
    if payload[53..56].iter().any(|byte| *byte != 0) {
        return Err(DfuError::Protocol(
            "GET_INFO reserved1 is non-zero for legacy layout".into(),
        ));
    }
    let otp_app_arv_floor = read_u32(payload, 56);
    if payload[60..128].iter().any(|byte| *byte != 0) {
        return Err(DfuError::Protocol(
            "GET_INFO reserved2 is non-zero; this is not the frozen legacy layout".into(),
        ));
    }

    let product_code =
        product_code_for_bl_version(bl_version).ok_or(DfuError::UnknownBootloader(bl_version))?;
    let info = DeviceInfo {
        uid,
        chip_family_id,
        product_code,
        hw_version,
        hw_version_valid: hw_valid == 1,
        bl_version,
        app0_addr,
        app0_max_size,
        sector_size,
        page_size,
        key_fingerprint,
        pubkey_fingerprint,
        security,
        otp_app_arv_floor,
    };
    validate_known_device(&info)?;
    Ok(info)
}

pub(crate) fn validate_known_device(info: &DeviceInfo) -> Result<()> {
    let mapped = product_code_for_bl_version(info.bl_version)
        .ok_or(DfuError::UnknownBootloader(info.bl_version))?;
    if info.product_code != mapped {
        return Err(DfuError::InvalidDevice(format!(
            "product code 0x{:08X} does not match legacy mapping 0x{mapped:08X}",
            info.product_code
        )));
    }
    if info.uid.iter().all(|byte| *byte == 0) {
        return Err(DfuError::InvalidDevice("UID is all zero".into()));
    }
    if info.chip_family_id == 0 {
        return Err(DfuError::InvalidDevice(
            "BootROM chip_family_id is zero".into(),
        ));
    }
    if info.app0_addr != APP0_ADDRESS
        || info.app0_max_size != APP0_MAX_SIZE
        || info.sector_size != FLASH_SECTOR_SIZE
        || info.page_size != FLASH_PAGE_SIZE
    {
        return Err(DfuError::InvalidDevice(format!(
            "unexpected APP0 geometry addr=0x{:08X}, max=0x{:08X}, sector={}, page={}",
            info.app0_addr, info.app0_max_size, info.sector_size, info.page_size
        )));
    }
    if info.otp_app_arv_floor > 32 && info.otp_app_arv_floor != u32::MAX {
        return Err(DfuError::InvalidDevice(format!(
            "invalid informational OTP ARV floor {}",
            info.otp_app_arv_floor
        )));
    }
    if info.security == SecurityMode::ProductionConfidential {
        if info.key_fingerprint == PLACEHOLDER_KEY_FINGERPRINT {
            return Err(DfuError::InvalidDevice(
                "protected device reports the public all-zero master-key fingerprint".into(),
            ));
        }
        if info.pubkey_fingerprint == PLACEHOLDER_PUBKEY_FINGERPRINT {
            return Err(DfuError::InvalidDevice(
                "protected device reports the public all-zero root-key fingerprint".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn encode_request(cmd: u8, seq: u8, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return Err(DfuError::Protocol(format!(
            "command 0x{cmd:02X} payload is {} bytes, max is {MAX_PAYLOAD}",
            payload.len()
        )));
    }
    let mut packet = Vec::with_capacity(HEADER_SIZE + payload.len());
    packet.push(cmd);
    packet.push(seq);
    packet.extend_from_slice(&0u16.to_le_bytes());
    packet.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Response<'a> {
    pub status: u16,
    pub body: &'a [u8],
}

pub(crate) fn decode_response<'a>(
    bytes: &'a [u8],
    expected_cmd: u8,
    expected_seq: u8,
) -> Result<Response<'a>> {
    if bytes.len() < HEADER_SIZE {
        return Err(DfuError::Protocol(format!(
            "short response: {} bytes",
            bytes.len()
        )));
    }
    let cmd = bytes[0];
    let seq = bytes[1];
    if cmd != expected_cmd || seq != expected_seq {
        return Err(DfuError::Protocol(format!(
            "response mismatch: expected cmd=0x{expected_cmd:02X}/seq=0x{expected_seq:02X}, got cmd=0x{cmd:02X}/seq=0x{seq:02X}"
        )));
    }
    let status = read_u16(bytes, 2);
    let length = read_u32(bytes, 4) as usize;
    let expected_len = HEADER_SIZE
        .checked_add(length)
        .ok_or_else(|| DfuError::Protocol("response length overflow".into()))?;
    if expected_len != bytes.len() {
        return Err(DfuError::Protocol(format!(
            "response length field says {length} payload bytes, transfer has {}",
            bytes.len()
        )));
    }
    Ok(Response {
        status,
        body: &bytes[HEADER_SIZE..],
    })
}

pub(crate) fn status_name(status: u16) -> &'static str {
    match status {
        0 => "OK",
        1 => "ERR_GENERIC",
        2 => "ERR_INVALID_CMD",
        3 => "ERR_INVALID_ARG",
        4 => "ERR_OUT_OF_REGION",
        5 => "ERR_FLASH",
        6 => "ERR_VERIFY_FAIL",
        7 => "ERR_TOO_LARGE",
        8 => "ERR_NOT_READY",
        _ => "UNKNOWN",
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_fixture(bl_version: u16) -> [u8; 128] {
        let mut payload = [0u8; 128];
        payload[0..16].copy_from_slice(&[0xA5; 16]);
        payload[16..20].copy_from_slice(&0x5300u32.to_le_bytes());
        payload[20..24].copy_from_slice(&0x0001_0002u32.to_le_bytes());
        payload[24..26].copy_from_slice(&bl_version.to_le_bytes());
        payload[26] = 1;
        payload[28..32].copy_from_slice(&APP0_ADDRESS.to_le_bytes());
        payload[32..36].copy_from_slice(&APP0_MAX_SIZE.to_le_bytes());
        payload[36..40].copy_from_slice(&FLASH_SECTOR_SIZE.to_le_bytes());
        payload[40..44].copy_from_slice(&FLASH_PAGE_SIZE.to_le_bytes());
        payload[44..48].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        payload[48..52].copy_from_slice(&0x5566_7788u32.to_le_bytes());
        payload[52] = 1;
        payload[56..60].copy_from_slice(&7u32.to_le_bytes());
        payload
    }

    #[test]
    fn exact_legacy_version_maps_to_ascii_gcan() {
        assert_eq!(
            product_code_for_bl_version(LEGACY_BL_VERSION),
            Some(0x6763_616E)
        );
        assert_eq!(GCAN_PRODUCT_CODE.to_be_bytes(), *b"gcan");
        assert_eq!(product_code_for_bl_version(0x0101), None);
    }

    #[test]
    fn parses_frozen_128_byte_get_info_layout() {
        let info = parse_chip_info(&info_fixture(LEGACY_BL_VERSION)).unwrap();
        assert_eq!(info.uid, [0xA5; 16]);
        assert_eq!(info.chip_family_id, 0x5300);
        assert_eq!(info.hw_version, 0x0001_0002);
        assert!(info.hw_version_valid);
        assert_eq!(info.product_code, GCAN_PRODUCT_CODE);
        assert_eq!(info.security, SecurityMode::ProductionConfidential);
        assert_eq!(info.otp_app_arv_floor, 7);
    }

    #[test]
    fn unknown_version_and_nonzero_reserved_tail_fail_closed() {
        assert!(matches!(
            parse_chip_info(&info_fixture(0x0101)),
            Err(DfuError::UnknownBootloader(0x0101))
        ));
        let mut payload = info_fixture(LEGACY_BL_VERSION);
        payload[60] = 1;
        assert!(matches!(
            parse_chip_info(&payload),
            Err(DfuError::Protocol(_))
        ));
    }

    #[test]
    fn response_checks_cmd_seq_and_exact_length() {
        let mut response = vec![CMD_PING, 9, 0, 0, 2, 0, 0, 0, 0xAA, 0xBB];
        assert_eq!(
            decode_response(&response, CMD_PING, 9).unwrap().body,
            [0xAA, 0xBB]
        );
        response[1] = 8;
        assert!(decode_response(&response, CMD_PING, 9).is_err());
        response[1] = 9;
        response.push(0);
        assert!(decode_response(&response, CMD_PING, 9).is_err());
    }
}
