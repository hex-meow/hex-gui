//! Attended, one-motor-at-a-time update of a valid Meow Motor calibration.
//!
//! The public service proves only the immutable identity + issuance token. The
//! original factory payload remains in the private ledger; user recalibration
//! changes only the motor's host-owned payload and CRC while preserving token
//! bytes exactly.

use std::{sync::Arc, time::Duration};

use can_transport::CanBus;
use canopen_sdo::asynch::{download_bytes_retry, upload_bytes_retry};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::State;
use tokio::sync::Mutex;

use crate::{
    authenticity::{
        decode_e4m11, encode_e4m11, meow_crc, read_identity, target_identity, validate_meow_v1,
        verify_meow_words_online, AuthTarget, FactoryWords, Identity, MANIFEST_UPPER_V1,
    },
    device_registry::{self, DeviceKind},
    state::AppState,
};

const SDO_TIMEOUT: Duration = Duration::from_millis(700);
const SAVE_SETTLE: Duration = Duration::from_millis(1_000);
const STORE_PARAMETERS_SIGNATURE: u32 = 0x6576_6173;
const MAX_RESULT_JSON_BYTES: usize = 1024 * 1024;
const RECORD_READ_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Serialize)]
pub struct RawWordView {
    pub subindex: u8,
    pub value_u32: u32,
    pub value_hex: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrictionPayloadView {
    pub static_pos_raw_nm: f64,
    pub static_neg_raw_nm: f64,
    pub kinetic_pos_raw_nm: f64,
    pub kinetic_neg_raw_nm: f64,
    pub reference_speed_rad_per_s: f64,
    pub calibration_temperature_c: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationPayloadView {
    pub torque_factor: f64,
    pub torque_fit_rmse_nm: f64,
    pub friction: Option<FrictionPayloadView>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CalibrationSourceView {
    pub vendor_id: u32,
    pub product_code: u32,
    pub revision_number: u32,
    pub serial_number: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationUpdatePreparedView {
    pub node_id: u8,
    pub session_epoch: u64,
    pub identity: Identity,
    pub online_status: String,
    pub token_decimal: String,
    pub token_hex: String,
    pub highest_subindex: u8,
    pub backup_words: Vec<RawWordView>,
    pub current_calibration: CalibrationPayloadView,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CalibrationUpdatePreviewRequest {
    target: AuthTarget,
    torque_json: String,
    friction_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationUpdatePreviewView {
    pub preview_id: String,
    pub node_id: u8,
    pub identity: Identity,
    pub token_decimal: String,
    pub token_hex: String,
    pub torque_source: CalibrationSourceView,
    pub friction_source: Option<CalibrationSourceView>,
    pub requested: CalibrationPayloadView,
    pub quantized: CalibrationPayloadView,
    pub new_words: Vec<RawWordView>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CalibrationUpdateWriteRequest {
    target: AuthTarget,
    preview_id: String,
    backup_acknowledged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationUpdateWriteView {
    pub node_id: u8,
    pub identity: Identity,
    pub preview_id: String,
    pub written_words: Vec<RawWordView>,
    pub ram_readback_confirmed: bool,
    pub power_cycle_required: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CalibrationUpdateVerifyRequest {
    target: AuthTarget,
    preview_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalibrationUpdatePersistedView {
    pub node_id: u8,
    pub session_epoch: u64,
    pub identity: Identity,
    pub preview_id: String,
    pub online_status: String,
    pub persisted_words: Vec<RawWordView>,
}

#[derive(Debug, Clone)]
struct CalibrationDraft {
    preview_id: String,
    words: [u32; 7],
    torque_source: CalibrationSourceView,
    friction_source: Option<CalibrationSourceView>,
    requested: CalibrationPayloadView,
    quantized: CalibrationPayloadView,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct CalibrationSession {
    target: AuthTarget,
    identity: Identity,
    online_status: String,
    backup_words: Vec<u32>,
    token: u64,
    draft: Option<CalibrationDraft>,
    write_completed: bool,
}

#[derive(Default)]
pub struct CalibrationUpdateState {
    session: Mutex<Option<CalibrationSession>>,
}

#[tauri::command]
pub async fn calibration_update_prepare(
    state: State<'_, AppState>,
    target: AuthTarget,
) -> Result<CalibrationUpdatePreparedView, String> {
    let (identity, backup_words) = {
        let _operation = state.device_settings_operation.acquire().await;
        let (identity, bus) = require_single_live_motor(&state, target).await?;
        let (_, words) = read_stable_record(&bus, target.node_id).await?;
        (identity, words)
    };
    let first_seven = first_seven(&backup_words)?;
    let factory_words = FactoryWords(first_seven);
    validate_meow_v1(identity, &factory_words)?;
    let token = token_from_words(first_seven);
    let online_status = verify_meow_words_online(identity, factory_words).await?;
    require_accepted_status(&online_status)?;

    // Do not retain a proof from a motor that disappeared or changed while the
    // HTTPS request was in flight.
    let current = target_identity(&state, target).await?;
    if current != identity {
        return Err("motor identity/session changed during online verification".into());
    }

    let session = CalibrationSession {
        target,
        identity,
        online_status,
        backup_words,
        token,
        draft: None,
        write_completed: false,
    };
    let view = prepared_view(&session)?;
    *state.calibration_update.session.lock().await = Some(session);
    Ok(view)
}

#[tauri::command]
pub async fn calibration_update_preview(
    state: State<'_, AppState>,
    request: CalibrationUpdatePreviewRequest,
) -> Result<CalibrationUpdatePreviewView, String> {
    let mut guard = state.calibration_update.session.lock().await;
    let session = matching_session_mut(&mut guard, request.target)?;
    if session.write_completed {
        return Err(
            "this motor was already written; verify it after a full power cycle or prepare again"
                .into(),
        );
    }

    let torque = parse_torque_result(&request.torque_json)?;
    require_source_product(torque.source, session.identity, "torque")?;
    let friction = match request.friction_json.as_deref() {
        Some(json) if !json.trim().is_empty() => {
            let parsed = parse_friction_result(json)?;
            require_source_product(parsed.source, session.identity, "friction")?;
            Some(parsed)
        }
        _ => None,
    };

    let requested = CalibrationPayloadView {
        torque_factor: torque.factor,
        torque_fit_rmse_nm: torque.rmse,
        friction: friction.as_ref().map(|result| result.values.clone()),
    };
    let words = build_words(session.identity, session.token, &requested)?;
    let quantized = decode_payload(words)?;
    let mut warnings = Vec::new();
    if torque.source.serial_number != session.identity.serial_number {
        warnings.push(
            "torque calibration comes from another motor and will be applied as a batch sample"
                .into(),
        );
    }
    if let Some(result) = &friction {
        if result.source.serial_number != session.identity.serial_number {
            warnings.push("friction calibration comes from another motor and will be applied as a batch sample".into());
        }
        if result.values.static_pos_raw_nm < result.values.kinetic_pos_raw_nm
            || result.values.static_neg_raw_nm < result.values.kinetic_neg_raw_nm
        {
            warnings.push("a static-friction magnitude is below its kinetic magnitude; this is wire-valid but should be reviewed".into());
        }
    } else {
        warnings.push(
            "friction calibration is explicitly absent; 0x4001:05..07 will be written as zero"
                .into(),
        );
    }

    let preview_id = preview_digest(
        session,
        &request.torque_json,
        request.friction_json.as_deref(),
        words,
    );
    let draft = CalibrationDraft {
        preview_id,
        words,
        torque_source: torque.source,
        friction_source: friction.as_ref().map(|result| result.source),
        requested,
        quantized,
        warnings,
    };
    let view = preview_view(session, &draft);
    session.draft = Some(draft);
    Ok(view)
}

#[tauri::command]
pub async fn calibration_update_write(
    state: State<'_, AppState>,
    request: CalibrationUpdateWriteRequest,
) -> Result<CalibrationUpdateWriteView, String> {
    if !request.backup_acknowledged {
        return Err("save the complete raw backup and acknowledge it before writing".into());
    }
    let session = {
        let guard = state.calibration_update.session.lock().await;
        let session = matching_session(&guard, request.target)?;
        let draft = session
            .draft
            .as_ref()
            .ok_or_else(|| "preview the calibration result before writing".to_owned())?;
        if draft.preview_id != request.preview_id {
            return Err("the confirmed preview is stale; preview the current JSON again".into());
        }
        if session.write_completed {
            return Err("this preview was already written".into());
        }
        session.clone()
    };
    let draft = session.draft.as_ref().expect("checked draft").clone();

    let _operation = state.device_settings_operation.acquire().await;
    let (identity, bus) = require_single_live_motor(&state, request.target).await?;
    if identity != session.identity {
        return Err("the attached motor identity changed after preparation".into());
    }
    let (_, live_backup) = read_stable_record(&bus, request.target.node_id).await?;
    if live_backup != session.backup_words {
        return Err("0x4001 changed after backup/verification; prepare this motor again".into());
    }

    // The Motor Control App caches this motor's torque factor per heartbeat
    // session. Drop it before the first mutation so no later command can scale
    // a target with the pre-rewrite factor, whatever happens to this write.
    state
        .meow_calibration
        .forget_node(request.target.node_id)
        .await;

    // A valid rewrite uses three durable phases. Any interruption before the
    // last save leaves an invalid manifest rather than a mixed valid record.
    write_u32(&bus, request.target.node_id, 0x4001, 1, 0)
        .await
        .map_err(|error| mutation_error("invalidating 0x4001:01", error))?;
    require_u32(&bus, request.target.node_id, 0x4001, 1, 0)
        .await
        .map_err(|error| mutation_error("reading back the invalid manifest", error))?;
    save_manufacturer_parameters(&bus, request.target.node_id)
        .await
        .map_err(|error| mutation_error("saving the invalid manifest", error))?;

    for (offset, word) in draft.words[1..].iter().enumerate() {
        write_u32(
            &bus,
            request.target.node_id,
            0x4001,
            (offset + 2) as u8,
            *word,
        )
        .await
        .map_err(|error| mutation_error("writing 0x4001 payload/token", error))?;
    }
    for (offset, word) in draft.words[1..].iter().enumerate() {
        require_u32(
            &bus,
            request.target.node_id,
            0x4001,
            (offset + 2) as u8,
            *word,
        )
        .await
        .map_err(|error| mutation_error("reading back 0x4001 payload/token", error))?;
    }
    save_manufacturer_parameters(&bus, request.target.node_id)
        .await
        .map_err(|error| mutation_error("saving the new payload/token", error))?;

    write_u32(&bus, request.target.node_id, 0x4001, 1, draft.words[0])
        .await
        .map_err(|error| mutation_error("committing the new manifest/CRC", error))?;
    require_u32(&bus, request.target.node_id, 0x4001, 1, draft.words[0])
        .await
        .map_err(|error| mutation_error("reading back the committed manifest/CRC", error))?;
    save_manufacturer_parameters(&bus, request.target.node_id)
        .await
        .map_err(|error| mutation_error("saving the committed manifest/CRC", error))?;

    let (_, readback) = read_stable_record(&bus, request.target.node_id).await?;
    let mut expected = session.backup_words.clone();
    expected[..7].copy_from_slice(&draft.words);
    if readback != expected {
        return Err("post-save RAM readback differs from the preview; keep power on and restore the saved backup".into());
    }
    validate_meow_v1(identity, &FactoryWords(draft.words))?;

    {
        let mut guard = state.calibration_update.session.lock().await;
        let live_session = matching_session_mut(&mut guard, request.target)?;
        let live_draft = live_session
            .draft
            .as_ref()
            .ok_or_else(|| "calibration preview disappeared".to_owned())?;
        if live_draft.preview_id != request.preview_id {
            return Err("calibration preview changed during the write".into());
        }
        live_session.write_completed = true;
    }

    Ok(CalibrationUpdateWriteView {
        node_id: request.target.node_id,
        identity,
        preview_id: draft.preview_id,
        written_words: raw_word_views(&draft.words),
        ram_readback_confirmed: true,
        power_cycle_required: true,
    })
}

#[tauri::command]
pub async fn calibration_update_verify_persisted(
    state: State<'_, AppState>,
    request: CalibrationUpdateVerifyRequest,
) -> Result<CalibrationUpdatePersistedView, String> {
    let session = {
        let guard = state.calibration_update.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| "no prepared calibration update remains in memory".to_owned())?;
        let draft = session
            .draft
            .as_ref()
            .ok_or_else(|| "no written calibration preview remains in memory".to_owned())?;
        if !session.write_completed || draft.preview_id != request.preview_id {
            return Err("the requested preview has not completed its write".into());
        }
        if request.target.session_epoch == session.target.session_epoch {
            return Err("no new heartbeat session was observed; fully power-cycle the motor while keeping this CAN connection open".into());
        }
        session.clone()
    };
    let draft = session.draft.as_ref().expect("checked draft").clone();

    let (identity, readback) = {
        let _operation = state.device_settings_operation.acquire().await;
        let (identity, bus) = require_single_live_motor(&state, request.target).await?;
        if identity.vendor_id != session.identity.vendor_id
            || identity.product_code != session.identity.product_code
            || identity.serial_number != session.identity.serial_number
        {
            return Err("the power-cycled motor does not match the prepared identity".into());
        }
        let (_, readback) = read_stable_record(&bus, request.target.node_id).await?;
        (identity, readback)
    };
    let mut expected = session.backup_words.clone();
    expected[..7].copy_from_slice(&draft.words);
    if readback != expected {
        return Err(
            "persisted 0x4001 readback differs after power cycle; restore the saved backup".into(),
        );
    }
    let words = FactoryWords(draft.words);
    validate_meow_v1(identity, &words)?;
    let online_status = verify_meow_words_online(identity, words).await?;
    require_accepted_status(&online_status)?;

    Ok(CalibrationUpdatePersistedView {
        node_id: request.target.node_id,
        session_epoch: request.target.session_epoch,
        identity,
        preview_id: draft.preview_id,
        online_status,
        persisted_words: raw_word_views(&draft.words),
    })
}

async fn require_single_live_motor(
    state: &AppState,
    target: AuthTarget,
) -> Result<(Identity, Arc<dyn CanBus>), String> {
    let expected = target_identity(state, target).await?;
    if device_registry::classify(expected.vendor_id, expected.product_code) != DeviceKind::MeowMotor
    {
        return Err("calibration updates support only known Meow Motors".into());
    }
    let manager = state
        .manager()
        .await
        .ok_or_else(|| "CAN manager is not connected".to_owned())?;
    let meow_count = manager
        .list()
        .into_iter()
        .filter(|device| {
            device.online
                && device.identity.as_ref().is_some_and(|identity| {
                    device_registry::classify(identity.vendor_id, identity.product_code)
                        == DeviceKind::MeowMotor
                })
        })
        .count();
    if meow_count != 1 {
        return Err(format!(
            "connect exactly one online Meow Motor before updating calibration; found {meow_count}"
        ));
    }
    let bus = state
        .calibration_bus()
        .await
        .ok_or_else(|| "CAN transport is not connected".to_owned())?;
    let identity = read_identity(&bus, target.node_id).await?;
    if identity != expected {
        return Err("complete 0x1018 changed after heartbeat discovery".into());
    }
    Ok((identity, bus))
}

async fn read_stable_record(bus: &Arc<dyn CanBus>, node_id: u8) -> Result<(u8, Vec<u32>), String> {
    let highest = read_exact::<1>(bus, node_id, 0x4001, 0).await?[0];
    if highest < 7 {
        return Err(format!(
            "0x4001:00 is {highest}; calibration format v1 requires at least 7"
        ));
    }
    for _ in 0..RECORD_READ_ATTEMPTS {
        let manifest_before = read_u32(bus, node_id, 0x4001, 1).await?;
        let mut words = Vec::with_capacity(usize::from(highest));
        words.push(manifest_before);
        for subindex in 2..=highest {
            words.push(read_u32(bus, node_id, 0x4001, subindex).await?);
        }
        let manifest_after = read_u32(bus, node_id, 0x4001, 1).await?;
        if manifest_before == manifest_after {
            return Ok((highest, words));
        }
    }
    Err("0x4001:01 changed repeatedly while reading; record is unstable".into())
}

async fn read_exact<const N: usize>(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    index: u16,
    subindex: u8,
) -> Result<[u8; N], String> {
    let bytes = upload_bytes_retry(&**bus, node_id, index, subindex, Some(SDO_TIMEOUT), 2)
        .await
        .map_err(|error| format!("SDO read 0x{index:04X}:{subindex:02X}: {error}"))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        format!(
            "0x{index:04X}:{subindex:02X} returned {} bytes; expected {N}",
            bytes.len()
        )
    })
}

async fn read_u32(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    index: u16,
    subindex: u8,
) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        read_exact::<4>(bus, node_id, index, subindex).await?,
    ))
}

async fn write_u32(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    index: u16,
    subindex: u8,
    value: u32,
) -> Result<(), String> {
    download_bytes_retry(
        &**bus,
        node_id,
        index,
        subindex,
        &value.to_le_bytes(),
        Some(SDO_TIMEOUT),
        1,
    )
    .await
    .map_err(|error| format!("SDO write 0x{index:04X}:{subindex:02X}: {error}"))
}

async fn require_u32(
    bus: &Arc<dyn CanBus>,
    node_id: u8,
    index: u16,
    subindex: u8,
    expected: u32,
) -> Result<(), String> {
    let observed = read_u32(bus, node_id, index, subindex).await?;
    if observed != expected {
        return Err(format!(
            "0x{index:04X}:{subindex:02X} readback mismatch: expected 0x{expected:08X}, got 0x{observed:08X}"
        ));
    }
    Ok(())
}

async fn save_manufacturer_parameters(bus: &Arc<dyn CanBus>, node_id: u8) -> Result<(), String> {
    write_u32(bus, node_id, 0x1010, 4, STORE_PARAMETERS_SIGNATURE).await?;
    tokio::time::sleep(SAVE_SETTLE).await;
    Ok(())
}

fn mutation_error(stage: &str, error: String) -> String {
    format!(
        "{stage} failed: {error}. The on-device record may now be intentionally invalid or partially updated; keep power on and use the saved full backup for attended recovery"
    )
}

fn require_accepted_status(status: &str) -> Result<(), String> {
    if matches!(status, "issued_unregistered" | "registered") {
        Ok(())
    } else {
        Err(format!(
            "motor is not eligible for user recalibration; online authenticity status is {status}"
        ))
    }
}

fn first_seven(words: &[u32]) -> Result<[u32; 7], String> {
    words
        .get(..7)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| "complete 0x4001:01..07 is unavailable".into())
}

fn token_from_words(words: [u32; 7]) -> u64 {
    let mut bytes = [0_u8; 8];
    bytes[..4].copy_from_slice(&words[1].to_le_bytes());
    bytes[4..].copy_from_slice(&words[2].to_le_bytes());
    u64::from_le_bytes(bytes)
}

fn prepared_view(session: &CalibrationSession) -> Result<CalibrationUpdatePreparedView, String> {
    let words = first_seven(&session.backup_words)?;
    Ok(CalibrationUpdatePreparedView {
        node_id: session.target.node_id,
        session_epoch: session.target.session_epoch,
        identity: session.identity,
        online_status: session.online_status.clone(),
        token_decimal: session.token.to_string(),
        token_hex: format!("0x{:016X}", session.token),
        highest_subindex: session.backup_words.len() as u8,
        backup_words: raw_word_views(&session.backup_words),
        current_calibration: decode_payload(words)?,
    })
}

fn preview_view(
    session: &CalibrationSession,
    draft: &CalibrationDraft,
) -> CalibrationUpdatePreviewView {
    CalibrationUpdatePreviewView {
        preview_id: draft.preview_id.clone(),
        node_id: session.target.node_id,
        identity: session.identity,
        token_decimal: session.token.to_string(),
        token_hex: format!("0x{:016X}", session.token),
        torque_source: draft.torque_source,
        friction_source: draft.friction_source,
        requested: draft.requested.clone(),
        quantized: draft.quantized.clone(),
        new_words: raw_word_views(&draft.words),
        warnings: draft.warnings.clone(),
    }
}

fn raw_word_views(words: &[u32]) -> Vec<RawWordView> {
    words
        .iter()
        .enumerate()
        .map(|(index, value)| RawWordView {
            subindex: (index + 1) as u8,
            value_u32: *value,
            value_hex: format!("0x{value:08X}"),
        })
        .collect()
}

fn matching_session(
    guard: &Option<CalibrationSession>,
    target: AuthTarget,
) -> Result<&CalibrationSession, String> {
    let session = guard
        .as_ref()
        .ok_or_else(|| "read, back up and verify one motor first".to_owned())?;
    if session.target.node_id != target.node_id
        || session.target.session_epoch != target.session_epoch
    {
        return Err("the selected motor heartbeat session differs from the prepared motor".into());
    }
    Ok(session)
}

fn matching_session_mut(
    guard: &mut Option<CalibrationSession>,
    target: AuthTarget,
) -> Result<&mut CalibrationSession, String> {
    let session = guard
        .as_mut()
        .ok_or_else(|| "read, back up and verify one motor first".to_owned())?;
    if session.target.node_id != target.node_id
        || session.target.session_epoch != target.session_epoch
    {
        return Err("the selected motor heartbeat session differs from the prepared motor".into());
    }
    Ok(session)
}

#[derive(Debug)]
struct TorqueResult {
    source: CalibrationSourceView,
    factor: f64,
    rmse: f64,
}

#[derive(Debug)]
struct FrictionResult {
    source: CalibrationSourceView,
    values: FrictionPayloadView,
}

fn parse_json(json: &str, kind: &str) -> Result<Value, String> {
    if json.len() > MAX_RESULT_JSON_BYTES {
        return Err(format!(
            "{kind} result JSON exceeds {MAX_RESULT_JSON_BYTES} bytes"
        ));
    }
    serde_json::from_str(json).map_err(|error| format!("invalid {kind} result JSON: {error}"))
}

fn parse_torque_result(json: &str) -> Result<TorqueResult, String> {
    let value = parse_json(json, "torque")?;
    require_string(
        &value,
        "schema",
        "hex-meow/gravity-torque-calibration-result/v3",
    )?;
    require_string(
        &value,
        "equation",
        "raw_command_nm = desired_physical_torque_nm * torque_factor",
    )?;
    Ok(TorqueResult {
        source: parse_source(&value)?,
        factor: field_f64(&value, "torque_factor")?,
        rmse: field_f64(&value, "torque_fit_rmse_nm")?,
    })
}

fn parse_friction_result(json: &str) -> Result<FrictionResult, String> {
    let value = parse_json(json, "friction")?;
    require_string(&value, "schema", "hex-meow/friction-calibration-result/v1")?;
    require_string(
        &value,
        "semantics",
        "raw_command_domain_before_torque_factor",
    )?;
    Ok(FrictionResult {
        source: parse_source(&value)?,
        values: FrictionPayloadView {
            static_pos_raw_nm: field_f64(&value, "static_pos_raw_nm")?,
            static_neg_raw_nm: field_f64(&value, "static_neg_raw_nm")?,
            kinetic_pos_raw_nm: field_f64(&value, "kinetic_pos_raw_nm")?,
            kinetic_neg_raw_nm: field_f64(&value, "kinetic_neg_raw_nm")?,
            reference_speed_rad_per_s: field_f64(&value, "kinetic_reference_speed_rad_per_s")?,
            calibration_temperature_c: field_f64(&value, "calibration_temperature_c")?,
        },
    })
}

fn parse_source(value: &Value) -> Result<CalibrationSourceView, String> {
    Ok(CalibrationSourceView {
        vendor_id: field_u32(value, "vendor_id")?,
        product_code: field_u32(value, "product_code")?,
        revision_number: field_u32(value, "revision_number")?,
        serial_number: field_u32(value, "serial_number")?,
    })
}

fn field_u32(value: &Value, name: &str) -> Result<u32, String> {
    let number = value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("result field {name} must be a u32"))?;
    u32::try_from(number).map_err(|_| format!("result field {name} exceeds u32"))
}

fn field_f64(value: &Value, name: &str) -> Result<f64, String> {
    let number = value
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("result field {name} must be a number"))?;
    if !number.is_finite() {
        return Err(format!("result field {name} must be finite"));
    }
    Ok(number)
}

fn require_string(value: &Value, name: &str, expected: &str) -> Result<(), String> {
    match value.get(name).and_then(Value::as_str) {
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(format!(
            "result field {name} is {actual:?}; expected {expected:?}"
        )),
        None => Err(format!("result field {name} must be a string")),
    }
}

fn require_source_product(
    source: CalibrationSourceView,
    target: Identity,
    kind: &str,
) -> Result<(), String> {
    if source.vendor_id != target.vendor_id || source.product_code != target.product_code {
        return Err(format!(
            "{kind} result is for vendor/product 0x{:08X}/0x{:08X}, but the target is 0x{:08X}/0x{:08X}",
            source.vendor_id, source.product_code, target.vendor_id, target.product_code
        ));
    }
    Ok(())
}

fn build_words(
    identity: Identity,
    token: u64,
    calibration: &CalibrationPayloadView,
) -> Result<[u32; 7], String> {
    if token == 0 {
        return Err("issuance token is zero".into());
    }
    let sub04 = pack_pair(
        encode_e4m11(calibration.torque_factor)?,
        encode_e4m11(calibration.torque_fit_rmse_nm)?,
    );
    let (sub05, sub06, sub07) = match &calibration.friction {
        Some(friction) => (
            pack_pair(
                encode_e4m11(friction.static_pos_raw_nm)?,
                encode_e4m11(friction.static_neg_raw_nm)?,
            ),
            pack_pair(
                encode_e4m11(friction.kinetic_pos_raw_nm)?,
                encode_e4m11(friction.kinetic_neg_raw_nm)?,
            ),
            pack_pair(
                encode_e4m11(friction.reference_speed_rad_per_s)?,
                encode_e4m11(friction.calibration_temperature_c)?,
            ),
        ),
        None => (0, 0, 0),
    };
    let token_bytes = token.to_le_bytes();
    let mut words = FactoryWords([
        u32::from(MANIFEST_UPPER_V1) << 16,
        u32::from_le_bytes(token_bytes[..4].try_into().expect("four token bytes")),
        u32::from_le_bytes(token_bytes[4..].try_into().expect("four token bytes")),
        sub04,
        sub05,
        sub06,
        sub07,
    ]);
    words.0[0] |= u32::from(meow_crc(identity, &words));
    validate_meow_v1(identity, &words)?;
    Ok(words.0)
}

fn pack_pair(low: u16, high: u16) -> u32 {
    u32::from(low) | (u32::from(high) << 16)
}

fn decode_payload(words: [u32; 7]) -> Result<CalibrationPayloadView, String> {
    let friction = if words[4] == 0 && words[5] == 0 && words[6] == 0 {
        None
    } else {
        Some(FrictionPayloadView {
            static_pos_raw_nm: decode_e4m11(words[4] as u16)?,
            static_neg_raw_nm: decode_e4m11((words[4] >> 16) as u16)?,
            kinetic_pos_raw_nm: decode_e4m11(words[5] as u16)?,
            kinetic_neg_raw_nm: decode_e4m11((words[5] >> 16) as u16)?,
            reference_speed_rad_per_s: decode_e4m11(words[6] as u16)?,
            calibration_temperature_c: decode_e4m11((words[6] >> 16) as u16)?,
        })
    };
    Ok(CalibrationPayloadView {
        torque_factor: decode_e4m11(words[3] as u16)?,
        torque_fit_rmse_nm: decode_e4m11((words[3] >> 16) as u16)?,
        friction,
    })
}

fn preview_digest(
    session: &CalibrationSession,
    torque_json: &str,
    friction_json: Option<&str>,
    words: [u32; 7],
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"hex-meow/calibration-update-preview/v1\0");
    hash.update(session.identity.vendor_id.to_le_bytes());
    hash.update(session.identity.product_code.to_le_bytes());
    hash.update(session.identity.serial_number.to_le_bytes());
    for word in &session.backup_words {
        hash.update(word.to_le_bytes());
    }
    for word in words {
        hash.update(word.to_le_bytes());
    }
    hash.update((torque_json.len() as u64).to_le_bytes());
    hash.update(torque_json.as_bytes());
    if let Some(json) = friction_json {
        hash.update((json.len() as u64).to_le_bytes());
        hash.update(json.as_bytes());
    } else {
        hash.update(0_u64.to_le_bytes());
    }
    hex::encode(hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY: Identity = Identity {
        vendor_id: 0x0068_6578,
        product_code: 0x6c64_bc78,
        revision_number: 0x0102_0304,
        serial_number: 0x1234_5678,
    };

    fn torque_json() -> String {
        serde_json::json!({
            "schema": "hex-meow/gravity-torque-calibration-result/v3",
            "equation": "raw_command_nm = desired_physical_torque_nm * torque_factor",
            "vendor_id": IDENTITY.vendor_id,
            "product_code": IDENTITY.product_code,
            "revision_number": IDENTITY.revision_number,
            "serial_number": 9,
            "torque_factor": 0.85,
            "torque_fit_rmse_nm": 0.1,
            "ignored_diagnostic": [1, 2, 3]
        })
        .to_string()
    }

    fn friction_json() -> String {
        serde_json::json!({
            "schema": "hex-meow/friction-calibration-result/v1",
            "semantics": "raw_command_domain_before_torque_factor",
            "vendor_id": IDENTITY.vendor_id,
            "product_code": IDENTITY.product_code,
            "revision_number": IDENTITY.revision_number,
            "serial_number": 10,
            "static_pos_raw_nm": 1.0,
            "static_neg_raw_nm": 2.0,
            "kinetic_pos_raw_nm": 0.5,
            "kinetic_neg_raw_nm": 0.1,
            "kinetic_reference_speed_rad_per_s": 1.0,
            "calibration_temperature_c": 25.0
        })
        .to_string()
    }

    #[test]
    fn copied_calibration_results_reproduce_the_frozen_words() {
        let torque = parse_torque_result(&torque_json()).unwrap();
        let friction = parse_friction_result(&friction_json()).unwrap();
        let values = CalibrationPayloadView {
            torque_factor: torque.factor,
            torque_fit_rmse_nm: torque.rmse,
            friction: Some(friction.values),
        };
        let token = 0x0fb6_4b09_4643_12d5;
        let words = build_words(IDENTITY, token, &values).unwrap();
        assert_eq!(
            words,
            [
                0x0106_f6f9,
                0x4643_12d5,
                0x0fb6_4b09,
                0x1ccd_359a,
                0x4000_3800,
                0x1ccd_3000,
                0x5c80_3800,
            ]
        );
        assert_eq!(token_from_words(words), token);
    }

    #[test]
    fn absent_friction_is_canonical_and_bad_sources_are_rejected() {
        let torque = parse_torque_result(&torque_json()).unwrap();
        let values = CalibrationPayloadView {
            torque_factor: torque.factor,
            torque_fit_rmse_nm: torque.rmse,
            friction: None,
        };
        let words = build_words(IDENTITY, 7, &values).unwrap();
        assert_eq!(&words[4..], &[0, 0, 0]);
        assert!(require_source_product(torque.source, IDENTITY, "torque").is_ok());

        let mut wrong = serde_json::from_str::<Value>(&torque_json()).unwrap();
        wrong["product_code"] = Value::from(1);
        let parsed = parse_torque_result(&wrong.to_string()).unwrap();
        assert!(require_source_product(parsed.source, IDENTITY, "torque").is_err());
    }

    #[test]
    fn schemas_and_required_numeric_fields_are_strict() {
        let mut wrong = serde_json::from_str::<Value>(&torque_json()).unwrap();
        wrong["schema"] = Value::from("wrong");
        assert!(parse_torque_result(&wrong.to_string()).is_err());

        let mut partial = serde_json::from_str::<Value>(&friction_json()).unwrap();
        partial["kinetic_pos_raw_nm"] = Value::from(0.0);
        let parsed = parse_friction_result(&partial.to_string()).unwrap();
        let values = CalibrationPayloadView {
            torque_factor: 1.0,
            torque_fit_rmse_nm: 0.0,
            friction: Some(parsed.values),
        };
        assert!(build_words(IDENTITY, 7, &values).is_err());
    }
}
