//! Public, read-only device authenticity and first-registration workflow.
//!
//! Discovery remains heartbeat driven. This module re-reads the complete
//! `0x1018` record on the same transport, gates proprietary reads on an exact
//! local product tuple, and never accepts an endpoint or public key from the
//! WebView.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use can_transport::CanBus;
use canopen_sdo::asynch::{upload_bytes_retry, AsyncSdoError};
use canopen_sdo::{SdoAbortCode, SdoError};
use p256::ecdsa::{signature::hazmat::PrehashVerifier, Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::State;
use tokio::sync::Mutex;

use crate::{
    device_registry::{self, DeviceKind},
    state::AppState,
};

const API_ORIGIN: &str = "https://product-auth.hexmeow.com";
const DEVICE_AUTH_DOMAIN: &[u8; 28] = b"hex-meow/device-auth/sign/v1";
const CRC_DOMAIN: &[u8; 23] = b"hex-meow/0x4001/crc/v1\0";
const MANIFEST_UPPER_V1: u16 = 0x0106;
const SDO_TIMEOUT: Duration = Duration::from_millis(700);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_RESPONSE_MAX_BYTES: usize = 64 * 1024;
const MAX_PROOFS: usize = 64;

const LIFT_PUBLIC_KEY_XY: [u8; 64] = [
    0x52, 0x77, 0x77, 0xB3, 0x24, 0xC8, 0x34, 0x7D, 0xB0, 0xB1, 0x68, 0xAB, 0xB0, 0x41, 0x89, 0x90,
    0x52, 0x89, 0x76, 0x6D, 0xDE, 0x95, 0xAC, 0x4B, 0xE6, 0x8C, 0x60, 0xF3, 0xDC, 0x35, 0x0C, 0xC9,
    0x37, 0x09, 0x23, 0xF7, 0xE1, 0x4E, 0x95, 0x2A, 0x7D, 0xF1, 0x86, 0x33, 0x1F, 0x83, 0xF1, 0x2F,
    0x29, 0xA7, 0x01, 0x4F, 0xFA, 0x55, 0x89, 0xDE, 0xFF, 0x5F, 0xA5, 0xCA, 0x7E, 0xFA, 0x37, 0x9F,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub vendor_id: u32,
    pub product_code: u32,
    pub revision_number: u32,
    pub serial_number: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct FactoryWords([u32; 7]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MotorProof {
    identity: Identity,
    factory_words: FactoryWords,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct SignedDeviceProof {
    identity: Identity,
    descriptor: [u8; 8],
    signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "scheme", rename_all = "snake_case", deny_unknown_fields)]
enum DeviceProof {
    MeowMotor { proof: MotorProof },
    SignedP256 { proof: SignedDeviceProof },
}

impl DeviceProof {
    const fn identity(&self) -> Identity {
        match self {
            Self::MeowMotor { proof } => proof.identity,
            Self::SignedP256 { proof } => proof.identity,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthTarget {
    node_id: u8,
    session_epoch: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthenticityDeviceView {
    pub node_id: u8,
    pub session_epoch: u64,
    pub identity: Identity,
    pub device_name: String,
    pub scheme: &'static str,
    pub local_status: &'static str,
    pub detail: String,
    pub signing_key_id: Option<u32>,
    pub digest_hex: Option<String>,
    pub registration_eligible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct OnlineStatusView {
    pub node_id: u8,
    pub session_epoch: u64,
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyResponse {
    statuses: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStatus {
    Registered,
    AlreadyRegistered,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationView {
    pub status: RegistrationStatus,
    pub device_count: u32,
}

#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct ProofRequest {
    proofs: Vec<DeviceProof>,
}

#[derive(Debug, Clone)]
struct CachedProof {
    proof: DeviceProof,
}

#[derive(Default)]
pub struct AuthenticityState {
    cache: Mutex<HashMap<(u8, u64), CachedProof>>,
}

impl AuthenticityState {
    pub async fn clear(&self) {
        self.cache.lock().await.clear();
    }

    async fn replace_node(&self, target: AuthTarget, proof: Option<DeviceProof>) {
        let mut cache = self.cache.lock().await;
        cache.retain(|(node, _), _| *node != target.node_id);
        if let Some(proof) = proof {
            cache.insert(
                (target.node_id, target.session_epoch),
                CachedProof { proof },
            );
        }
    }
}

#[tauri::command]
pub async fn authenticity_inspect(
    state: State<'_, AppState>,
    target: AuthTarget,
) -> Result<AuthenticityDeviceView, String> {
    let _operation = state.device_settings_operation.acquire().await;
    let (view, proof) = inspect_live(&state, target).await?;
    state.authenticity.replace_node(target, proof).await;
    Ok(view)
}

#[tauri::command]
pub async fn authenticity_verify_online(
    state: State<'_, AppState>,
    targets: Vec<AuthTarget>,
) -> Result<Vec<OnlineStatusView>, String> {
    validate_targets(&targets)?;
    let proofs = {
        let cache = state.authenticity.cache.lock().await;
        targets
            .iter()
            .map(|target| {
                cache
                    .get(&(target.node_id, target.session_epoch))
                    .map(|entry| entry.proof.clone())
                    .ok_or_else(|| {
                        format!(
                            "node 0x{:02X} has no valid proof for this heartbeat session",
                            target.node_id
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    ensure_targets_current(&state, &targets, &proofs).await?;
    let response: VerifyResponse =
        post_json("/v1/devices/verify", &ProofRequest { proofs }).await?;
    if response.statuses.len() != targets.len() {
        return Err("verification server returned a mismatched status count".into());
    }
    Ok(targets
        .into_iter()
        .zip(response.statuses)
        .map(|(target, status)| OnlineStatusView {
            node_id: target.node_id,
            session_epoch: target.session_epoch,
            status,
        })
        .collect())
}

#[tauri::command]
pub async fn authenticity_register(
    state: State<'_, AppState>,
    targets: Vec<AuthTarget>,
) -> Result<RegistrationView, String> {
    validate_targets(&targets)?;
    let mut proofs = Vec::with_capacity(targets.len());
    {
        // Registration never trusts a prior WebView/cached proof: re-read the
        // exact live 1018 and source-proof objects under the disconnect gate.
        let _operation = state.device_settings_operation.acquire().await;
        for target in targets.iter().copied() {
            let (view, proof) = inspect_live(&state, target).await?;
            let proof = proof.ok_or_else(|| {
                format!(
                    "node 0x{:02X} is not registration eligible: {}",
                    target.node_id, view.detail
                )
            })?;
            state
                .authenticity
                .replace_node(target, Some(proof.clone()))
                .await;
            proofs.push(proof);
        }
    }
    post_json("/v1/devices/register", &ProofRequest { proofs }).await
}

fn validate_targets(targets: &[AuthTarget]) -> Result<(), String> {
    if targets.is_empty() || targets.len() > MAX_PROOFS {
        return Err(format!("select between 1 and {MAX_PROOFS} devices"));
    }
    let mut nodes = HashSet::with_capacity(targets.len());
    if targets.iter().any(|target| !nodes.insert(target.node_id)) {
        return Err("the selected device set contains a duplicate node-id".into());
    }
    Ok(())
}

async fn inspect_live(
    state: &AppState,
    target: AuthTarget,
) -> Result<(AuthenticityDeviceView, Option<DeviceProof>), String> {
    let expected = target_identity(state, target).await?;
    let bus = state
        .calibration_bus()
        .await
        .ok_or_else(|| "CAN transport is not connected".to_owned())?;
    let identity = read_identity(&bus, target.node_id).await?;
    if identity != expected {
        return Err(format!(
            "node 0x{:02X} complete 0x1018 changed after heartbeat discovery",
            target.node_id
        ));
    }
    let kind = device_registry::classify(identity.vendor_id, identity.product_code);
    match kind {
        DeviceKind::MeowMotor => inspect_meow(target, identity, &bus).await,
        DeviceKind::Lift => inspect_signed(target, identity, &bus).await,
        _ => Err(format!(
            "node 0x{:02X} is not an exact registered authenticity target",
            target.node_id
        )),
    }
}

async fn target_identity(state: &AppState, target: AuthTarget) -> Result<Identity, String> {
    let manager = state
        .manager()
        .await
        .ok_or_else(|| "CAN manager is not connected".to_owned())?;
    let device = manager
        .list()
        .into_iter()
        .find(|device| device.node_id == target.node_id)
        .ok_or_else(|| format!("node 0x{:02X} is not heartbeat-discovered", target.node_id))?;
    if !device.online || device.session_epoch != target.session_epoch {
        return Err(format!(
            "node 0x{:02X} is offline or belongs to a different heartbeat session",
            target.node_id
        ));
    }
    let identity = device
        .identity
        .ok_or_else(|| format!("node 0x{:02X} has no complete 0x1018", target.node_id))?;
    let identity = Identity {
        vendor_id: identity.vendor_id,
        product_code: identity.product_code,
        revision_number: identity.revision_number,
        serial_number: identity.serial_number,
    };
    if !matches!(
        device_registry::classify(identity.vendor_id, identity.product_code),
        DeviceKind::MeowMotor | DeviceKind::Lift
    ) {
        return Err(format!(
            "node 0x{:02X} identity is not in the exact authenticity allowlist",
            target.node_id
        ));
    }
    Ok(identity)
}

async fn ensure_targets_current(
    state: &AppState,
    targets: &[AuthTarget],
    proofs: &[DeviceProof],
) -> Result<(), String> {
    for (target, proof) in targets.iter().copied().zip(proofs) {
        if target_identity(state, target).await? != proof.identity() {
            return Err(format!(
                "node 0x{:02X} cached proof no longer matches the heartbeat identity",
                target.node_id
            ));
        }
    }
    Ok(())
}

async fn read_identity(bus: &Arc<dyn CanBus>, node: u8) -> Result<Identity, String> {
    let count = read_exact::<1>(bus, node, 0x1018, 0).await?[0];
    if count != 4 {
        return Err(format!("0x1018:00 must equal 4, got {count}"));
    }
    Ok(Identity {
        vendor_id: u32::from_le_bytes(read_exact::<4>(bus, node, 0x1018, 1).await?),
        product_code: u32::from_le_bytes(read_exact::<4>(bus, node, 0x1018, 2).await?),
        revision_number: u32::from_le_bytes(read_exact::<4>(bus, node, 0x1018, 3).await?),
        serial_number: u32::from_le_bytes(read_exact::<4>(bus, node, 0x1018, 4).await?),
    })
}

async fn inspect_meow(
    target: AuthTarget,
    identity: Identity,
    bus: &Arc<dyn CanBus>,
) -> Result<(AuthenticityDeviceView, Option<DeviceProof>), String> {
    let highest = read_exact::<1>(bus, target.node_id, 0x4001, 0).await?[0];
    if highest < 7 {
        return Ok((
            view(
                target,
                identity,
                "meow_motor_token",
                "invalid",
                format!("0x4001:00 is {highest}; v1 requires at least 7"),
            ),
            None,
        ));
    }
    let mut words = [0u32; 7];
    for (offset, word) in words.iter_mut().enumerate() {
        *word = u32::from_le_bytes(
            read_exact::<4>(bus, target.node_id, 0x4001, (offset + 1) as u8).await?,
        );
    }
    let factory_words = FactoryWords(words);
    match validate_meow_v1(identity, &factory_words) {
        Ok(()) => {
            let proof = DeviceProof::MeowMotor {
                proof: MotorProof {
                    identity,
                    factory_words,
                },
            };
            let mut result = view(
                target,
                identity,
                "meow_motor_token",
                "envelope_valid",
                format!("0x4001 v1 is locally self-consistent; highest sub-index {highest}"),
            );
            result.registration_eligible = true;
            Ok((result, Some(proof)))
        }
        Err(detail) => Ok((
            view(target, identity, "meow_motor_token", "invalid", detail),
            None,
        )),
    }
}

async fn inspect_signed(
    target: AuthTarget,
    identity: Identity,
    bus: &Arc<dyn CanBus>,
) -> Result<(AuthenticityDeviceView, Option<DeviceProof>), String> {
    let count = match upload(bus, target.node_id, 0x2113, 0).await {
        Ok(bytes) => match exact::<1>(&bytes, "0x2113:00") {
            Ok(value) => value[0],
            Err(detail) => {
                return Ok((
                    view(target, identity, "signed_p256", "invalid", detail),
                    None,
                ));
            }
        },
        Err(error) if object_missing(&error) => {
            return Ok((
                view(
                    target,
                    identity,
                    "signed_p256",
                    "unsupported",
                    "0x2113 is absent".into(),
                ),
                None,
            ));
        }
        Err(error) if subindex_missing(&error) => {
            return Ok((
                view(
                    target,
                    identity,
                    "signed_p256",
                    "invalid",
                    "0x2113 exists but sub-index 00 is absent".into(),
                ),
                None,
            ));
        }
        Err(error) => return Err(sdo_error(0x2113, 0, error)),
    };
    if count != 9 {
        return Ok((
            view(
                target,
                identity,
                "signed_p256",
                "invalid",
                format!("0x2113:00 must equal 9, got {count}"),
            ),
            None,
        ));
    }
    let descriptor = match read_auth_exact::<8>(bus, target.node_id, 1).await {
        Ok(value) => value,
        Err(AuthReadError::Invalid(detail)) => {
            return Ok((
                view(target, identity, "signed_p256", "invalid", detail),
                None,
            ));
        }
        Err(AuthReadError::Transport(detail)) => return Err(detail),
    };
    let mut signature = Vec::with_capacity(64);
    for sub in 2..=9 {
        match read_auth_exact::<8>(bus, target.node_id, sub).await {
            Ok(block) => signature.extend_from_slice(&block),
            Err(AuthReadError::Invalid(detail)) => {
                return Ok((
                    view(target, identity, "signed_p256", "invalid", detail),
                    None,
                ));
            }
            Err(AuthReadError::Transport(detail)) => return Err(detail),
        }
    }
    if descriptor.iter().all(|byte| *byte == 0xFF) {
        return Ok((
            view(
                target,
                identity,
                "signed_p256",
                "unprovisioned",
                "0x2113 descriptor is erased".into(),
            ),
            None,
        ));
    }
    let (key_id, digest) = match verify_signed(identity, descriptor, &signature) {
        Ok(value) => value,
        Err(detail) => {
            return Ok((
                view(target, identity, "signed_p256", "invalid", detail),
                None,
            ));
        }
    };
    let proof = DeviceProof::SignedP256 {
        proof: SignedDeviceProof {
            identity,
            descriptor,
            signature,
        },
    };
    let mut result = view(
        target,
        identity,
        "signed_p256",
        "valid",
        "raw low-S ECDSA-P256 proof verified offline".into(),
    );
    result.signing_key_id = Some(key_id);
    result.digest_hex = Some(hex::encode(digest));
    result.registration_eligible = true;
    Ok((result, Some(proof)))
}

fn view(
    target: AuthTarget,
    identity: Identity,
    scheme: &'static str,
    local_status: &'static str,
    detail: String,
) -> AuthenticityDeviceView {
    AuthenticityDeviceView {
        node_id: target.node_id,
        session_epoch: target.session_epoch,
        identity,
        device_name: device_registry::display_name(identity.vendor_id, identity.product_code)
            .unwrap_or("known hexmeow device")
            .to_owned(),
        scheme,
        local_status,
        detail,
        signing_key_id: None,
        digest_hex: None,
        registration_eligible: false,
    }
}

fn validate_meow_v1(identity: Identity, words: &FactoryWords) -> Result<(), String> {
    if (words.0[0] >> 16) as u16 != MANIFEST_UPPER_V1 {
        return Err("0x4001 manifest is not format v1 / payload length 6".into());
    }
    if words.0[0] as u16 != meow_crc(identity, words) {
        return Err("0x4001 v1 CRC16/CCITT-FALSE mismatch".into());
    }
    let token = u64::from_le_bytes([
        words.0[1] as u8,
        (words.0[1] >> 8) as u8,
        (words.0[1] >> 16) as u8,
        (words.0[1] >> 24) as u8,
        words.0[2] as u8,
        (words.0[2] >> 8) as u8,
        (words.0[2] >> 16) as u8,
        (words.0[2] >> 24) as u8,
    ]);
    if token == 0 {
        return Err("issuance token is zero".into());
    }
    let factor = decode_e4m11(words.0[3] as u16)?;
    let rmse = decode_e4m11((words.0[3] >> 16) as u16)?;
    if factor <= 0.0 || rmse < 0.0 {
        return Err("torque factor/RMSE group is invalid".into());
    }
    if words.0[4] == 0 && words.0[5] == 0 && words.0[6] == 0 {
        return Ok(());
    }
    for raw in [
        words.0[4] as u16,
        (words.0[4] >> 16) as u16,
        words.0[5] as u16,
        (words.0[5] >> 16) as u16,
        words.0[6] as u16,
    ] {
        if decode_e4m11(raw)? <= 0.0 {
            return Err("friction calibration group is incomplete or non-positive".into());
        }
    }
    let _temperature = decode_e4m11((words.0[6] >> 16) as u16)?;
    Ok(())
}

fn meow_crc(identity: Identity, words: &FactoryWords) -> u16 {
    let mut transcript = Vec::with_capacity(61);
    transcript.extend_from_slice(CRC_DOMAIN);
    transcript.extend_from_slice(&identity.vendor_id.to_le_bytes());
    transcript.extend_from_slice(&identity.product_code.to_le_bytes());
    transcript.extend_from_slice(&identity.serial_number.to_le_bytes());
    for word in &words.0[1..] {
        transcript.extend_from_slice(&word.to_le_bytes());
    }
    transcript.extend_from_slice(&MANIFEST_UPPER_V1.to_le_bytes());
    crc16(&transcript)
}

fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xFFFFu16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn decode_e4m11(raw: u16) -> Result<f64, String> {
    let sign = if raw & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = (raw >> 11) & 0x0F;
    let fraction = raw & 0x07FF;
    if exponent == 15 || (exponent == 0 && fraction == 0 && raw & 0x8000 != 0) {
        return Err("e4m11 value is noncanonical or non-finite".into());
    }
    let magnitude = if exponent == 0 {
        2f64.powi(-6) * (f64::from(fraction) / 2048.0)
    } else {
        2f64.powi(i32::from(exponent) - 7) * (1.0 + f64::from(fraction) / 2048.0)
    };
    Ok(sign * magnitude)
}

fn verify_signed(
    identity: Identity,
    descriptor: [u8; 8],
    signature_bytes: &[u8],
) -> Result<(u32, [u8; 32]), String> {
    if descriptor[0..4] != [1, 1, 1, 0] {
        return Err("unsupported or noncanonical 0x2113 descriptor".into());
    }
    let key_id = u32::from_le_bytes(descriptor[4..8].try_into().expect("fixed slice"));
    if key_id == 0 || key_id == u32::MAX {
        return Err("invalid 0x2113 signing key id".into());
    }
    let public_key = match (identity.vendor_id, identity.product_code, key_id) {
        (device_registry::VENDOR_HEXM, device_registry::PRODUCT_LIFT, 1) => &LIFT_PUBLIC_KEY_XY,
        _ => return Err("no exact local public-key profile for 0x1018 + key-id".into()),
    };
    let digest = verify_signed_with_key(identity, descriptor, signature_bytes, public_key)?;
    Ok((key_id, digest))
}

fn verify_signed_with_key(
    identity: Identity,
    descriptor: [u8; 8],
    signature_bytes: &[u8],
    public_key: &[u8; 64],
) -> Result<[u8; 32], String> {
    if identity.serial_number == u32::MAX {
        return Err("0x1018:04 is unprovisioned".into());
    }
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_AUTH_DOMAIN);
    hasher.update(descriptor);
    hasher.update(identity.vendor_id.to_le_bytes());
    hasher.update(identity.product_code.to_le_bytes());
    hasher.update(identity.serial_number.to_le_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    sec1[1..].copy_from_slice(public_key);
    let key = VerifyingKey::from_sec1_bytes(&sec1)
        .map_err(|_| "local public-key registry contains an invalid P-256 point")?;
    let signature = Signature::from_slice(signature_bytes)
        .map_err(|_| "signature contains malformed P-256 scalars")?;
    if signature.normalize_s().is_some() {
        return Err("signature is noncanonical high-S".into());
    }
    key.verify_prehash(&digest, &signature)
        .map_err(|_| "P-256 signature does not match 0x1018 identity")?;
    Ok(digest)
}

async fn read_auth_exact<const N: usize>(
    bus: &Arc<dyn CanBus>,
    node: u8,
    sub: u8,
) -> Result<[u8; N], AuthReadError> {
    let bytes = match upload(bus, node, 0x2113, sub).await {
        Ok(bytes) => bytes,
        Err(error) if proof_member_missing(&error) => {
            return Err(AuthReadError::Invalid(format!(
                "required proof member 0x2113:{sub:02X} is absent"
            )));
        }
        Err(error) => {
            return Err(AuthReadError::Transport(sdo_error(0x2113, sub, error)));
        }
    };
    exact::<N>(&bytes, &format!("0x2113:{sub:02X}")).map_err(AuthReadError::Invalid)
}

enum AuthReadError {
    Invalid(String),
    Transport(String),
}

async fn read_exact<const N: usize>(
    bus: &Arc<dyn CanBus>,
    node: u8,
    index: u16,
    sub: u8,
) -> Result<[u8; N], String> {
    let bytes = upload(bus, node, index, sub)
        .await
        .map_err(|error| sdo_error(index, sub, error))?;
    exact::<N>(&bytes, &format!("0x{index:04X}:{sub:02X}"))
}

fn exact<const N: usize>(bytes: &[u8], object: &str) -> Result<[u8; N], String> {
    bytes
        .try_into()
        .map_err(|_| format!("{object} must be exactly {N} bytes, got {}", bytes.len()))
}

async fn upload(
    bus: &Arc<dyn CanBus>,
    node: u8,
    index: u16,
    sub: u8,
) -> Result<Vec<u8>, AsyncSdoError> {
    upload_bytes_retry(&**bus, node, index, sub, Some(SDO_TIMEOUT), 1).await
}

fn object_missing(error: &AsyncSdoError) -> bool {
    matches!(
        error,
        AsyncSdoError::Sdo(SdoError::ServerAborted(SdoAbortCode::ObjectDoesNotExist))
    )
}

fn subindex_missing(error: &AsyncSdoError) -> bool {
    matches!(
        error,
        AsyncSdoError::Sdo(SdoError::ServerAborted(SdoAbortCode::SubindexDoesNotExist))
    )
}

fn proof_member_missing(error: &AsyncSdoError) -> bool {
    object_missing(error) || subindex_missing(error)
}

fn sdo_error(index: u16, sub: u8, error: AsyncSdoError) -> String {
    format!("SDO upload 0x{index:04X}:{sub:02X} failed: {error}")
}

async fn post_json<T: for<'de> Deserialize<'de>>(
    path: &str,
    request: &ProofRequest,
) -> Result<T, String> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        return Err("cannot install the rustls crypto provider".into());
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("hex-motor-gui/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("building authenticity HTTPS client: {error}"))?;
    let url = format!("{API_ORIGIN}{path}");
    let body = serde_json::to_vec(request)
        .map_err(|error| format!("encoding authenticity request: {error}"))?;
    let mut response = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|error| format!("authenticity service is temporarily unavailable: {error}"))?;
    if response.url().as_str() != url {
        return Err("authenticity response changed the fixed HTTPS URL".into());
    }
    let status = response.status();
    if !status.is_success() {
        let retry = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(|value| format!("; retry after {value} s"))
            .unwrap_or_default();
        return Err(format!(
            "authenticity service returned HTTP {status}{retry}"
        ));
    }
    let initial_capacity = checked_response_capacity(response.content_length())?;
    let mut bytes = Vec::with_capacity(initial_capacity);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("reading authenticity service response: {error}"))?
    {
        append_response_chunk(&mut bytes, &chunk)?;
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid authenticity service response: {error}"))
}

fn checked_response_capacity(content_length: Option<u64>) -> Result<usize, String> {
    match content_length {
        Some(length) if length > HTTP_RESPONSE_MAX_BYTES as u64 => Err(format!(
            "authenticity service response exceeds {HTTP_RESPONSE_MAX_BYTES} bytes"
        )),
        Some(length) => Ok(length as usize),
        None => Ok(0),
    }
}

fn append_response_chunk(bytes: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if chunk.len() > HTTP_RESPONSE_MAX_BYTES.saturating_sub(bytes.len()) {
        return Err(format!(
            "authenticity service response exceeds {HTTP_RESPONSE_MAX_BYTES} bytes"
        ));
    }
    bytes.extend_from_slice(chunk);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEOW_IDENTITY: Identity = Identity {
        vendor_id: 0x0068_6578,
        product_code: 0x6C64_BC78,
        revision_number: 0x0001_0000,
        serial_number: 0x1234_5678,
    };

    const SIGNED_GOLDEN_PUBLIC_KEY_XY: [u8; 64] = [
        0x1c, 0xcb, 0xe9, 0x1c, 0x07, 0x5f, 0xc7, 0xf4, 0xf0, 0x33, 0xbf, 0xa2, 0x48, 0xdb, 0x8f,
        0xcc, 0xd3, 0x56, 0x5d, 0xe9, 0x4b, 0xbf, 0xb1, 0x2f, 0x3c, 0x59, 0xff, 0x46, 0xc2, 0x71,
        0xbf, 0x83, 0xce, 0x40, 0x14, 0xc6, 0x88, 0x11, 0xf9, 0xa2, 0x1a, 0x1f, 0xdb, 0x2c, 0x0e,
        0x61, 0x13, 0xe0, 0x6d, 0xb7, 0xca, 0x93, 0xb7, 0x40, 0x4e, 0x78, 0xdc, 0x7c, 0xcd, 0x5c,
        0xa8, 0x9a, 0x4c, 0xa9,
    ];

    const SIGNED_GOLDEN_SIGNATURE: [u8; 64] = [
        0xa3, 0x88, 0xe0, 0x3d, 0xde, 0xc8, 0x7c, 0x8a, 0x31, 0x14, 0x6d, 0x54, 0xb3, 0x6f, 0x41,
        0xfd, 0x1f, 0xa3, 0x82, 0x8d, 0x74, 0xf9, 0xbf, 0x7f, 0x4a, 0x7b, 0xc2, 0x43, 0xbf, 0x80,
        0xa5, 0x55, 0x56, 0xa6, 0x77, 0x14, 0x3b, 0xad, 0x62, 0x61, 0x24, 0x1e, 0x04, 0x09, 0x20,
        0x59, 0x03, 0x00, 0x85, 0xf5, 0xfa, 0x8d, 0x2a, 0xde, 0xed, 0x3e, 0x9d, 0x30, 0xfd, 0x7f,
        0x52, 0x06, 0x1c, 0xdc,
    ];

    #[test]
    fn frozen_meow_vector_is_accepted_and_tampering_is_not() {
        let words = FactoryWords([
            0x0106_f6f9,
            0x4643_12d5,
            0x0fb6_4b09,
            0x1ccd_359a,
            0x4000_3800,
            0x1ccd_3000,
            0x5c80_3800,
        ]);
        assert!(validate_meow_v1(MEOW_IDENTITY, &words).is_ok());
        let mut changed = words.clone();
        changed.0[4] ^= 1;
        assert!(validate_meow_v1(MEOW_IDENTITY, &changed).is_err());
    }

    #[test]
    fn frozen_signed_device_vector_is_accepted_and_identity_is_bound() {
        let identity = Identity {
            vendor_id: 0x6865_786D,
            product_code: 0x006C_6674,
            revision_number: 0x2026_0807,
            serial_number: 0x1357_9BDF,
        };
        let descriptor = [1, 1, 1, 0, 1, 0, 0, 0];
        let expected_digest = [
            0x1f, 0x8a, 0xab, 0xba, 0xeb, 0x6e, 0x29, 0x3d, 0x17, 0xa0, 0x27, 0xd5, 0x48, 0x9b,
            0x14, 0x52, 0x21, 0x24, 0x31, 0xaf, 0x7b, 0x1f, 0x91, 0x21, 0x94, 0x30, 0xd2, 0x7d,
            0x88, 0x9c, 0x24, 0x21,
        ];
        assert_eq!(
            verify_signed_with_key(
                identity,
                descriptor,
                &SIGNED_GOLDEN_SIGNATURE,
                &SIGNED_GOLDEN_PUBLIC_KEY_XY,
            )
            .unwrap(),
            expected_digest,
        );

        let mut revision_changed = identity;
        revision_changed.revision_number ^= u32::MAX;
        assert!(verify_signed_with_key(
            revision_changed,
            descriptor,
            &SIGNED_GOLDEN_SIGNATURE,
            &SIGNED_GOLDEN_PUBLIC_KEY_XY,
        )
        .is_ok());

        let mut serial_changed = identity;
        serial_changed.serial_number ^= 1;
        assert!(verify_signed_with_key(
            serial_changed,
            descriptor,
            &SIGNED_GOLDEN_SIGNATURE,
            &SIGNED_GOLDEN_PUBLIC_KEY_XY,
        )
        .is_err());
    }

    #[test]
    fn target_set_rejects_duplicates_and_out_of_range() {
        let one = AuthTarget {
            node_id: 1,
            session_epoch: 2,
        };
        assert!(validate_targets(&[one]).is_ok());
        assert!(validate_targets(&[]).is_err());
        assert!(validate_targets(&[one, one]).is_err());
    }

    #[test]
    fn response_limit_checks_declared_and_streamed_sizes() {
        assert_eq!(checked_response_capacity(None).unwrap(), 0);
        assert_eq!(
            checked_response_capacity(Some(HTTP_RESPONSE_MAX_BYTES as u64)).unwrap(),
            HTTP_RESPONSE_MAX_BYTES
        );
        assert!(checked_response_capacity(Some(HTTP_RESPONSE_MAX_BYTES as u64 + 1)).is_err());

        let mut bytes = vec![0_u8; HTTP_RESPONSE_MAX_BYTES - 1];
        append_response_chunk(&mut bytes, &[1]).unwrap();
        assert_eq!(bytes.len(), HTTP_RESPONSE_MAX_BYTES);
        assert!(append_response_chunk(&mut bytes, &[2]).is_err());
    }
}
