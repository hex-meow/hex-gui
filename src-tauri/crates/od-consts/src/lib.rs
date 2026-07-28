//! Fleet-shared CANopen object indices and protocol magic values.
//!
//! Canonical specs: `can-related-conventions/specs/canopen-cob-id-and-node-id.md`
//! (§4 hexmeow manufacturer block `0x2100–0x21FF`, node-id story) and
//! `specs/dfu-over-canopen.md` (DFU objects, stay-magic handshake). Firmware
//! and the host tool both import these — no re-declared literals.

#![no_std]

// ---------------------------------------------------------------------------
// hexmeow manufacturer standard block 0x2100–0x21FF (COB-ID doc §4).
// 0x2000–0x20FF is FROZEN reserved-legacy (old hexfellow objects) — never
// allocate there.
// ---------------------------------------------------------------------------

/// `0x2100:01` CAN bitrate (persisted in NVS; read by the bootloader).
pub const OD_BITRATE: u16 = 0x2100;
/// `0x2101` CANopen node-id (persisted in NVS; persist-now-apply-on-reset, D5).
pub const OD_NODE_ID: u16 = 0x2101;
/// `0x2102` hardware version (U32 RO, from OTP; `0xFFFF_FFFF` = unprovisioned).
pub const OD_HW_VERSION: u16 = 0x2102;
/// `0x2103` find-me (U8 RW; write 1 → identify pattern ~30 s; D21, app-only).
pub const OD_FIND_ME: u16 = 0x2103;
/// `0x2110` firmware download record (DFU data path).
pub const OD_FW_DOWNLOAD: u16 = 0x2110;
/// `0x2111` security envelope (per-MCU crypto metadata; HPM — STM32 absent).
pub const OD_SEC_ENVELOPE: u16 = 0x2111;
/// `0x2112` key/pubkey fingerprint (RO; HPM only, reserved).
pub const OD_KEY_FINGERPRINT: u16 = 0x2112;
/// `0x2120` factory provisioning record (`:01` hw ver, `:02` serial, `:03`
/// product code; WO into OTP, blank-only).
pub const OD_PROVISION: u16 = 0x2120;

// ---------------------------------------------------------------------------
// Assembly nameplate block 0x5F00–0x5F03 (specs/device-identity.md). App-area
// objects (top of the CiA manufacturer area) — present only on
// nameplate-provisioned devices, NOT part of the 0x21xx every-device block.
// ---------------------------------------------------------------------------

/// `0x5F00` nameplate record (`:01` kind U8, `:02` assembly hw_rev U32,
/// `:03` manufacture date U32 BCD, `:04` description version U32).
pub const OD_NAMEPLATE: u16 = 0x5F00;
/// `0x5F01` model name (`:01..:04` U64×4, packed ASCII, NUL-padded, ≤32 chars).
pub const OD_NAMEPLATE_MODEL: u16 = 0x5F01;
/// `0x5F02` calibration header (`:01` layout id, `:02` used u32-slot count U8,
/// `:03` CRC-32, `:04` cal date, `:05` station id).
pub const OD_CAL_HEADER: u16 = 0x5F02;
/// `0x5F03` calibration payload (`:01..:32` U64×32 = logical `u32[64]`,
/// low half = earlier slot; semantics per layout id).
pub const OD_CAL_PAYLOAD: u16 = 0x5F03;

/// Nameplate `kind` enum (`0x5F00:01`; device-identity.md §4). `0xFF` (the
/// NVS-erased default) = un-provisioned.
pub mod nameplate_kind {
    pub const ARM: u8 = 1;
    pub const EE: u8 = 2;
    pub const LIFT: u8 = 3;
    pub const BASE: u8 = 4;
    pub const IMU: u8 = 5;
    pub const UNPROVISIONED: u8 = 0xFF;
}

/// Calibration layout ids (`0x5F02:01` = `kind<<16 | layout_ver`).
pub const CAL_LAYOUT_ARM_V1: u32 = 0x0001_0001;
/// lift v1: up to five joints, four u32 slots per joint plus one `dof` slot.
pub const CAL_LAYOUT_LIFT_V1: u32 = 0x0003_0001;
/// imu v1: no factory calibration (`used = 0`); id reserved.
pub const CAL_LAYOUT_IMU_V1: u32 = 0x0005_0001;

// ---------------------------------------------------------------------------
// Protocol magics
// ---------------------------------------------------------------------------

/// Stay-in-bootloader magic VALUE — universal across all series (D13). The
/// per-series ADDRESS (`RAM_TOP - 4`) lives in the `mcu-series` crate. This
/// ABI must never change; every fielded app writes it for a bootloader that
/// ships later.
pub const STAY_MAGIC: u32 = 0xB007_10AD;

/// Image-container header magic (`stm32-bl/docs/image-container.md`).
pub const CONTAINER_MAGIC: u32 = 0x4D45_4F57;

/// hexmeow CANopen vendor id (`0x1018:01`). Tools MUST verify this before
/// interpreting any hexmeow object on a node (COB-ID doc §4).
pub const VENDOR_ID: u32 = 0x6865_786D;

/// `0x1010` store-parameters signature ("save", CiA 301).
pub const SIG_SAVE: u32 = 0x6576_6173;
/// `0x1011` restore-default-parameters signature ("load", CiA 301).
pub const SIG_LOAD: u32 = 0x6461_6F6C;

// ---------------------------------------------------------------------------
// Node-id allocation (COB-ID doc §2/§3)
// ---------------------------------------------------------------------------

/// Fresh-device / rendezvous node-id: an unprovisioned device answers here.
pub const NODE_ID_DEFAULT: u8 = 0x7F;

/// Node-ids that must NEVER be allocated (COB-ID doc §3): at these ids a
/// third-party IAP bootloader's `0x780 + id` identifiers would collide with
/// the CANopen LSS COB-IDs `0x7E4`/`0x7E5`. Enforced by the provisioning tool.
pub const RESERVED_NODE_IDS: [u8; 2] = [100, 101];
