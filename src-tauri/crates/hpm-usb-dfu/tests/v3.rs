use hpm_usb_dfu::{v3::*, FlashOutcome};
use p256::ecdsa::{signature::hazmat::PrehashSigner, Signature, SigningKey};
use sha2::{Digest, Sha256};
use std::{
    io::{Cursor, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use zip::{write::SimpleFileOptions, ZipWriter};

fn word(b: &mut [u8], off: usize, value: u32) {
    b[off..off + 4].copy_from_slice(&value.to_le_bytes());
}
fn zip_image(image: &[u8], public: &[u8], format: &str) -> Vec<u8> {
    let mut z = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    z.start_file("meta.json", options).unwrap();
    let pk: String = public.iter().map(|b| format!("{b:02x}")).collect();
    write!(z,"{}",serde_json::json!({"format":format,"product_code":0x45564b33u32,"image_size":image.len(),"public_key":pk})).unwrap();
    z.start_file("app0.bin", options).unwrap();
    z.write_all(image).unwrap();
    z.finish().unwrap().into_inner()
}
fn fixture() -> (Vec<u8>, DeviceInfo, Vec<u8>, Vec<u8>) {
    // Public test vector scalar 1; never use as a device/product signing key.
    let mut scalar = [0; 32];
    scalar[31] = 1;
    let key = SigningKey::from_bytes((&scalar).into()).unwrap();
    let public = key.verifying_key().to_encoded_point(false).as_bytes()[1..].to_vec();
    let mut image = vec![255; 0x6000];
    image[0x1000..0x1004].copy_from_slice(&[0xbf, 0x10, 0x90, 0]);
    image[0x1008..0x100c].copy_from_slice(&[0, 0, 0, 1]);
    word(&mut image, 0x1014, 0x1000);
    word(&mut image, 0x1020, 0x80003000);
    word(&mut image, 0x1028, 0x80003000);
    image[0x3000..0x4000].fill(0x42);
    let digest = Sha256::digest(&image);
    let mut d = vec![0; 96];
    d[..8].copy_from_slice(b"HPM3\x03\x00\x60\x00");
    word(&mut d, 8, 0x45564b33);
    word(&mut d, 16, u32::MAX);
    word(&mut d, 20, 4);
    word(&mut d, 28, image.len() as u32);
    d[32..64].copy_from_slice(&digest);
    let mut hash = Sha256::new();
    hash.update(b"hexmeow-hpmota/3");
    hash.update(&d);
    hash.update(digest);
    let sig: Signature = key.sign_prehash(&hash.finalize()).unwrap();
    image[MANIFEST..MANIFEST + 96].copy_from_slice(&d);
    image[MANIFEST + 96..MANIFEST + 160].copy_from_slice(&sig.to_bytes());
    let info = DeviceInfo {
        uid: [7; 16],
        chip_family: 0x5300,
        hardware: 0,
        hardware_valid: false,
        product: 0x45564b33,
        capabilities: 31,
        encrypted: false,
        floor: 0,
        pubkey_fingerprint: crc32fast::hash(&public),
    };
    (
        zip_image(&image, &public, "hpm-bl-ota/3"),
        info,
        image,
        public,
    )
}
fn wire_info(info: &DeviceInfo) -> Vec<u8> {
    let mut b = vec![0; 128];
    b[..16].copy_from_slice(&info.uid);
    word(&mut b, 16, info.chip_family);
    word(&mut b, 20, info.hardware);
    b[24..26].copy_from_slice(&VERSION.to_le_bytes());
    b[26] = info.hardware_valid as u8;
    for (o, v) in [
        (28, 0x80000000),
        (32, 0xc0000),
        (36, 4096),
        (40, 256),
        (48, info.pubkey_fingerprint),
        (56, info.floor),
        (60, info.product),
        (64, info.capabilities),
    ] {
        word(&mut b, o, v);
    }
    b[52] = info.encrypted as u8;
    b[68] = 3;
    b
}
#[test]
fn signed_deflated_package_and_identity_are_validated() {
    let (bytes, info, image, _) = fixture();
    let artifact = Artifact::parse(bytes, info.clone()).unwrap();
    assert_eq!(artifact.image(), image);
    assert_eq!(artifact.arv, 4);
    assert_eq!(parse_info(&wire_info(&info)).unwrap(), info);
    let mut wrong_version = wire_info(&info);
    wrong_version[24..26].copy_from_slice(&0x0100u16.to_le_bytes());
    assert!(parse_info(&wrong_version).is_err());
}
#[test]
fn wrong_identity_encryption_key_and_floor_fail_before_session_writes() {
    let (bytes, info, _, _) = fixture();
    for field in 0..6 {
        let mut i = info.clone();
        match field {
            0 => i.product ^= 1,
            1 => i.encrypted = true,
            2 => i.floor = 5,
            3 => i.floor = u32::MAX,
            4 => i.pubkey_fingerprint ^= 1,
            _ => i.capabilities &= !CAP_PLAINTEXT,
        };
        assert!(Artifact::parse(bytes.clone(), i).is_err());
    }
}
#[test]
fn signature_image_padding_and_cross_format_tampering_fail() {
    let (_, info, image, public) = fixture();
    for off in [0x2008, 0x2060, 0x2100, 0x3000] {
        let mut bad = image.clone();
        bad[off] ^= 1;
        assert!(
            Artifact::parse(zip_image(&bad, &public, "hpm-bl-ota/3"), info.clone()).is_err(),
            "offset {off:x}"
        );
    }
    assert!(Artifact::parse(zip_image(&image, &public, "hpm-bl-ota/2"), info).is_err());
}
#[derive(Default)]
struct Trace {
    requests: Vec<Vec<u8>>,
    next: u32,
    wrong_nonce: bool,
    expiring: bool,
    wrong_owner: bool,
    changed: bool,
}
struct Mock {
    trace: Arc<Mutex<Trace>>,
    info: DeviceInfo,
    id: [u8; 20],
}
impl Transport for Mock {
    fn exchange(&mut self, p: &[u8], _: Instant) -> hpm_usb_dfu::Result<Vec<u8>> {
        let mut t = self.trace.lock().unwrap();
        t.requests.push(p.to_vec());
        let mut body = vec![];
        match p[0] {
            1 => {
                body = wire_info(&self.info);
                if t.changed {
                    body[0] ^= 1;
                }
            }
            CLAIM => {
                body = vec![0; 64];
                body[0] = 3;
                body[2] = if t.wrong_owner { 2 } else { 1 };
                body[20..28].copy_from_slice(&42u64.to_le_bytes());
                body[28..44].copy_from_slice(&self.info.uid);
                body[48..64].copy_from_slice(&p[8..24]);
                if t.wrong_nonce {
                    body[48] ^= 1;
                }
                if t.expiring {
                    word(&mut body, 44, 30000);
                }
            }
            PREPARE | KEEPALIVE | RELEASE | COMMIT | JUMP | WRITE => {
                assert_eq!(u64::from_le_bytes(p[8..16].try_into().unwrap()), 42);
                if p[0] == WRITE {
                    let off = u32::from_le_bytes(p[20..24].try_into().unwrap());
                    assert_eq!(off, t.next);
                    t.next += p.len() as u32 - 24;
                    body = t.next.to_le_bytes().to_vec();
                }
                if p[0] == COMMIT {
                    body = self.id.to_vec();
                }
            }
            other => panic!("unexpected/legacy opcode {other:02x}"),
        }
        let mut reply = vec![p[0], p[1], 0, 0];
        reply.extend_from_slice(&(body.len() as u32).to_le_bytes());
        reply.extend(body);
        Ok(reply)
    }
}
fn claimed(
    trace: Arc<Mutex<Trace>>,
    info: DeviceInfo,
    id: [u8; 20],
) -> hpm_usb_dfu::Result<Session> {
    Session::claim(
        Box::new(Mock {
            trace,
            info: info.clone(),
            id,
        }),
        info,
        Instant::now() + Duration::from_secs(1),
    )
}
#[test]
fn claim_requires_matching_identity_nonce_owner_and_nonexpiring_wait() {
    let (_, info, _, _) = fixture();
    for kind in 0..3 {
        let t = Arc::new(Mutex::new(Trace::default()));
        {
            let mut a = t.lock().unwrap();
            a.wrong_nonce = kind == 0;
            a.wrong_owner = kind == 1;
            a.expiring = kind == 2;
        }
        assert!(claimed(t, info.clone(), [0; 20]).is_err());
    }
}
#[test]
fn flash_revalidates_and_uses_prepare_write_commit_jump_without_legacy_kn() {
    let (bytes, info, image, _) = fixture();
    let a = Artifact::parse(bytes, info.clone()).unwrap();
    let t = Arc::new(Mutex::new(Trace::default()));
    let mut s = claimed(t.clone(), info, a.image_id()).unwrap();
    let outcome = s.flash(&a, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(outcome, FlashOutcome::JumpAckedStartupUnconfirmed);
    let trace = t.lock().unwrap();
    assert_eq!(trace.next, image.len() as u32);
    let cmds: Vec<u8> = trace.requests.iter().map(|p| p[0]).collect();
    assert_eq!(
        cmds,
        [CLAIM, 1, PREPARE, WRITE, WRITE, WRITE, WRITE, WRITE, WRITE, COMMIT, JUMP]
    );
    for (i, p) in trace.requests.iter().skip(2).enumerate() {
        assert_eq!(
            u32::from_le_bytes(p[16..20].try_into().unwrap()),
            i as u32 + 1
        );
    }
}
#[test]
fn identity_change_and_cancellation_prevent_further_mutation() {
    let (bytes, info, _, _) = fixture();
    let a = Artifact::parse(bytes, info.clone()).unwrap();
    let t = Arc::new(Mutex::new(Trace::default()));
    let mut s = claimed(t.clone(), info.clone(), a.image_id()).unwrap();
    t.lock().unwrap().changed = true;
    assert!(s.flash(&a, &AtomicBool::new(false), |_| {}).is_err());
    assert!(t
        .lock()
        .unwrap()
        .requests
        .iter()
        .all(|p| [CLAIM, 1].contains(&p[0])));
    let t = Arc::new(Mutex::new(Trace::default()));
    let mut s = claimed(t.clone(), info, a.image_id()).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        s.flash(&a, &cancel, |p| {
            if p.stage == Stage::Writing && p.completed == 4096 {
                cancel.store(true, Ordering::SeqCst);
            }
        })
        .unwrap(),
        FlashOutcome::CancelledRecoverable
    );
    s.close();
    let t = t.lock().unwrap();
    assert_eq!(t.next, 4096);
    assert!(!t.requests.iter().any(|p| [COMMIT, JUMP].contains(&p[0])));
    assert_eq!(t.requests.last().unwrap()[0], RELEASE);
}
