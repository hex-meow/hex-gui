//! Per-MCU-series hardware map — the single source of truth (D12/D13).
//!
//! Canonical spec: `can-related-conventions/specs/mcu-series.md` (flash/RAM/NVS
//! map, generated-`memory.x` rule, STAY_MAGIC ABI, G474 single-bank) and
//! `specs/otp-map.md` (OTP identity layout). **No firmware, bootloader, or tool
//! re-declares any of these numbers — they import this crate.**
//!
//! Two ways to consume it:
//!
//! * **Firmware** enables exactly one series cargo feature (`stm32g431` /
//!   `stm32g474` / `stm32g0b1`) and uses the flat, feature-selected constants
//!   ([`FLASH_BASE`], [`PAGE_SIZE`], [`STAY_MAGIC_ADDR`], …).
//! * **Build scripts / host tools** enable the `std` feature (no series
//!   feature), pick a [`McuSeries`] from [`series`] at runtime (a build script
//!   reads the corresponding `CARGO_FEATURE_STM32…` variable), and call [`memory_x`]
//!   to generate the linker script. Hand-written `memory.x` files are
//!   forbidden (mcu-series.md §2).

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(any(
    all(feature = "stm32g431", feature = "stm32g474"),
    all(feature = "stm32g431", feature = "stm32g0b1"),
    all(feature = "stm32g474", feature = "stm32g0b1"),
))]
compile_error!("mcu-series: exactly one series feature must be enabled (`stm32g431`, `stm32g474`, or `stm32g0b1`), got more than one");

#[cfg(not(any(
    feature = "stm32g431",
    feature = "stm32g474",
    feature = "stm32g0b1",
    feature = "std"
)))]
compile_error!("mcu-series: exactly one series feature (`stm32g431`, `stm32g474`, or `stm32g0b1`) must be enabled (or `std` for build-script/host use)");

// ---------------------------------------------------------------------------
// The per-series map type + supported series (specs/mcu-series.md §1)
// ---------------------------------------------------------------------------

/// One MCU series' full memory map. Each entry is pinned to the exact SKU its
/// cargo feature selects (today `stm32g431cb` / `stm32g474cb` /
/// `stm32g0b1cb`, all 128 KB);
/// moving to a different flash-size SKU is a NEW series, not an edit —
/// fielded devices depend on these addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McuSeries {
    /// Cargo-feature / manifest `mcu` name (e.g. `"stm32g431"`).
    pub name: &'static str,
    /// Flash base address (memory-mapped).
    pub flash_base: u32,
    /// Total flash size in bytes.
    pub flash_size: u32,
    /// Flash erase-page size in bytes (G474 is single-bank DBANK=0 ⇒ 4 KB, §3).
    pub page_size: u32,
    /// Bootloader region size (flash-relative `0 .. bl_size`). Ends exactly on
    /// a page boundary so a DFU erase can never touch BL code. G4 remains
    /// 36 KB; the pre-production secure G0B1 profile uses 42 KB.
    pub bl_size: u32,
    /// Flash-relative offset of the 256-B image-container header slot.
    pub header_off: u32,
    /// Flash-relative offset of the application vector table (512-B aligned).
    pub app_off: u32,
    /// Flash-relative offset of the NVS region = end of the app region.
    pub nvs_base_off: u32,
    /// NVS region length in bytes (always the last TWO pages).
    pub nvs_size: u32,
    /// First RAM address.
    pub ram_base: u32,
    /// One past the last RAM address (top of contiguous SRAM; on the G474 the
    /// top of SRAM2 at its native address — deliberately NOT the CCM alias).
    pub ram_top: u32,
}

impl McuSeries {
    /// End of the application region (flash-relative) = start of NVS.
    pub const fn app_end_off(&self) -> u32 {
        self.nvs_base_off
    }
    /// Maximum application image size in bytes.
    pub const fn max_image(&self) -> u32 {
        self.app_end_off() - self.app_off
    }
    /// NVS page size (== flash page size; the NVS always spans two pages).
    pub const fn nvs_page_size(&self) -> u32 {
        self.page_size
    }
    /// Address of the stay-in-bootloader magic word: always `RAM_TOP - 4`
    /// (STAY_MAGIC ABI, mcu-series.md §4 / D13). Never change.
    pub const fn stay_magic_addr(&self) -> u32 {
        self.ram_top - 4
    }
    /// RAM length linked by BOTH the bootloader and applications: the top
    /// 32 bytes are reserved for the bootloader<->application handshake word
    /// (the stay magic at `ram_top - 4` must survive a warm reset and must not
    /// be clobbered by either side's stack).
    pub const fn ram_len_app(&self) -> u32 {
        self.ram_top - self.ram_base - 32
    }
}

/// The series tables, available regardless of cargo features (build scripts /
/// host tools index these by name).
pub mod series {
    use super::McuSeries;

    /// STM32G431CB: 128 KB flash, 2 KB pages, 32 KB RAM.
    pub const STM32G431: McuSeries = McuSeries {
        name: "stm32g431",
        flash_base: 0x0800_0000,
        flash_size: 128 * 1024,
        page_size: 2048,
        bl_size: 0x9000, // 36 KB (R4a); ends on the 0x9000 2 KB-page boundary
        header_off: 0x9000,
        app_off: 0x9200, // 512 B after the header slot (Cortex-M VT alignment)
        // last two 2 KB pages: 0x0001_F000 .. 0x0002_0000 (4 KB)
        nvs_base_off: 0x0001_F000,
        nvs_size: 0x1000,
        ram_base: 0x2000_0000,
        ram_top: 0x2000_8000,
    };

    /// STM32G474CB, **single-bank** (DBANK=0 ⇒ 4 KB pages, mcu-series.md §3):
    /// 128 KB flash, 4 KB pages, 96 KB contiguous SRAM (SRAM1+SRAM2).
    pub const STM32G474: McuSeries = McuSeries {
        name: "stm32g474",
        flash_base: 0x0800_0000,
        flash_size: 128 * 1024,
        page_size: 4096,
        bl_size: 0x9000, // 36 KB (R4a); 0x9000 is a multiple of the 4 KB page
        header_off: 0x9000,
        app_off: 0x9200, // 512 B after the header slot (Cortex-M VT alignment)
        // last two 4 KB pages: 0x0001_E000 .. 0x0002_0000 (8 KB)
        nvs_base_off: 0x0001_E000,
        nvs_size: 0x2000,
        ram_base: 0x2000_0000,
        ram_top: 0x2001_8000,
    };

    /// STM32G0B1CBT6, Cortex-M0+ (thumbv6m): 128 KB flash, **2 KB pages**,
    /// **144 KB** contiguous SRAM. The 128 KB
    /// CB variant is **single flash bank** (RM0444 / stm32-metapac `BANK_1`
    /// only — dual-bank starts at the 512 KB `xE` parts), so there is no DBANK
    /// geometry concern, exactly like the G431. OTP identity map is identical to
    /// the G4 family (`0x1FFF_7000`, RM0444).
    pub const STM32G0B1: McuSeries = McuSeries {
        name: "stm32g0b1",
        flash_base: 0x0800_0000,
        flash_size: 128 * 1024,
        page_size: 2048,
        bl_size: 0xA800, // 42 KB secure profile; 2 KB-page aligned
        header_off: 0xA800,
        app_off: 0xAA00, // 512 B after the header slot (Cortex-M0+ VT ≥256 B)
        // last two 2 KB pages: 0x0001_F000 .. 0x0002_0000 (4 KB), same as G431
        nvs_base_off: 0x0001_F000,
        nvs_size: 0x1000,
        ram_base: 0x2000_0000,
        ram_top: 0x2002_4000, // 144 KB SRAM (0x20000000 + 0x24000)
    };

    /// Look a series up by its manifest / cargo-feature name.
    pub fn by_name(name: &str) -> Option<&'static McuSeries> {
        match name {
            "stm32g431" => Some(&STM32G431),
            "stm32g474" => Some(&STM32G474),
            "stm32g0b1" => Some(&STM32G0B1),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Feature-selected flat constants (firmware use)
// ---------------------------------------------------------------------------

#[cfg(feature = "stm32g431")]
/// The series this firmware is built for (selected by cargo feature).
pub const SERIES: McuSeries = series::STM32G431;
#[cfg(all(feature = "stm32g474", not(feature = "stm32g431")))]
/// The series this firmware is built for (selected by cargo feature).
pub const SERIES: McuSeries = series::STM32G474;
#[cfg(all(
    feature = "stm32g0b1",
    not(feature = "stm32g431"),
    not(feature = "stm32g474")
))]
/// The series this firmware is built for (selected by cargo feature).
pub const SERIES: McuSeries = series::STM32G0B1;

#[cfg(any(feature = "stm32g431", feature = "stm32g474", feature = "stm32g0b1"))]
mod selected {
    use super::SERIES;

    /// Flash base address (`0x0800_0000`).
    pub const FLASH_BASE: u32 = SERIES.flash_base;
    /// Total flash size in bytes (128 KB on both series today).
    pub const FLASH_SIZE: u32 = SERIES.flash_size;
    /// Flash erase-page size in bytes (2048 on G431, 4096 on single-bank G474).
    pub const PAGE_SIZE: u32 = SERIES.page_size;
    /// Bootloader region size, ending on a page boundary (36 KB G4 / 42 KB G0B1).
    pub const BL_SIZE: u32 = SERIES.bl_size;
    /// Flash-relative offset of the 256-B image-container header slot.
    pub const HEADER_OFF: u32 = SERIES.header_off;
    /// Flash-relative offset of the application vector table.
    pub const APP_OFF: u32 = SERIES.app_off;
    /// Flash-relative offset where the NVS region begins (== app region end).
    pub const NVS_BASE_OFF: u32 = SERIES.nvs_base_off;
    /// NVS region length (the last TWO flash pages).
    pub const NVS_SIZE: u32 = SERIES.nvs_size;
    /// NVS page size (== flash page size).
    pub const NVS_PAGE_SIZE: u32 = SERIES.nvs_page_size();
    /// End of the application region, flash-relative (== NVS base).
    pub const APP_END_OFF: u32 = SERIES.app_end_off();
    /// Maximum application image size in bytes.
    pub const MAX_IMAGE: u32 = SERIES.max_image();
    /// First RAM address (`0x2000_0000`).
    pub const RAM_BASE: u32 = SERIES.ram_base;
    /// Top of contiguous RAM (one past the last byte).
    pub const RAM_TOP: u32 = SERIES.ram_top;
    /// Stay-in-bootloader magic word address = `RAM_TOP - 4` (D13; the value
    /// lives in `od-consts::STAY_MAGIC`, universal across series).
    pub const STAY_MAGIC_ADDR: u32 = SERIES.stay_magic_addr();
    /// RAM length linked by BL and apps (top 32 B reserved for the handshake).
    pub const RAM_LEN_APP: u32 = SERIES.ram_len_app();
}

#[cfg(any(feature = "stm32g431", feature = "stm32g474", feature = "stm32g0b1"))]
pub use selected::*;

// ---------------------------------------------------------------------------
// OTP identity map — STM32G4 family, and identical on STM32G0 (both put the
// 1 KB OTP area at 0x1FFF_7000; RM0440 / RM0444). specs/otp-map.md.
// ---------------------------------------------------------------------------

/// OTP: per-unit serial number (`u32` in an 8-byte slot, upper 4 bytes stay
/// `0xFF`). Surfaces as CANopen `0x1018:04`.
pub const OTP_SERIAL: u32 = 0x1FFF_7000;
/// OTP: legacy 6-byte UID slot. **Frozen forever — never reuse.** Fielded
/// units have it programmed; the legacy broadcast DFU that consumed it is
/// deleted, the bytes stay reserved so no future field ever lands here.
pub const OTP_LEGACY_UID: u32 = 0x1FFF_7008;
/// OTP: exact product-specific hardware profile/revision (`u32`). Surfaces as
/// `0x2102`; firmware may use it for internal driver/pin selection and update
/// tooling may use it for image compatibility. It is not a product/API
/// classifier.
pub const OTP_HWVER: u32 = 0x1FFF_7010;
/// OTP: the four product-code slots (D3, last-non-blank-wins: scan from the
/// highest address backwards, first non-`0xFFFF_FFFF` value is the product
/// code; all blank ⇒ unprovisioned ⇒ `0x1018:02 = 0xFFFF_FFFF` and DFU
/// download refused). A provisioning mistake costs one slot; 4 lifetime
/// re-provisions.
pub const OTP_PRODUCT_SLOTS: [u32; 4] = [0x1FFF_7018, 0x1FFF_7020, 0x1FFF_7028, 0x1FFF_7030];

// ---------------------------------------------------------------------------
// memory.x generation (mcu-series.md §2: linker scripts can no longer drift)
// ---------------------------------------------------------------------------

/// Which link layout to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryXKind {
    /// Bootloader: FLASH at the flash base, exactly `bl_size` long.
    Bl,
    /// Application: FLASH at `flash_base + app_off`, `max_image` long.
    App,
}

/// Render the `memory.x` contents for `series`/`kind`. Called by every
/// firmware's `build.rs` (this crate as a `[build-dependency]` with the `std`
/// feature); the output goes to `OUT_DIR`. Both kinds link RAM as
/// `ram_len_app` — the top 32 bytes are reserved on BOTH sides so neither
/// stack can clobber the stay-magic handshake word.
#[cfg(feature = "std")]
pub fn memory_x(series: &McuSeries, kind: MemoryXKind) -> String {
    let (flash_origin, flash_len, what) = match kind {
        MemoryXKind::Bl => (series.flash_base, series.bl_size, "bootloader"),
        MemoryXKind::App => (
            series.flash_base + series.app_off,
            series.max_image(),
            "application",
        ),
    };
    format!(
        "/* GENERATED by build.rs from the mcu-series crate ({name}, {what}) — do not\n\
        \x20  hand-edit, do not commit a memory.x (specs/mcu-series.md forbids it). */\n\
        MEMORY\n\
        {{\n\
        \x20   FLASH : ORIGIN = {fo:#010x}, LENGTH = {fl:#x}\n\
        \x20   /* Top 32 bytes of RAM reserved for the bootloader<->application\n\
        \x20      handshake: the stay-in-DFU magic word at {sm:#010x} (RAM_TOP - 4)\n\
        \x20      survives a warm reset; bootloader AND application link the same\n\
        \x20      reduced RAM so neither stack ever clobbers it. */\n\
        \x20   RAM   : ORIGIN = {rb:#010x}, LENGTH = {rl:#x}\n\
        }}\n",
        name = series.name,
        what = what,
        fo = flash_origin,
        fl = flash_len,
        sm = series.stay_magic_addr(),
        rb = series.ram_base,
        rl = series.ram_len_app(),
    )
}

// ---------------------------------------------------------------------------
// Cross-checks (compile-time; specs/mcu-series.md §1 table)
// ---------------------------------------------------------------------------

const _: () = {
    // --- Invariants that hold on EVERY series (checked per-series so a new
    //     series row cannot skip them; specs/mcu-series.md §1). ---
    const ALL: [McuSeries; 3] = [series::STM32G431, series::STM32G474, series::STM32G0B1];
    let mut i = 0;
    while i < ALL.len() {
        let s = ALL[i];
        // NVS is exactly the last two pages, ending at the flash top.
        assert!(s.nvs_size == 2 * s.page_size);
        assert!(s.nvs_base_off + s.nvs_size == s.flash_size);
        // BL region ends exactly on a page boundary (a DFU erase can never touch
        // BL code), the header slot starts there (no gap/overlap), and the app
        // VT sits exactly 512 B later (Cortex-M VTOR alignment; ≥512 B on M4,
        // ≥256 B on M0+).
        assert!(s.bl_size % s.page_size == 0);
        assert!(s.header_off == s.bl_size);
        assert!(s.app_off - s.header_off == 0x200);
        assert!(s.app_off % 512 == 0);
        // STAY_MAGIC address is always RAM_TOP - 4 (D13).
        assert!(s.stay_magic_addr() == s.ram_top - 4);
        i += 1;
    }
    // --- Per-series literals pinned to the spec table (D13) so a map edit can
    //     never silently move a handshake word or NVS base. ---
    assert!(series::STM32G431.stay_magic_addr() == 0x2000_7FFC);
    assert!(series::STM32G474.stay_magic_addr() == 0x2001_7FFC);
    // G0B1 has 144 KiB RAM, hence RAM_TOP - 4 = 0x2002_3FFC.
    assert!(series::STM32G0B1.stay_magic_addr() == 0x2002_3FFC);
    // G4 ABI remains at 36 KB; the not-yet-released G0 secure profile moves to
    // 42 KB so software crypto retains one page of growth budget.
    assert!(series::STM32G431.bl_size == 0x9000);
    assert!(series::STM32G431.header_off == 0x9000);
    assert!(series::STM32G431.app_off == 0x9200);
    assert!(series::STM32G474.bl_size == 0x9000);
    assert!(series::STM32G474.header_off == 0x9000);
    assert!(series::STM32G474.app_off == 0x9200);
    assert!(series::STM32G0B1.bl_size == 0xA800);
    assert!(series::STM32G0B1.header_off == 0xA800);
    assert!(series::STM32G0B1.app_off == 0xAA00);
    // G431 NVS base per spec: 0x0801_F000 absolute (last two 2 KB pages).
    assert!(series::STM32G431.flash_base + series::STM32G431.nvs_base_off == 0x0801_F000);
    // G474 NVS base per spec: 0x0801_E000 absolute (last two 4 KB pages).
    assert!(series::STM32G474.flash_base + series::STM32G474.nvs_base_off == 0x0801_E000);
    // G0B1 NVS base: 0x0801_F000 absolute (last two 2 KB pages, same as G431).
    assert!(series::STM32G0B1.flash_base + series::STM32G0B1.nvs_base_off == 0x0801_F000);
    // G0B1 is single flash bank (RM0444 / metapac BANK_1 only) — 2 KB pages, and
    // 144 KB contiguous SRAM.
    assert!(series::STM32G0B1.page_size == 2048);
    assert!(series::STM32G0B1.ram_top - series::STM32G0B1.ram_base == 144 * 1024);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g431_map_matches_spec_table() {
        let s = series::STM32G431;
        assert_eq!(s.bl_size, 0x9000); // 36 KB (R4a)
        assert_eq!(s.header_off, 0x9000);
        assert_eq!(s.app_off, 0x9200);
        assert_eq!(s.max_image(), 0x0001_F000 - 0x9200); // 0x15E00 = 89600 B
        assert_eq!(s.ram_len_app(), 32 * 1024 - 32);
        assert_eq!(s.nvs_page_size(), 2048);
    }

    #[test]
    fn g474_map_matches_spec_table() {
        let s = series::STM32G474;
        assert_eq!(s.bl_size, 0x9000); // 36 KB (R4a)
        assert_eq!(s.header_off, 0x9000);
        assert_eq!(s.app_off, 0x9200);
        assert_eq!(s.max_image(), 0x0001_E000 - 0x9200); // 0x14E00 = 85504 B
        assert_eq!(s.ram_len_app(), 96 * 1024 - 32);
        assert_eq!(s.nvs_page_size(), 4096);
        assert_eq!(s.nvs_base_off, 0x0001_E000);
        assert_eq!(s.nvs_size, 0x2000);
    }

    #[test]
    fn g0b1_map_matches_spec_table() {
        let s = series::STM32G0B1;
        assert_eq!(s.bl_size, 0xA800); // 42 KB secure profile
        assert_eq!(s.header_off, 0xA800);
        assert_eq!(s.app_off, 0xAA00);
        assert_eq!(s.page_size, 2048); // single-bank, 2 KB pages (RM0444)
        assert_eq!(s.max_image(), 0x0001_F000 - 0xAA00); // 0x14600 = 83456 B
        assert_eq!(s.ram_len_app(), 144 * 1024 - 32); // 144 KB SRAM, top 32 B reserved
        assert_eq!(s.nvs_page_size(), 2048);
        assert_eq!(s.nvs_base_off, 0x0001_F000);
        assert_eq!(s.nvs_size, 0x1000);
        assert_eq!(s.stay_magic_addr(), 0x2002_3FFC);
    }

    #[test]
    fn memory_x_bl_and_app() {
        let bl = memory_x(&series::STM32G431, MemoryXKind::Bl);
        assert!(bl.contains("ORIGIN = 0x08000000, LENGTH = 0x9000")); // 36 KB (R4a)
        assert!(bl.contains("ORIGIN = 0x20000000, LENGTH = 0x7fe0"));
        let app = memory_x(&series::STM32G431, MemoryXKind::App);
        assert!(app.contains("ORIGIN = 0x08009200, LENGTH = 0x15e00"));
        let app474 = memory_x(&series::STM32G474, MemoryXKind::App);
        assert!(app474.contains("ORIGIN = 0x08009200, LENGTH = 0x14e00"));
        assert!(app474.contains("ORIGIN = 0x20000000, LENGTH = 0x17fe0"));
        // G0B1 secure profile: BL 42 KB, app 0x14600 @ 0x0800AA00,
        // RAM 144 KB - 32 B = 0x23fe0.
        let bl0 = memory_x(&series::STM32G0B1, MemoryXKind::Bl);
        assert!(bl0.contains("ORIGIN = 0x08000000, LENGTH = 0xa800"));
        assert!(bl0.contains("ORIGIN = 0x20000000, LENGTH = 0x23fe0"));
        let app0 = memory_x(&series::STM32G0B1, MemoryXKind::App);
        assert!(app0.contains("ORIGIN = 0x0800aa00, LENGTH = 0x14600"));
        assert!(app0.contains("ORIGIN = 0x20000000, LENGTH = 0x23fe0"));
    }
}
