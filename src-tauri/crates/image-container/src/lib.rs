//! The 256-byte STM32 image-container header — ONE definition for the
//! bootloader and the host tool.
//!
//! Canonical format doc: `stm32-bl/docs/image-container.md` (offsets mirrored
//! in the [`offsets`] module and asserted byte-for-byte in the unit tests).
//! The header is written out-of-band by the host to `0x2110:04` before
//! `clear`, and committed at the selected series' header offset after a
//! successful `start` verification.

#![cfg_attr(not(any(test, feature = "std")), no_std)]

pub use od_consts::{CONTAINER_MAGIC, VENDOR_ID};
use sha2::{Digest, Sha256};

/// Total header length in bytes (a 512-B flash slot holds it; 256 B used).
pub const HEADER_LEN: usize = 256;
/// Plaintext + CRC32 baseline format.
pub const FORMAT_VERSION_V1: u16 = 1;
/// Signed / record-encrypted secure format.
pub const FORMAT_VERSION_V2: u16 = 2;
/// Backwards-compatible name used by the baseline v1 builder.
pub const FORMAT_VERSION: u16 = FORMAT_VERSION_V1;

pub const FLAG_ENCRYPTED: u32 = 1 << 0;
pub const FLAG_SIGNED: u32 = 1 << 1;
pub const V2_FLAGS_SIGNED_ONLY: u32 = FLAG_SIGNED;
pub const V2_FLAGS_SIGNED_ENCRYPTED: u32 = FLAG_SIGNED | FLAG_ENCRYPTED;

/// Little-endian FourCC whose bytes in the header read `G0B1`.
pub const TARGET_MCU_G0B1: u32 = 0x3142_3047;
/// Little-endian FourCC whose bytes in the header read `G431`.
pub const TARGET_MCU_G431: u32 = 0x3133_3447;
/// Little-endian FourCC whose bytes in the header read `G474`.
pub const TARGET_MCU_G474: u32 = 0x3437_3447;

pub const HASH_ALG_SHA256: u8 = 1;
pub const SIG_ALG_ECDSA_P256_SHA256: u8 = 1;
pub const ENC_ALG_NONE: u8 = 0;
pub const ENC_ALG_AES_256_GCM: u8 = 1;

/// Key IDs used by the checked-in G0B1 secure-DFU experiment.
///
/// Key-ID acceptance remains consumer policy so a later product can rotate
/// keys without changing the v2 byte format.
pub const V2_SIGNING_KEY_ID: u32 = 1;
pub const V2_ENCRYPTION_KEY_ID: u32 = 1;

pub const V2_RECORD_PLAIN_SIZE: u16 = 240;
pub const V2_RECORD_TAG_SIZE: u8 = 16;
pub const V2_COMMIT_MAGIC: u32 = 0x3253_4548; // LE bytes: "HES2"

pub const SIGNING_DOMAIN: &[u8; 25] = b"hex-meow/stm32-fw/sign/v2";
pub const RECORD_AAD_DOMAIN: &[u8; 27] = b"hex-meow/stm32-fw/record/v2";
pub const SIGNATURE_DIGEST_LEN: usize = 32;
pub const RECORD_AAD_LEN: usize = 66;
pub const BASE_NONCE_LEN: usize = 12;

/// CRC-32/ISO-HDLC — used for both `header_crc32` and `image_crc32`.
pub const CRC32: crc::Crc<u32, crc::NoTable> =
    crc::Crc::<u32, crc::NoTable>::new(&crc::CRC_32_ISO_HDLC);

/// Byte offsets of every documented field (image-container.md, table §
/// "Container header"). These are the wire/flash layout — never reorder.
pub mod offsets {
    /// u32 — `0x4D454F57`.
    pub const MAGIC: usize = 0;
    /// u16 — format version, `1`.
    pub const FORMAT_VERSION: usize = 4;
    /// u16 — `256`.
    pub const HEADER_LEN: usize = 6;
    /// u32 — bit0 = encrypted, bit1 = signed.
    pub const FLAGS: usize = 8;
    /// u32 — `0x6865786D`.
    pub const VENDOR_ID: usize = 12;
    /// u32 — must equal the device's `0x1018:02` (cross-flash guard).
    pub const PRODUCT_CODE: usize = 16;
    /// u32 — hardware requirement: 0 = any provisioned profile; otherwise
    /// high 16 bits are the exact profile and low 16 bits the minimum revision.
    pub const MIN_HARDWARE_REV: usize = 20;
    /// u32 — logical firmware identity.
    pub const FIRMWARE_ID: usize = 24;
    /// u32 — monotonic; anti-rollback where present.
    pub const FIRMWARE_VERSION: usize = 28;
    /// u8 — target slot; 0 = default/only.
    pub const REGION: usize = 32;
    /// u8[3] — zero.
    pub const RESERVED0: usize = 33;
    /// u32 — per-series application load address.
    pub const LOAD_ADDRESS: usize = 36;
    /// u32 — image length in bytes (header excluded, unpadded).
    pub const IMAGE_SIZE: usize = 40;
    /// u32 — CRC32 over the unpadded plaintext image bytes.
    pub const IMAGE_CRC32: usize = 44;
    /// v1 u8[16] — zero.
    pub const RESERVED1: usize = 48;
    /// v2 u32 — target MCU FourCC.
    pub const TARGET_MCU: usize = 48;
    /// v2 u32 — monotonic security epoch.
    pub const SECURITY_EPOCH: usize = 52;
    /// v2 u8 — image digest algorithm.
    pub const HASH_ALG: usize = 56;
    /// v2 u8 — signature algorithm.
    pub const SIG_ALG: usize = 57;
    /// v2 u8 — encryption algorithm.
    pub const ENC_ALG: usize = 58;
    /// v2 u8 — zero.
    pub const V2_RESERVED0: usize = 59;
    /// v2 u32 — signing-key selector.
    pub const SIGNING_KEY_ID: usize = 60;
    /// v1 zero; v2 SHA-256 over unpadded plaintext.
    pub const IMAGE_SHA256: usize = 64;
    /// v2 alias that makes the digest semantics explicit.
    pub const PLAINTEXT_SHA256: usize = IMAGE_SHA256;
    /// v1 zero; v2 ECDSA-P256 raw `r || s`.
    pub const SIGNATURE: usize = 96;
    /// v1 u8[88] — zero.
    pub const RESERVED2: usize = 160;
    /// v2 u32 — AES key selector (zero in signed-only mode).
    pub const ENCRYPTION_KEY_ID: usize = 160;
    /// v2 u8[12] — base GCM nonce.
    pub const BASE_NONCE: usize = 164;
    /// v2 u16 — maximum plaintext bytes in each encrypted record.
    pub const RECORD_PLAIN_SIZE: usize = 176;
    /// v2 u8 — authentication tag bytes per record.
    pub const RECORD_TAG_SIZE: usize = 178;
    /// v2 u8 — zero.
    pub const V2_RESERVED1: usize = 179;
    /// v2 u32 — exact bytes carried by `image.bin` / `0x2110:03`.
    pub const WIRE_SIZE: usize = 180;
    /// v2 u8[64] — zero.
    pub const V2_RESERVED2: usize = 184;
    /// u32 — CRC32 over `header[0..248]`.
    pub const HEADER_CRC32: usize = 248;
    /// v1 u8[4] — zero.
    pub const RESERVED3: usize = 252;
    /// v2 u32 — fixed `HES2` commit marker.
    pub const COMMIT_MAGIC: usize = 252;
}

/// Why a byte blob was rejected as a container header. Only the *structural*
/// self-consistency checks live here (magic / version / len / header CRC);
/// target checks (vendor, product code, hw rev, size-vs-region) are the
/// consumer's policy, applied via the field accessors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// Input is not exactly [`HEADER_LEN`] bytes.
    WrongLength,
    /// `magic != 0x4D454F57`.
    BadMagic,
    /// `format_version` is neither v1 nor v2.
    BadFormatVersion,
    /// `header_len` field `!= 256`.
    BadHeaderLenField,
    /// `header_crc32` does not match CRC-32/ISO-HDLC over `header[0..248]`.
    BadHeaderCrc,
    /// Secure v2 accepts exactly signed-only (`2`) or signed+encrypted (`3`).
    BadV2Flags,
    /// A secure-v2 hash/signature/encryption algorithm ID is inconsistent.
    BadV2Algorithms,
    /// A byte reserved by secure v2 is nonzero.
    BadV2Reserved,
    /// Secure-v2 mode fields do not agree with `flags`.
    BadV2Mode,
    /// `wire_size` is not the unique size implied by the image and record mode.
    BadV2WireSize,
    /// Bytes `252..256` are not the fixed little-endian `HES2` marker.
    BadV2CommitMagic,
    /// An operation requiring secure v2 was attempted on a v1 header.
    NotSecureV2,
}

/// The two modes accepted by the secure-v2 structural parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V2Mode {
    SignedOnly,
    SignedEncrypted,
}

/// A validated 256-byte container header (owns a copy of the raw bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    raw: [u8; HEADER_LEN],
}

fn get_u32(raw: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]])
}
fn get_u16(raw: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([raw[off], raw[off + 1]])
}

fn put_u32(raw: &mut [u8], off: usize, value: u32) {
    raw[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u16(raw: &mut [u8], off: usize, value: u16) {
    raw[off..off + 2].copy_from_slice(&value.to_le_bytes());
}

fn finish_header_crc(raw: &mut [u8; HEADER_LEN]) {
    let crc = CRC32.checksum(&raw[..offsets::HEADER_CRC32]);
    put_u32(raw, offsets::HEADER_CRC32, crc);
}

fn all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|&byte| byte == 0)
}

/// Return whether a device hardware version satisfies an image requirement.
///
/// Hardware version `major.minor` is encoded as `major << 16 | minor`.
/// Requirement `0` is universal across provisioned profiles. Otherwise the
/// profile major must match exactly and the device minor must be at least the
/// required minor. `0xFFFF_FFFF` is the unprovisioned sentinel and is invalid
/// on either side.
pub const fn hardware_revision_compatible(minimum: u32, actual: u32) -> bool {
    if minimum == u32::MAX || actual == u32::MAX {
        return false;
    }
    if minimum == 0 {
        return true;
    }

    (minimum >> 16) == (actual >> 16) && (minimum as u16) <= (actual as u16)
}

/// Round an unpadded plaintext length up to the STM32 8-byte programming
/// granularity. `None` means the `u32` result would overflow.
pub fn padded_plain_size(image_size: u32) -> Option<u32> {
    image_size.checked_add(7).map(|size| size & !7)
}

/// Number of AES-GCM records needed for an image, after its final plaintext
/// record has been padded with authenticated `0xFF` bytes to 8-byte alignment.
pub fn encrypted_record_count(image_size: u32) -> Option<u32> {
    let padded = padded_plain_size(image_size)?;
    Some(if padded == 0 {
        0
    } else {
        (padded - 1) / u32::from(V2_RECORD_PLAIN_SIZE) + 1
    })
}

/// Exact number of bytes sent to `0x2110:03` and stored as `image.bin`.
///
/// Signed-only is `plaintext || 0xFF padding`; encrypted is a concatenation
/// of `ciphertext || 16-byte tag` records, where the final record encrypts the
/// same authenticated `0xFF` padding.
pub fn expected_wire_size(mode: V2Mode, image_size: u32) -> Option<u32> {
    let padded = padded_plain_size(image_size)?;
    match mode {
        V2Mode::SignedOnly => Some(padded),
        V2Mode::SignedEncrypted => {
            let tags =
                encrypted_record_count(image_size)?.checked_mul(u32::from(V2_RECORD_TAG_SIZE))?;
            padded.checked_add(tags)
        }
    }
}

impl Header {
    /// Parse + structurally validate `bytes` (magic, format version, header
    /// length field, header CRC). Returns an owned, validated header.
    pub fn parse(bytes: &[u8]) -> Result<Header, HeaderError> {
        if bytes.len() != HEADER_LEN {
            return Err(HeaderError::WrongLength);
        }
        let mut raw = [0u8; HEADER_LEN];
        raw.copy_from_slice(bytes);
        if get_u32(&raw, offsets::MAGIC) != CONTAINER_MAGIC {
            return Err(HeaderError::BadMagic);
        }
        let format_version = get_u16(&raw, offsets::FORMAT_VERSION);
        if !matches!(format_version, FORMAT_VERSION_V1 | FORMAT_VERSION_V2) {
            return Err(HeaderError::BadFormatVersion);
        }
        if get_u16(&raw, offsets::HEADER_LEN) as usize != HEADER_LEN {
            return Err(HeaderError::BadHeaderLenField);
        }
        if CRC32.checksum(&raw[..offsets::HEADER_CRC32]) != get_u32(&raw, offsets::HEADER_CRC32) {
            return Err(HeaderError::BadHeaderCrc);
        }
        if format_version == FORMAT_VERSION_V2 {
            Self::validate_v2(&raw)?;
        }
        Ok(Header { raw })
    }

    fn validate_v2(raw: &[u8; HEADER_LEN]) -> Result<(), HeaderError> {
        let flags = get_u32(raw, offsets::FLAGS);
        let mode = match flags {
            V2_FLAGS_SIGNED_ONLY => V2Mode::SignedOnly,
            V2_FLAGS_SIGNED_ENCRYPTED => V2Mode::SignedEncrypted,
            _ => return Err(HeaderError::BadV2Flags),
        };

        if !all_zero(&raw[offsets::RESERVED0..offsets::RESERVED0 + 3])
            || raw[offsets::V2_RESERVED0] != 0
            || raw[offsets::V2_RESERVED1] != 0
            || !all_zero(&raw[offsets::V2_RESERVED2..offsets::HEADER_CRC32])
        {
            return Err(HeaderError::BadV2Reserved);
        }
        if get_u32(raw, offsets::COMMIT_MAGIC) != V2_COMMIT_MAGIC {
            return Err(HeaderError::BadV2CommitMagic);
        }
        if raw[offsets::HASH_ALG] != HASH_ALG_SHA256
            || raw[offsets::SIG_ALG] != SIG_ALG_ECDSA_P256_SHA256
        {
            return Err(HeaderError::BadV2Algorithms);
        }
        // Zero is the format-level "no key" value. Which nonzero ID is
        // trusted is deliberately left to the consuming bootloader.
        if get_u32(raw, offsets::SIGNING_KEY_ID) == 0 {
            return Err(HeaderError::BadV2Mode);
        }

        let encryption_key_id = get_u32(raw, offsets::ENCRYPTION_KEY_ID);
        let nonce = &raw[offsets::BASE_NONCE..offsets::BASE_NONCE + BASE_NONCE_LEN];
        let record_plain_size = get_u16(raw, offsets::RECORD_PLAIN_SIZE);
        let record_tag_size = raw[offsets::RECORD_TAG_SIZE];
        match mode {
            V2Mode::SignedOnly => {
                if raw[offsets::ENC_ALG] != ENC_ALG_NONE
                    || encryption_key_id != 0
                    || !all_zero(nonce)
                    || record_plain_size != 0
                    || record_tag_size != 0
                {
                    return Err(HeaderError::BadV2Mode);
                }
            }
            V2Mode::SignedEncrypted => {
                if raw[offsets::ENC_ALG] != ENC_ALG_AES_256_GCM
                    || encryption_key_id == 0
                    || all_zero(nonce)
                    || record_plain_size != V2_RECORD_PLAIN_SIZE
                    || record_tag_size != V2_RECORD_TAG_SIZE
                {
                    return Err(HeaderError::BadV2Mode);
                }
            }
        }

        let expected = expected_wire_size(mode, get_u32(raw, offsets::IMAGE_SIZE))
            .ok_or(HeaderError::BadV2WireSize)?;
        if get_u32(raw, offsets::WIRE_SIZE) != expected {
            return Err(HeaderError::BadV2WireSize);
        }
        Ok(())
    }

    /// The raw 256 bytes (e.g. for writing to `0x2110:04` / to flash).
    pub fn as_bytes(&self) -> &[u8; HEADER_LEN] {
        &self.raw
    }

    // ---- field accessors ----------------------------------------------------

    pub fn magic(&self) -> u32 {
        get_u32(&self.raw, offsets::MAGIC)
    }
    pub fn format_version(&self) -> u16 {
        get_u16(&self.raw, offsets::FORMAT_VERSION)
    }
    pub fn header_len(&self) -> u16 {
        get_u16(&self.raw, offsets::HEADER_LEN)
    }
    pub fn flags(&self) -> u32 {
        get_u32(&self.raw, offsets::FLAGS)
    }
    /// bit0 of `flags`.
    pub fn flag_encrypted(&self) -> bool {
        self.flags() & 0b01 != 0
    }
    /// bit1 of `flags`.
    pub fn flag_signed(&self) -> bool {
        self.flags() & 0b10 != 0
    }
    pub fn vendor_id(&self) -> u32 {
        get_u32(&self.raw, offsets::VENDOR_ID)
    }
    pub fn product_code(&self) -> u32 {
        get_u32(&self.raw, offsets::PRODUCT_CODE)
    }
    pub fn min_hardware_rev(&self) -> u32 {
        get_u32(&self.raw, offsets::MIN_HARDWARE_REV)
    }
    pub fn firmware_id(&self) -> u32 {
        get_u32(&self.raw, offsets::FIRMWARE_ID)
    }
    pub fn firmware_version(&self) -> u32 {
        get_u32(&self.raw, offsets::FIRMWARE_VERSION)
    }
    pub fn region(&self) -> u8 {
        self.raw[offsets::REGION]
    }
    pub fn load_address(&self) -> u32 {
        get_u32(&self.raw, offsets::LOAD_ADDRESS)
    }
    pub fn image_size(&self) -> u32 {
        get_u32(&self.raw, offsets::IMAGE_SIZE)
    }
    pub fn image_crc32(&self) -> u32 {
        get_u32(&self.raw, offsets::IMAGE_CRC32)
    }
    pub fn image_sha256(&self) -> &[u8] {
        &self.raw[offsets::IMAGE_SHA256..offsets::IMAGE_SHA256 + 32]
    }
    pub fn signature(&self) -> &[u8] {
        &self.raw[offsets::SIGNATURE..offsets::SIGNATURE + 64]
    }
    pub fn header_crc32(&self) -> u32 {
        get_u32(&self.raw, offsets::HEADER_CRC32)
    }

    // ---- secure-v2 accessors ----------------------------------------------

    pub fn v2_mode(&self) -> Option<V2Mode> {
        if self.format_version() != FORMAT_VERSION_V2 {
            return None;
        }
        match self.flags() {
            V2_FLAGS_SIGNED_ONLY => Some(V2Mode::SignedOnly),
            V2_FLAGS_SIGNED_ENCRYPTED => Some(V2Mode::SignedEncrypted),
            _ => None, // unreachable for a parsed Header
        }
    }
    pub fn target_mcu(&self) -> u32 {
        get_u32(&self.raw, offsets::TARGET_MCU)
    }
    pub fn security_epoch(&self) -> u32 {
        get_u32(&self.raw, offsets::SECURITY_EPOCH)
    }
    pub fn hash_algorithm(&self) -> u8 {
        self.raw[offsets::HASH_ALG]
    }
    pub fn signature_algorithm(&self) -> u8 {
        self.raw[offsets::SIG_ALG]
    }
    pub fn encryption_algorithm(&self) -> u8 {
        self.raw[offsets::ENC_ALG]
    }
    pub fn signing_key_id(&self) -> u32 {
        get_u32(&self.raw, offsets::SIGNING_KEY_ID)
    }
    pub fn plaintext_sha256(&self) -> &[u8; SIGNATURE_DIGEST_LEN] {
        self.raw[offsets::PLAINTEXT_SHA256..offsets::PLAINTEXT_SHA256 + SIGNATURE_DIGEST_LEN]
            .try_into()
            .expect("fixed header range has the declared length")
    }
    pub fn encryption_key_id(&self) -> u32 {
        get_u32(&self.raw, offsets::ENCRYPTION_KEY_ID)
    }
    pub fn base_nonce(&self) -> &[u8; BASE_NONCE_LEN] {
        self.raw[offsets::BASE_NONCE..offsets::BASE_NONCE + BASE_NONCE_LEN]
            .try_into()
            .expect("fixed header range has the declared length")
    }
    pub fn record_plain_size(&self) -> u16 {
        get_u16(&self.raw, offsets::RECORD_PLAIN_SIZE)
    }
    pub fn record_tag_size(&self) -> u8 {
        self.raw[offsets::RECORD_TAG_SIZE]
    }
    pub fn wire_size(&self) -> u32 {
        get_u32(&self.raw, offsets::WIRE_SIZE)
    }
    pub fn commit_magic(&self) -> u32 {
        get_u32(&self.raw, offsets::COMMIT_MAGIC)
    }

    /// SHA-256 prehash signed by ECDSA-P256 in secure v2.
    ///
    /// The transcript is an exact byte concatenation, not a Rust/C structure:
    /// `SIGNING_DOMAIN || header[0..96] || header[160..248]`.
    pub fn signature_digest(&self) -> Option<[u8; SIGNATURE_DIGEST_LEN]> {
        if self.format_version() != FORMAT_VERSION_V2 {
            return None;
        }
        let mut digest = Sha256::new();
        digest.update(SIGNING_DOMAIN);
        digest.update(&self.raw[..offsets::SIGNATURE]);
        digest.update(&self.raw[offsets::ENCRYPTION_KEY_ID..offsets::HEADER_CRC32]);
        Some(digest.finalize().into())
    }

    /// Insert a raw 64-byte P-256 `r || s` signature and recompute the
    /// non-security header CRC. Cryptographic validity is consumer policy.
    pub fn with_signature(mut self, signature: [u8; 64]) -> Result<Header, HeaderError> {
        if self.format_version() != FORMAT_VERSION_V2 {
            return Err(HeaderError::NotSecureV2);
        }
        self.raw[offsets::SIGNATURE..offsets::SIGNATURE + signature.len()]
            .copy_from_slice(&signature);
        finish_header_crc(&mut self.raw);
        Header::parse(&self.raw)
    }
}

/// Convenience: CRC-32/ISO-HDLC over `image` — the value that belongs in the
/// `image_crc32` field (over the UNPADDED image bytes, per D11).
pub fn image_crc32_of(image: &[u8]) -> u32 {
    CRC32.checksum(image)
}

/// Build the fixed per-record AES-GCM AAD.
///
/// Layout (66 bytes):
///
/// ```text
/// "hex-meow/stm32-fw/record/v2" ||
/// secure_header_signature_digest[32] ||
/// plaintext_offset_le32 ||
/// plain_len_le16 ||
/// last_u8
/// ```
///
/// `last_u8` is exactly `0` or `1`. The digest binds every signed header
/// field, while the suffix makes record reordering, truncation and duplication
/// fail authentication.
pub fn record_aad(
    header_digest: &[u8; SIGNATURE_DIGEST_LEN],
    plaintext_offset: u32,
    plain_len: u16,
    last: bool,
) -> [u8; RECORD_AAD_LEN] {
    let mut aad = [0u8; RECORD_AAD_LEN];
    let mut cursor = 0;
    aad[cursor..cursor + RECORD_AAD_DOMAIN.len()].copy_from_slice(RECORD_AAD_DOMAIN);
    cursor += RECORD_AAD_DOMAIN.len();
    aad[cursor..cursor + header_digest.len()].copy_from_slice(header_digest);
    cursor += header_digest.len();
    aad[cursor..cursor + 4].copy_from_slice(&plaintext_offset.to_le_bytes());
    cursor += 4;
    aad[cursor..cursor + 2].copy_from_slice(&plain_len.to_le_bytes());
    cursor += 2;
    aad[cursor] = u8::from(last);
    aad
}

/// Derive one record's 96-bit GCM nonce from the signed base nonce.
///
/// The first eight bytes are unchanged. The final four bytes are XORed with
/// the record index encoded big-endian, matching the conventional GCM
/// counter-field byte order.
pub fn derive_record_nonce(
    base_nonce: &[u8; BASE_NONCE_LEN],
    record_index: u32,
) -> [u8; BASE_NONCE_LEN] {
    let mut nonce = *base_nonce;
    for (dst, index_byte) in nonce[8..].iter_mut().zip(record_index.to_be_bytes()) {
        *dst ^= index_byte;
    }
    nonce
}

/// Builder for a strict secure-v2 header.
///
/// `finish()` produces a structurally valid header whose signature bytes are
/// still zero, computes the exact `wire_size`, and stores the header CRC. The
/// release packer then obtains [`Header::signature_digest`], signs that prehash,
/// and calls [`Header::with_signature`], which recomputes the CRC.
#[derive(Debug, Clone)]
pub struct HeaderV2Builder {
    raw: [u8; HEADER_LEN],
}

impl HeaderV2Builder {
    fn common(flags: u32) -> Self {
        let mut raw = [0u8; HEADER_LEN];
        put_u32(&mut raw, offsets::MAGIC, CONTAINER_MAGIC);
        put_u16(&mut raw, offsets::FORMAT_VERSION, FORMAT_VERSION_V2);
        put_u16(&mut raw, offsets::HEADER_LEN, HEADER_LEN as u16);
        put_u32(&mut raw, offsets::FLAGS, flags);
        put_u32(&mut raw, offsets::VENDOR_ID, VENDOR_ID);
        put_u32(&mut raw, offsets::TARGET_MCU, TARGET_MCU_G0B1);
        raw[offsets::HASH_ALG] = HASH_ALG_SHA256;
        raw[offsets::SIG_ALG] = SIG_ALG_ECDSA_P256_SHA256;
        put_u32(&mut raw, offsets::SIGNING_KEY_ID, V2_SIGNING_KEY_ID);
        put_u32(&mut raw, offsets::COMMIT_MAGIC, V2_COMMIT_MAGIC);
        Self { raw }
    }

    /// Signed plaintext mode. This exists for isolated signature-chain tests;
    /// the production G0B1 policy is expected to require encryption.
    pub fn signed_only() -> Self {
        let mut builder = Self::common(V2_FLAGS_SIGNED_ONLY);
        builder.raw[offsets::ENC_ALG] = ENC_ALG_NONE;
        builder
    }

    /// Signed AES-256-GCM record mode, using the experiment's key ID `1`.
    pub fn encrypted(base_nonce: [u8; BASE_NONCE_LEN]) -> Self {
        let mut builder = Self::common(V2_FLAGS_SIGNED_ENCRYPTED);
        builder.raw[offsets::ENC_ALG] = ENC_ALG_AES_256_GCM;
        put_u32(
            &mut builder.raw,
            offsets::ENCRYPTION_KEY_ID,
            V2_ENCRYPTION_KEY_ID,
        );
        builder.raw[offsets::BASE_NONCE..offsets::BASE_NONCE + BASE_NONCE_LEN]
            .copy_from_slice(&base_nonce);
        put_u16(
            &mut builder.raw,
            offsets::RECORD_PLAIN_SIZE,
            V2_RECORD_PLAIN_SIZE,
        );
        builder.raw[offsets::RECORD_TAG_SIZE] = V2_RECORD_TAG_SIZE;
        builder
    }

    fn set_u32(mut self, offset: usize, value: u32) -> Self {
        put_u32(&mut self.raw, offset, value);
        self
    }

    pub fn product_code(self, value: u32) -> Self {
        self.set_u32(offsets::PRODUCT_CODE, value)
    }
    pub fn min_hardware_rev(self, value: u32) -> Self {
        self.set_u32(offsets::MIN_HARDWARE_REV, value)
    }
    pub fn firmware_id(self, value: u32) -> Self {
        self.set_u32(offsets::FIRMWARE_ID, value)
    }
    pub fn firmware_version(self, value: u32) -> Self {
        self.set_u32(offsets::FIRMWARE_VERSION, value)
    }
    pub fn region(mut self, value: u8) -> Self {
        self.raw[offsets::REGION] = value;
        self
    }
    pub fn load_address(self, value: u32) -> Self {
        self.set_u32(offsets::LOAD_ADDRESS, value)
    }
    pub fn target_mcu(self, value: u32) -> Self {
        self.set_u32(offsets::TARGET_MCU, value)
    }
    pub fn security_epoch(self, value: u32) -> Self {
        self.set_u32(offsets::SECURITY_EPOCH, value)
    }
    pub fn signing_key_id(self, value: u32) -> Self {
        self.set_u32(offsets::SIGNING_KEY_ID, value)
    }
    pub fn encryption_key_id(self, value: u32) -> Self {
        self.set_u32(offsets::ENCRYPTION_KEY_ID, value)
    }

    /// Fill all three fields defined over the unpadded plaintext.
    pub fn plaintext(mut self, image: &[u8]) -> Self {
        let digest: [u8; SIGNATURE_DIGEST_LEN] = Sha256::digest(image).into();
        put_u32(&mut self.raw, offsets::IMAGE_SIZE, image.len() as u32);
        put_u32(&mut self.raw, offsets::IMAGE_CRC32, image_crc32_of(image));
        self.raw[offsets::PLAINTEXT_SHA256..offsets::PLAINTEXT_SHA256 + SIGNATURE_DIGEST_LEN]
            .copy_from_slice(&digest);
        self
    }

    /// Set precomputed metadata when the plaintext is streamed elsewhere.
    pub fn image_metadata(
        mut self,
        image_size: u32,
        image_crc32: u32,
        plaintext_sha256: [u8; SIGNATURE_DIGEST_LEN],
    ) -> Self {
        put_u32(&mut self.raw, offsets::IMAGE_SIZE, image_size);
        put_u32(&mut self.raw, offsets::IMAGE_CRC32, image_crc32);
        self.raw[offsets::PLAINTEXT_SHA256..offsets::PLAINTEXT_SHA256 + SIGNATURE_DIGEST_LEN]
            .copy_from_slice(&plaintext_sha256);
        self
    }

    /// Finalize metadata and CRC with a zero signature. This is the header used
    /// to derive both the record AAD digest and the ECDSA prehash.
    pub fn finish(mut self) -> Result<Header, HeaderError> {
        let mode = match get_u32(&self.raw, offsets::FLAGS) {
            V2_FLAGS_SIGNED_ONLY => V2Mode::SignedOnly,
            V2_FLAGS_SIGNED_ENCRYPTED => V2Mode::SignedEncrypted,
            _ => return Err(HeaderError::BadV2Flags),
        };
        let wire_size = expected_wire_size(mode, get_u32(&self.raw, offsets::IMAGE_SIZE))
            .ok_or(HeaderError::BadV2WireSize)?;
        put_u32(&mut self.raw, offsets::WIRE_SIZE, wire_size);
        finish_header_crc(&mut self.raw);
        Header::parse(&self.raw)
    }
}

/// Builder for a baseline (plaintext, unsigned) header. Works no_std — it
/// fills a fixed 256-byte array; `std` adds nothing but is the intended host
/// context. `finish()` computes and stores `header_crc32`.
#[derive(Debug, Clone)]
pub struct HeaderBuilder {
    raw: [u8; HEADER_LEN],
}

impl Default for HeaderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HeaderBuilder {
    /// A baseline header: magic/version/len/vendor prefilled, flags 0,
    /// region 0, all reserved areas zero.
    pub fn new() -> Self {
        let mut raw = [0u8; HEADER_LEN];
        raw[offsets::MAGIC..offsets::MAGIC + 4].copy_from_slice(&CONTAINER_MAGIC.to_le_bytes());
        raw[offsets::FORMAT_VERSION..offsets::FORMAT_VERSION + 2]
            .copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        raw[offsets::HEADER_LEN..offsets::HEADER_LEN + 2]
            .copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        raw[offsets::VENDOR_ID..offsets::VENDOR_ID + 4].copy_from_slice(&VENDOR_ID.to_le_bytes());
        HeaderBuilder { raw }
    }

    fn put_u32(mut self, off: usize, v: u32) -> Self {
        self.raw[off..off + 4].copy_from_slice(&v.to_le_bytes());
        self
    }

    pub fn product_code(self, v: u32) -> Self {
        self.put_u32(offsets::PRODUCT_CODE, v)
    }
    pub fn min_hardware_rev(self, v: u32) -> Self {
        self.put_u32(offsets::MIN_HARDWARE_REV, v)
    }
    pub fn firmware_id(self, v: u32) -> Self {
        self.put_u32(offsets::FIRMWARE_ID, v)
    }
    pub fn firmware_version(self, v: u32) -> Self {
        self.put_u32(offsets::FIRMWARE_VERSION, v)
    }
    pub fn region(mut self, v: u8) -> Self {
        self.raw[offsets::REGION] = v;
        self
    }
    pub fn load_address(self, v: u32) -> Self {
        self.put_u32(offsets::LOAD_ADDRESS, v)
    }
    /// Set `image_size` + `image_crc32` from the actual (unpadded) image bytes.
    pub fn image(self, image: &[u8]) -> Self {
        self.put_u32(offsets::IMAGE_SIZE, image.len() as u32)
            .put_u32(offsets::IMAGE_CRC32, image_crc32_of(image))
    }
    /// Set `image_size` / `image_crc32` directly (when the image is elsewhere).
    pub fn image_size_crc(self, size: u32, crc: u32) -> Self {
        self.put_u32(offsets::IMAGE_SIZE, size)
            .put_u32(offsets::IMAGE_CRC32, crc)
    }

    /// Compute `header_crc32` over bytes `0..248` and return the finished,
    /// self-consistent header.
    pub fn finish(mut self) -> Header {
        let crc = CRC32.checksum(&self.raw[..offsets::HEADER_CRC32]);
        self.raw[offsets::HEADER_CRC32..offsets::HEADER_CRC32 + 4]
            .copy_from_slice(&crc.to_le_bytes());
        Header { raw: self.raw }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_header() -> Header {
        HeaderBuilder::new()
            .product_code(0x0100_494D)
            .min_hardware_rev(2)
            .firmware_id(0xF00D_0001)
            .firmware_version(7)
            .region(0)
            .load_address(0x0800_9200)
            .image(&[0xAA; 1000])
            .finish()
    }

    /// Every documented offset, asserted against the doc's table — the raw
    /// byte positions are the contract, not the accessors.
    #[test]
    fn documented_offsets_are_exact() {
        assert_eq!(offsets::MAGIC, 0);
        assert_eq!(offsets::FORMAT_VERSION, 4);
        assert_eq!(offsets::HEADER_LEN, 6);
        assert_eq!(offsets::FLAGS, 8);
        assert_eq!(offsets::VENDOR_ID, 12);
        assert_eq!(offsets::PRODUCT_CODE, 16);
        assert_eq!(offsets::MIN_HARDWARE_REV, 20);
        assert_eq!(offsets::FIRMWARE_ID, 24);
        assert_eq!(offsets::FIRMWARE_VERSION, 28);
        assert_eq!(offsets::REGION, 32);
        assert_eq!(offsets::RESERVED0, 33);
        assert_eq!(offsets::LOAD_ADDRESS, 36);
        assert_eq!(offsets::IMAGE_SIZE, 40);
        assert_eq!(offsets::IMAGE_CRC32, 44);
        assert_eq!(offsets::RESERVED1, 48);
        assert_eq!(offsets::TARGET_MCU, 48);
        assert_eq!(offsets::SECURITY_EPOCH, 52);
        assert_eq!(offsets::HASH_ALG, 56);
        assert_eq!(offsets::SIG_ALG, 57);
        assert_eq!(offsets::ENC_ALG, 58);
        assert_eq!(offsets::V2_RESERVED0, 59);
        assert_eq!(offsets::SIGNING_KEY_ID, 60);
        assert_eq!(offsets::IMAGE_SHA256, 64);
        assert_eq!(offsets::PLAINTEXT_SHA256, 64);
        assert_eq!(offsets::SIGNATURE, 96);
        assert_eq!(offsets::RESERVED2, 160);
        assert_eq!(offsets::ENCRYPTION_KEY_ID, 160);
        assert_eq!(offsets::BASE_NONCE, 164);
        assert_eq!(offsets::RECORD_PLAIN_SIZE, 176);
        assert_eq!(offsets::RECORD_TAG_SIZE, 178);
        assert_eq!(offsets::V2_RESERVED1, 179);
        assert_eq!(offsets::WIRE_SIZE, 180);
        assert_eq!(offsets::V2_RESERVED2, 184);
        assert_eq!(offsets::HEADER_CRC32, 248);
        assert_eq!(offsets::RESERVED3, 252);
        assert_eq!(offsets::COMMIT_MAGIC, 252);
        assert_eq!(HEADER_LEN, 256);
    }

    /// The builder places every field at its documented byte position.
    #[test]
    fn builder_lays_fields_at_documented_positions() {
        let h = demo_header();
        let b = h.as_bytes();
        assert_eq!(&b[0..4], &0x4D45_4F57u32.to_le_bytes(), "magic @0");
        assert_eq!(&b[4..6], &1u16.to_le_bytes(), "format_version @4");
        assert_eq!(&b[6..8], &256u16.to_le_bytes(), "header_len @6");
        assert_eq!(&b[8..12], &0u32.to_le_bytes(), "flags @8 (baseline 0)");
        assert_eq!(&b[12..16], &0x6865_786Du32.to_le_bytes(), "vendor @12");
        assert_eq!(&b[16..20], &0x0100_494Du32.to_le_bytes(), "product @16");
        assert_eq!(&b[20..24], &2u32.to_le_bytes(), "min_hw_rev @20");
        assert_eq!(&b[24..28], &0xF00D_0001u32.to_le_bytes(), "fw_id @24");
        assert_eq!(&b[28..32], &7u32.to_le_bytes(), "fw_version @28");
        assert_eq!(b[32], 0, "region @32");
        assert_eq!(&b[33..36], &[0; 3], "reserved0 @33");
        assert_eq!(&b[36..40], &0x0800_9200u32.to_le_bytes(), "load_addr @36");
        assert_eq!(&b[40..44], &1000u32.to_le_bytes(), "image_size @40");
        assert_eq!(
            &b[44..48],
            &image_crc32_of(&[0xAA; 1000]).to_le_bytes(),
            "image_crc32 @44"
        );
        assert_eq!(&b[48..64], &[0; 16], "reserved1 @48");
        assert_eq!(&b[64..96], &[0; 32], "image_sha256 @64 (baseline 0)");
        assert_eq!(&b[96..160], &[0u8; 64][..], "signature @96 (baseline 0)");
        assert_eq!(&b[160..248], &[0u8; 88][..], "reserved2 @160");
        assert_eq!(
            &b[248..252],
            &CRC32.checksum(&b[..248]).to_le_bytes(),
            "header_crc32 @248"
        );
        assert_eq!(&b[252..256], &[0; 4], "reserved3 @252");
    }

    #[test]
    fn parse_round_trips_and_accessors_read_back() {
        let built = demo_header();
        let parsed = Header::parse(built.as_bytes()).unwrap();
        assert_eq!(parsed, built);
        assert_eq!(parsed.magic(), CONTAINER_MAGIC);
        assert_eq!(parsed.format_version(), 1);
        assert_eq!(parsed.header_len(), 256);
        assert_eq!(parsed.flags(), 0);
        assert!(!parsed.flag_encrypted() && !parsed.flag_signed());
        assert_eq!(parsed.vendor_id(), VENDOR_ID);
        assert_eq!(parsed.product_code(), 0x0100_494D);
        assert_eq!(parsed.min_hardware_rev(), 2);
        assert_eq!(parsed.firmware_id(), 0xF00D_0001);
        assert_eq!(parsed.firmware_version(), 7);
        assert_eq!(parsed.region(), 0);
        assert_eq!(parsed.load_address(), 0x0800_9200);
        assert_eq!(parsed.image_size(), 1000);
        assert_eq!(parsed.image_crc32(), image_crc32_of(&[0xAA; 1000]));
        assert_eq!(parsed.image_sha256(), &[0u8; 32][..]);
        assert_eq!(parsed.signature(), &[0u8; 64][..]);
    }

    #[test]
    fn parse_rejects_structural_corruption() {
        let good = demo_header();

        assert_eq!(
            Header::parse(&good.as_bytes()[..255]),
            Err(HeaderError::WrongLength)
        );

        let mut b = *good.as_bytes();
        b[0] ^= 0xFF;
        assert_eq!(Header::parse(&b), Err(HeaderError::BadMagic));

        let mut b = *good.as_bytes();
        b[offsets::FORMAT_VERSION] = 3;
        assert_eq!(Header::parse(&b), Err(HeaderError::BadFormatVersion));

        let mut b = *good.as_bytes();
        b[offsets::HEADER_LEN] = 0xFF;
        assert_eq!(Header::parse(&b), Err(HeaderError::BadHeaderLenField));

        // Any payload flip breaks the header CRC…
        let mut b = *good.as_bytes();
        b[offsets::PRODUCT_CODE] ^= 0x01;
        assert_eq!(Header::parse(&b), Err(HeaderError::BadHeaderCrc));
        // …and so does flipping the stored CRC itself.
        let mut b = *good.as_bytes();
        b[offsets::HEADER_CRC32] ^= 0x01;
        assert_eq!(Header::parse(&b), Err(HeaderError::BadHeaderCrc));
    }

    /// Pin CRC-32/ISO-HDLC (the exact polynomial/init/xorout matters: it must
    /// match both `crc32` on the tool side and the BL's `crc` usage).
    #[test]
    fn crc_engine_is_iso_hdlc() {
        assert_eq!(image_crc32_of(b"123456789"), 0xCBF4_3926);
    }

    fn secure_demo(mode: V2Mode) -> Header {
        let image = b"secure-v2 demo plaintext";
        let builder = match mode {
            V2Mode::SignedOnly => HeaderV2Builder::signed_only(),
            V2Mode::SignedEncrypted => {
                HeaderV2Builder::encrypted([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11])
            }
        };
        builder
            .product_code(0x006C_6674)
            .firmware_id(0xD3F0_0001)
            .firmware_version(7)
            .load_address(0x0800_AA00)
            .plaintext(image)
            .finish()
            .unwrap()
    }

    fn repair_crc(raw: &mut [u8; HEADER_LEN]) {
        finish_header_crc(raw);
    }

    #[test]
    fn secure_v2_encrypted_layout_and_accessors_are_exact() {
        let h = secure_demo(V2Mode::SignedEncrypted);
        let b = h.as_bytes();
        assert_eq!(h.format_version(), FORMAT_VERSION_V2);
        assert_eq!(h.v2_mode(), Some(V2Mode::SignedEncrypted));
        assert_eq!(h.flags(), V2_FLAGS_SIGNED_ENCRYPTED);
        assert_eq!(h.target_mcu(), TARGET_MCU_G0B1);
        assert_eq!(h.security_epoch(), 0);
        assert_eq!(h.hash_algorithm(), HASH_ALG_SHA256);
        assert_eq!(h.signature_algorithm(), SIG_ALG_ECDSA_P256_SHA256);
        assert_eq!(h.encryption_algorithm(), ENC_ALG_AES_256_GCM);
        assert_eq!(h.signing_key_id(), V2_SIGNING_KEY_ID);
        assert_eq!(h.encryption_key_id(), V2_ENCRYPTION_KEY_ID);
        assert_eq!(h.base_nonce(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(h.record_plain_size(), V2_RECORD_PLAIN_SIZE);
        assert_eq!(h.record_tag_size(), V2_RECORD_TAG_SIZE);
        assert_eq!(h.image_size(), 24);
        assert_eq!(h.image_crc32(), 0x28D2_769F);
        assert_eq!(
            h.plaintext_sha256(),
            &[
                0x36, 0x30, 0x61, 0x74, 0xE5, 0xB9, 0x37, 0xAB, 0x18, 0xE3, 0xF2, 0x3F, 0x59, 0x57,
                0x0D, 0xC6, 0xD0, 0x48, 0xAD, 0xCB, 0xD7, 0x5E, 0x41, 0x51, 0x47, 0x45, 0xB7, 0x0A,
                0x7A, 0xF0, 0x46, 0xD5,
            ]
        );
        assert_eq!(h.wire_size(), 40); // 24 B ciphertext + one 16 B tag
        assert_eq!(h.commit_magic(), V2_COMMIT_MAGIC);
        assert_eq!(&b[offsets::COMMIT_MAGIC..], b"HES2");
        assert_eq!(&b[offsets::V2_RESERVED2..offsets::HEADER_CRC32], &[0; 64]);
        assert_eq!(h.signature(), &[0; 64]);
        assert_eq!(h.header_crc32(), 0x9A4C_B100);
    }

    #[test]
    fn secure_v2_signed_only_mode_is_canonical() {
        let h = secure_demo(V2Mode::SignedOnly);
        assert_eq!(h.v2_mode(), Some(V2Mode::SignedOnly));
        assert_eq!(h.flags(), V2_FLAGS_SIGNED_ONLY);
        assert_eq!(h.encryption_algorithm(), ENC_ALG_NONE);
        assert_eq!(h.encryption_key_id(), 0);
        assert_eq!(h.base_nonce(), &[0; BASE_NONCE_LEN]);
        assert_eq!(h.record_plain_size(), 0);
        assert_eq!(h.record_tag_size(), 0);
        assert_eq!(h.wire_size(), 24);
    }

    #[test]
    fn secure_v2_signature_digest_is_a_fixed_cross_end_vector() {
        let unsigned = secure_demo(V2Mode::SignedEncrypted);
        let expected = [
            0xAA, 0xF3, 0x8C, 0xED, 0xAE, 0x49, 0x90, 0x8B, 0x6E, 0xE5, 0x58, 0xE5, 0xA5, 0x70,
            0x1D, 0x7D, 0x83, 0x60, 0x66, 0xF6, 0x80, 0x1A, 0x21, 0x3C, 0x24, 0xC0, 0xEE, 0xE4,
            0xF2, 0x1E, 0xF4, 0x1C,
        ];
        assert_eq!(unsigned.signature_digest(), Some(expected));

        let old_crc = unsigned.header_crc32();
        let signed = unsigned.with_signature([0xA5; 64]).unwrap();
        assert_eq!(signed.signature(), &[0xA5; 64]);
        assert_ne!(signed.header_crc32(), old_crc);
        // The signature field and header CRC are deliberately outside the
        // signing transcript, so insertion cannot change the prehash.
        assert_eq!(signed.signature_digest(), Some(expected));

        let v1 = demo_header();
        assert_eq!(v1.signature_digest(), None);
        assert_eq!(v1.with_signature([0xA5; 64]), Err(HeaderError::NotSecureV2));
    }

    #[test]
    fn record_aad_and_nonce_derivation_are_fixed_vectors() {
        let digest = secure_demo(V2Mode::SignedEncrypted)
            .signature_digest()
            .unwrap();
        let aad = record_aad(&digest, 0x1122_3344, 240, true);
        assert_eq!(aad.len(), RECORD_AAD_LEN);
        assert_eq!(&aad[..RECORD_AAD_DOMAIN.len()], RECORD_AAD_DOMAIN);
        assert_eq!(&aad[27..59], &digest);
        assert_eq!(&aad[59..63], &[0x44, 0x33, 0x22, 0x11]);
        assert_eq!(&aad[63..65], &[0xF0, 0x00]);
        assert_eq!(aad[65], 1);

        let nonce = derive_record_nonce(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], 0x0102_0304);
        assert_eq!(nonce, [0, 1, 2, 3, 4, 5, 6, 7, 9, 11, 9, 15]);
        assert_eq!(
            derive_record_nonce(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11], 0),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn wire_size_helpers_cover_alignment_records_and_overflow() {
        assert_eq!(padded_plain_size(1), Some(8));
        assert_eq!(padded_plain_size(240), Some(240));
        assert_eq!(padded_plain_size(241), Some(248));
        assert_eq!(padded_plain_size(u32::MAX), None);

        assert_eq!(encrypted_record_count(0), Some(0));
        assert_eq!(encrypted_record_count(1), Some(1));
        assert_eq!(encrypted_record_count(240), Some(1));
        assert_eq!(encrypted_record_count(241), Some(2));
        assert_eq!(expected_wire_size(V2Mode::SignedOnly, 241), Some(248));
        assert_eq!(
            expected_wire_size(V2Mode::SignedEncrypted, 241),
            Some(248 + 2 * 16)
        );
    }

    #[test]
    fn hardware_revision_gate_is_profile_aware_and_fail_closed() {
        let cases = [
            (0x0000_0000, 0x0001_0000, true),
            (0x0000_0000, 0xFFFF_FFFF, false),
            (0x0001_0000, 0x0001_0000, true),
            (0x0001_0000, 0x0001_0001, true),
            (0x0001_0001, 0x0001_0000, false),
            (0x0001_0000, 0x0002_0000, false),
            (0x0001_FFFF, 0x0002_0000, false),
            (0x0001_0000, 0xFFFF_FFFF, false),
            (0xFFFF_FFFF, 0x0001_0000, false),
            (0xFFFF_FFFF, 0xFFFF_FFFF, false),
        ];

        for (minimum, actual, expected) in cases {
            assert_eq!(
                hardware_revision_compatible(minimum, actual),
                expected,
                "minimum=0x{minimum:08X}, actual=0x{actual:08X}"
            );
        }
    }

    #[test]
    fn secure_v2_parser_rejects_noncanonical_encodings() {
        let good = secure_demo(V2Mode::SignedEncrypted);

        let mut b = *good.as_bytes();
        put_u32(&mut b, offsets::FLAGS, FLAG_ENCRYPTED);
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2Flags));

        let mut b = *good.as_bytes();
        b[offsets::HASH_ALG] = 9;
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2Algorithms));

        let mut b = *good.as_bytes();
        b[offsets::V2_RESERVED2] = 1;
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2Reserved));

        let mut b = *good.as_bytes();
        put_u16(&mut b, offsets::RECORD_PLAIN_SIZE, 128);
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2Mode));

        let mut b = *good.as_bytes();
        b[offsets::BASE_NONCE..offsets::BASE_NONCE + BASE_NONCE_LEN].fill(0);
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2Mode));

        let mut b = *good.as_bytes();
        put_u32(&mut b, offsets::WIRE_SIZE, good.wire_size() + 8);
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2WireSize));

        // The commit marker is outside the header CRC, but is still an exact
        // v2 structural requirement.
        let mut b = *good.as_bytes();
        b[offsets::COMMIT_MAGIC] ^= 1;
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2CommitMagic));

        let signed_only = secure_demo(V2Mode::SignedOnly);
        let mut b = *signed_only.as_bytes();
        put_u32(&mut b, offsets::ENCRYPTION_KEY_ID, 1);
        repair_crc(&mut b);
        assert_eq!(Header::parse(&b), Err(HeaderError::BadV2Mode));
    }
}
