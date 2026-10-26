//! Signed plaintext HPM OTA/3. Legacy 0x0100 identification remains in protocol.rs.
use crate::{DfuError, FlashOutcome, Result, APP0_ADDRESS, APP0_MAX_SIZE};
use p256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const VERSION: u16 = 0x0200;
pub const MAX_PACKET: usize = 4120;
pub const MAILBOX: u16 = 0x2110;
pub const MANIFEST: usize = 0x2000;
pub const MANIFEST_SIZE: usize = 512;
pub const CAP_USB: u32 = 1;
pub const CAP_CAN: u32 = 2;
pub const CAP_PLAINTEXT: u32 = 4;
pub const CAP_CLAIM: u32 = 8;
pub const CAP_TRIAL: u32 = 16;
pub const CLAIM: u8 = 0x30;
pub const RELEASE: u8 = 0x31;
pub const KEEPALIVE: u8 = 0x32;
pub const PREPARE: u8 = 0x33;
pub const WRITE: u8 = 0x34;
pub const COMMIT: u8 = 0x35;
pub const JUMP: u8 = 0x36;

fn bad(message: impl Into<String>) -> DfuError {
    DfuError::InvalidArtifact(message.into())
}
fn protocol(message: impl Into<String>) -> DfuError {
    DfuError::Protocol(message.into())
}
fn u16_at(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub uid: [u8; 16],
    pub chip_family: u32,
    pub hardware: u32,
    pub hardware_valid: bool,
    pub product: u32,
    pub capabilities: u32,
    pub encrypted: bool,
    pub floor: u32,
    pub pubkey_fingerprint: u32,
}
pub fn parse_info(b: &[u8]) -> Result<DeviceInfo> {
    if b.len() != 128 {
        return Err(protocol("V3 GET_INFO requires 128 bytes"));
    }
    if u16_at(b, 24) != VERSION || u16_at(b, 68) != 3 {
        return Err(protocol("not a supported HPM V3 bootloader"));
    }
    let caps = u32_at(b, 64);
    if caps & (CAP_CLAIM | CAP_TRIAL) != (CAP_CLAIM | CAP_TRIAL) {
        return Err(protocol("missing V3 capture/trial capabilities"));
    }
    if b[26] > 1
        || b[52] > 1
        || b[27] != 0
        || b[53..56].iter().any(|v| *v != 0)
        || b[70..].iter().any(|v| *v != 0)
    {
        return Err(protocol("noncanonical V3 GET_INFO"));
    }
    if u32_at(b, 28) != APP0_ADDRESS
        || u32_at(b, 32) != APP0_MAX_SIZE
        || u32_at(b, 36) != 4096
        || u32_at(b, 40) != 256
    {
        return Err(protocol("unsupported V3 Flash geometry"));
    }
    let uid: [u8; 16] = b[..16].try_into().unwrap();
    if uid == [0; 16] || u32_at(b, 16) == 0 {
        return Err(protocol("invalid V3 UID/chip identity"));
    }
    Ok(DeviceInfo {
        uid,
        chip_family: u32_at(b, 16),
        hardware: u32_at(b, 20),
        hardware_valid: b[26] != 0,
        product: u32_at(b, 60),
        capabilities: caps,
        encrypted: b[52] != 0,
        floor: u32_at(b, 56),
        pubkey_fingerprint: u32_at(b, 48),
    })
}

#[derive(Debug, Clone)]
pub struct Artifact {
    image: Vec<u8>,
    manifest: Vec<u8>,
    id: [u8; 20],
    device: DeviceInfo,
    pub source_sha256: String,
    pub image_sha256: String,
    pub source_size: usize,
    pub arv: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    format: String,
    public_key: String,
    product_code: u32,
    image_size: usize,
}
impl Artifact {
    pub fn image(&self) -> &[u8] {
        &self.image
    }
    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }
    pub fn image_id(&self) -> [u8; 20] {
        self.id
    }
    pub fn parse(bytes: Vec<u8>, device: DeviceInfo) -> Result<Self> {
        if device.encrypted || device.capabilities & CAP_PLAINTEXT == 0 {
            return Err(bad(
                "encrypted V3 is not implemented; plaintext downgrade is forbidden",
            ));
        }
        if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
            return Err(bad("V3 package exceeds 2 MiB limit"));
        }
        let source_size = bytes.len();
        let source_sha256 = hex(&Sha256::digest(&bytes));
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| bad(format!("invalid V3 ZIP: {e}")))?;
        if archive.len() != 2 {
            return Err(bad("V3 requires exactly meta.json and app0.bin"));
        }
        let mut meta = None;
        let mut image = None;
        for i in 0..archive.len() {
            let mut f = archive
                .by_index(i)
                .map_err(|e| bad(format!("ZIP member: {e}")))?;
            let (limit, slot) = match f.name() {
                "meta.json" => (4096, &mut meta),
                "app0.bin" => (APP0_MAX_SIZE as usize, &mut image),
                _ => return Err(bad("unexpected V3 ZIP member")),
            };
            if slot.is_some() || f.size() > limit as u64 {
                return Err(bad("duplicate/oversized V3 ZIP member"));
            }
            let mut data = Vec::new();
            f.by_ref()
                .take(limit as u64 + 1)
                .read_to_end(&mut data)
                .map_err(|e| bad(format!("ZIP read: {e}")))?;
            if data.len() > limit || data.len() as u64 != f.size() {
                return Err(bad("invalid decompressed member size"));
            }
            *slot = Some(data);
        }
        let meta: Metadata = serde_json::from_slice(&meta.ok_or_else(|| bad("missing meta.json"))?)
            .map_err(|e| bad(format!("V3 metadata: {e}")))?;
        let image = image.ok_or_else(|| bad("missing app0.bin"))?;
        if meta.format != "hpm-bl-ota/3"
            || meta.product_code != device.product
            || meta.image_size != image.len()
        {
            return Err(bad("V3 format/product/size mismatch"));
        }
        if image.len() < 0x4000 || image.len() > APP0_MAX_SIZE as usize || image.len() % 4096 != 0 {
            return Err(bad("V3 image geometry"));
        }
        let d = &image[MANIFEST..MANIFEST + 96];
        let sig = &image[MANIFEST + 96..MANIFEST + 160];
        if &d[..4] != b"HPM3"
            || u16_at(d, 4) != 3
            || u16_at(d, 6) != 96
            || d[64..].iter().any(|v| *v != 0)
            || u32_at(d, 8) != device.product
            || u32_at(d, 24) != 0
            || u32_at(d, 28) as usize != image.len()
        {
            return Err(bad("invalid signed V3 descriptor"));
        }
        if u32_at(d, 12) > u32_at(d, 16)
            || device.hardware < u32_at(d, 12)
            || device.hardware > u32_at(d, 16)
        {
            return Err(bad("V3 hardware range mismatch"));
        }
        let arv = u32_at(d, 20);
        if device.floor > 32 || arv < device.floor {
            return Err(bad("V3 anti-rollback floor rejects image"));
        }
        if image[MANIFEST + 160..MANIFEST + MANIFEST_SIZE]
            .iter()
            .any(|v| *v != 255)
        {
            return Err(bad("manifest padding must be FF"));
        }
        let mut hash = Sha256::new();
        hash.update(&image[..MANIFEST]);
        hash.update([255; MANIFEST_SIZE]);
        hash.update(&image[MANIFEST + MANIFEST_SIZE..]);
        let digest = hash.finalize();
        if digest[..] != d[32..64] {
            return Err(bad("V3 image SHA-256 mismatch"));
        }
        let h = &image[0x1000..0x1030];
        if &h[..2] != [0xbf, 0x10]
            || !(0x90..=0x1000).contains(&u16_at(h, 2))
            || u16_at(h, 8) >= 0xfe00
            || h[11] != 1
            || u32_at(h, 0x20) != 0x80003000
            || u32_at(h, 0x28) != 0x80003000
            || u32_at(h, 0x14) == 0
            || u32_at(h, 0x14) as usize > image.len() - 0x3000
        {
            return Err(bad("invalid APP ROM header"));
        }
        if meta.public_key.len() != 128 {
            return Err(bad("V3 public key must be x||y, 64 bytes"));
        }
        let mut pk = vec![4u8];
        for pair in meta.public_key.as_bytes().chunks_exact(2) {
            let s = std::str::from_utf8(pair).map_err(|_| bad("public key is not hex"))?;
            pk.push(u8::from_str_radix(s, 16).map_err(|_| bad("public key is not hex"))?);
        }
        if crc32fast::hash(&pk[1..]) != device.pubkey_fingerprint {
            return Err(bad("V3 signing key fingerprint differs from device"));
        }
        let key =
            VerifyingKey::from_sec1_bytes(&pk).map_err(|_| bad("invalid P-256 public key"))?;
        let signature = Signature::from_slice(sig).map_err(|_| bad("invalid P-256 signature"))?;
        let mut transcript = Sha256::new();
        transcript.update(b"hexmeow-hpmota/3");
        transcript.update(d);
        transcript.update(digest);
        key.verify_prehash(&transcript.finalize(), &signature)
            .map_err(|_| bad("V3 P-256 signature verification failed"))?;
        let id = Sha256::digest(d)[..20].try_into().unwrap();
        let manifest = image[MANIFEST..MANIFEST + 160].to_vec();
        let image_sha256 = hex(&digest);
        Ok(Self {
            image,
            manifest,
            id,
            device,
            source_sha256,
            image_sha256,
            source_size,
            arv,
        })
    }
}

/// A complete envelope transaction. Implementations must preserve packet bytes
/// on ACK retry, enforce one deadline, and never retry an ambiguous partial OUT.
pub trait Transport: Send {
    fn exchange(&mut self, packet: &[u8], deadline: Instant) -> Result<Vec<u8>>;
    fn owner(&self) -> u8 {
        1
    }
    fn node_id(&self) -> Option<u8> {
        None
    }
}
pub fn query(
    transport: &mut dyn Transport,
    cmd: u8,
    seq: u8,
    payload: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>> {
    if payload.len() + 8 > MAX_PACKET {
        return Err(protocol("oversized V3 request"));
    }
    let mut p = vec![cmd, seq, 0, 0];
    p.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    p.extend_from_slice(payload);
    let out = transport.exchange(&p, deadline)?;
    if out.len() < 8
        || out.len() > 136
        || out[0] != cmd
        || out[1] != seq
        || u32_at(&out, 4) as usize != out.len() - 8
    {
        return Err(protocol("V3 response cmd/seq/length mismatch"));
    }
    let status = u16_at(&out, 2);
    if status != 0 {
        return Err(DfuError::DeviceStatus {
            cmd,
            status,
            name: match status {
                9 => "BUSY",
                10 => "SESSION",
                11 => "SEQUENCE",
                12 => "SIGNATURE",
                13 => "ENCODING",
                14 => "HARDWARE",
                15 => "ROLLBACK",
                16 => "TRIAL_FAILED",
                17 => "STORAGE",
                _ => "V3_ERROR",
            },
        });
    }
    Ok(out[8..].to_vec())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Capturing,
    Revalidating,
    Preparing,
    Writing,
    Committing,
    RequestingJump,
}
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub stage: Stage,
    pub completed: u64,
    pub total: u64,
    pub cancellable: bool,
}
pub struct Session {
    transport: Box<dyn Transport>,
    pub info: DeviceInfo,
    session: u64,
    rid: u32,
    seq: u8,
    closed: bool,
}
impl Session {
    pub fn node_id(&self) -> Option<u8> {
        self.transport.node_id()
    }
    pub fn claim(
        mut transport: Box<dyn Transport>,
        info: DeviceInfo,
        deadline: Instant,
    ) -> Result<Self> {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| protocol(format!("nonce: {e}")))?;
        let owner = transport.owner();
        let b = query(transport.as_mut(), CLAIM, 2, &nonce, deadline)?;
        if b.len() != 64
            || b[28..44] != info.uid
            || b[48..64] != nonce
            || b[0] != 3
            || b[2] != owner
        {
            return Err(protocol("V3 CLAIM identity/nonce/status mismatch"));
        }
        if u32_at(&b, 44) != 0 {
            return Err(protocol(
                "V3 capture still expires; update the bootloader to latched-wait firmware",
            ));
        }
        let session = u64::from_le_bytes(b[20..28].try_into().unwrap());
        if session == 0 {
            return Err(protocol("zero session ID"));
        }
        Ok(Self {
            transport,
            info,
            session,
            rid: 0,
            seq: 2,
            closed: false,
        })
    }
    fn command(&mut self, cmd: u8, data: &[u8], timeout: Duration) -> Result<Vec<u8>> {
        if self.closed {
            return Err(protocol(
                "capture session is closed; wait for the device again",
            ));
        }
        self.rid = self
            .rid
            .checked_add(1)
            .ok_or_else(|| protocol("request ID exhausted"))?;
        self.seq = self.seq.wrapping_add(1);
        let mut p = self.session.to_le_bytes().to_vec();
        p.extend_from_slice(&self.rid.to_le_bytes());
        p.extend_from_slice(data);
        query(
            self.transport.as_mut(),
            cmd,
            self.seq,
            &p,
            Instant::now() + timeout,
        )
    }
    pub fn keepalive(&mut self) -> Result<()> {
        self.command(KEEPALIVE, &[], Duration::from_secs(2))
            .map(|_| ())
    }
    pub fn close(&mut self) {
        if !self.closed {
            let _ = self.command(RELEASE, &[], Duration::from_secs(1));
            self.closed = true;
        }
    }
    pub fn flash(
        &mut self,
        artifact: &Artifact,
        cancel: &AtomicBool,
        mut progress: impl FnMut(Progress),
    ) -> Result<FlashOutcome> {
        progress(Progress {
            stage: Stage::Revalidating,
            completed: 0,
            total: 1,
            cancellable: true,
        });
        self.seq = self.seq.wrapping_add(1);
        let current = parse_info(&query(
            self.transport.as_mut(),
            1,
            self.seq,
            &[],
            Instant::now() + Duration::from_secs(3),
        )?)?;
        if current != self.info || current != artifact.device {
            return Err(DfuError::DeviceChanged);
        }
        if cancel.load(Ordering::SeqCst) {
            return Ok(FlashOutcome::CancelledBeforeErase);
        }
        progress(Progress {
            stage: Stage::Preparing,
            completed: 0,
            total: 1,
            cancellable: false,
        });
        self.command(PREPARE, &artifact.manifest, Duration::from_secs(10))?;
        for (i, data) in artifact.image.chunks(4096).enumerate() {
            if cancel.load(Ordering::SeqCst) {
                return Ok(if i == 0 {
                    FlashOutcome::CancelledBeforeErase
                } else {
                    FlashOutcome::CancelledRecoverable
                });
            }
            let off = i * 4096;
            let mut body = (off as u32).to_le_bytes().to_vec();
            body.extend_from_slice(data);
            progress(Progress {
                stage: Stage::Writing,
                completed: off as u64,
                total: artifact.image.len() as u64,
                cancellable: false,
            });
            let reply = self.command(WRITE, &body, Duration::from_secs(10))?;
            if reply != (off as u32 + data.len() as u32).to_le_bytes() {
                return Err(protocol("WRITE next offset mismatch"));
            }
            progress(Progress {
                stage: Stage::Writing,
                completed: (off + data.len()) as u64,
                total: artifact.image.len() as u64,
                cancellable: true,
            });
        }
        if cancel.load(Ordering::SeqCst) {
            return Ok(FlashOutcome::CancelledRecoverable);
        }
        progress(Progress {
            stage: Stage::Committing,
            completed: 0,
            total: 1,
            cancellable: false,
        });
        if self.command(COMMIT, &[], Duration::from_secs(15))? != artifact.id {
            return Err(protocol("COMMIT image ID mismatch"));
        }
        progress(Progress {
            stage: Stage::RequestingJump,
            completed: 0,
            total: 1,
            cancellable: false,
        });
        let result = self.command(JUMP, &[], Duration::from_secs(5));
        self.closed = true;
        match result {
            Ok(b) if b.is_empty() => Ok(FlashOutcome::JumpAckedStartupUnconfirmed),
            Err(e @ DfuError::DeviceStatus { .. }) => Err(e),
            _ => Ok(FlashOutcome::JumpOutcomeUnknown),
        }
    }
}

pub enum UsbCapture {
    Legacy(crate::DeviceInfo),
    V3(Session),
}
pub fn capture_usb(cancel: &AtomicBool, wait: Duration) -> Result<UsbCapture> {
    let deadline = Instant::now() + wait;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(protocol("USB capture cancelled"));
        }
        match crate::usb::UsbBootloader::open_raw_unique() {
            Ok(mut transport) => {
                let b = transport.discovery_info(deadline)?;
                if b.len() != 128 {
                    return Err(protocol("GET_INFO length"));
                }
                if u16_at(&b, 24) == crate::LEGACY_BL_VERSION {
                    return Ok(UsbCapture::Legacy(crate::parse_chip_info(&b)?));
                }
                let info = parse_info(&b)?;
                if info.capabilities & CAP_USB == 0 {
                    return Err(protocol("V3 USB capture is not supported"));
                }
                return Session::claim(Box::new(transport), info, deadline).map(UsbCapture::V3);
            }
            Err(DfuError::DeviceNotFound { .. }) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(e) => return Err(e),
        }
    }
}
