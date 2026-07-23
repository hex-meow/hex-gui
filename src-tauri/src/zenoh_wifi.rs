//! Robot Console Wi-Fi client.
//!
//! Reuses the console's single Zenoh session. The wire format is the public
//! hex-wifi JSON protocol; passphrases are write-only and never appear in a
//! response DTO or log message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const PROTOCOL_VERSION: u16 = 1;
const QUERY_TIMEOUT: Duration = Duration::from_secs(6);
const SCAN_TIMEOUT: Duration = Duration::from_secs(25);
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(3);
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize, Clone)]
pub struct WifiSsidDto {
    pub hex: String,
    pub display: String,
}

#[derive(Serialize, Clone)]
pub struct WifiStatusDto {
    pub state: String,
    pub connected: Option<WifiSsidDto>,
    pub revision: u64,
}

#[derive(Serialize, Clone)]
pub struct WifiControllerDto {
    pub cid: String,
    pub status: WifiStatusDto,
}

#[derive(Serialize, Clone)]
pub struct WifiScanEntryDto {
    pub ssid: WifiSsidDto,
    pub signal_dbm: i16,
    pub security: String,
}

#[derive(Serialize, Clone)]
pub struct WifiSavedNetworkDto {
    pub ssid: WifiSsidDto,
    pub enabled: bool,
    pub connected: bool,
}

#[derive(Serialize, Clone)]
pub struct WifiJobDto {
    pub job_id: String,
    pub request_id: String,
    pub operation: String,
    pub state: String,
    pub revision: Option<u64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Deserialize)]
struct WireResponse {
    version: u16,
    request_id: String,
    #[serde(flatten)]
    outcome: WireOutcome,
}

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum WireOutcome {
    Ok { reply: WireReply },
    Error { error: WireError },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireReply {
    Status { status: WireStatus },
    Scan { networks: Vec<WireScanEntry> },
    Networks { networks: Vec<WireSavedNetwork> },
    Validated,
    Accepted { revision: u64 },
    JobAccepted { job: WireJob },
    Job { job: WireJob },
}

#[derive(Deserialize)]
struct WireStatus {
    state: String,
    connected: Option<String>,
    revision: u64,
}

#[derive(Deserialize)]
struct WireScanEntry {
    ssid: String,
    signal_dbm: i16,
    security: String,
}

#[derive(Deserialize)]
struct WireSavedNetwork {
    ssid: String,
    enabled: bool,
    connected: bool,
}

#[derive(Deserialize)]
struct WireJob {
    job_id: String,
    request_id: String,
    operation: String,
    state: String,
    revision: Option<u64>,
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
    message: String,
}

fn request(operation: Value) -> (String, Vec<u8>) {
    let request_id = format!(
        "gui-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let mut value = operation;
    let object = value.as_object_mut().expect("Wi-Fi operation is an object");
    object.insert("version".into(), json!(PROTOCOL_VERSION));
    object.insert("request_id".into(), json!(request_id));
    (
        request_id,
        serde_json::to_vec(&value).expect("Wi-Fi request serializes"),
    )
}

async fn query_one(
    session: &zenoh::Session,
    key: &str,
    operation: Value,
    timeout: Duration,
    write: bool,
) -> anyhow::Result<WireReply> {
    let (request_id, payload) = request(operation);
    let replies = session
        .get(key)
        .payload(payload)
        .timeout(timeout)
        .await
        .map_err(|error| anyhow!("Zenoh query {key}: {error}"))?;
    let reply = tokio::time::timeout(timeout + Duration::from_secs(1), replies.recv_async())
        .await
        .map_err(|_| {
            if write {
                anyhow!("Wi-Fi 写操作无回复；请通过控制器有线 end0 地址连接后重试")
            } else {
                anyhow!("Wi-Fi query 超时: {key}")
            }
        })?
        .map_err(|error| {
            if write {
                anyhow!("Wi-Fi 写操作无回复；请通过控制器有线 end0 地址连接后重试: {error}")
            } else {
                anyhow!("Wi-Fi query 无回复: {error}")
            }
        })?;
    let sample = reply
        .result()
        .map_err(|error| anyhow!("Wi-Fi query error: {error}"))?;
    decode_response(&sample.payload().to_bytes(), &request_id)
}

fn decode_response(bytes: &[u8], expected_request_id: &str) -> anyhow::Result<WireReply> {
    let response: WireResponse =
        serde_json::from_slice(bytes).context("invalid hex-wifi response")?;
    if response.version != PROTOCOL_VERSION {
        return Err(anyhow!(
            "unsupported hex-wifi protocol version {}",
            response.version
        ));
    }
    if response.request_id != expected_request_id {
        return Err(anyhow!("hex-wifi response request_id mismatch"));
    }
    match response.outcome {
        WireOutcome::Ok { reply } => Ok(reply),
        WireOutcome::Error { error } => Err(anyhow!("{}: {}", error.code, error.message)),
    }
}

fn ssid(hex_value: String) -> anyhow::Result<WifiSsidDto> {
    let bytes = hex::decode(&hex_value).context("invalid SSID hex in hex-wifi response")?;
    Ok(WifiSsidDto {
        hex: hex_value,
        display: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

fn status_dto(status: WireStatus) -> anyhow::Result<WifiStatusDto> {
    Ok(WifiStatusDto {
        state: status.state,
        connected: status.connected.map(ssid).transpose()?,
        revision: status.revision,
    })
}

fn job_dto(job: WireJob) -> WifiJobDto {
    let (error_code, error_message) = match job.error {
        Some(error) => (Some(error.code), Some(error.message)),
        None => (None, None),
    };
    WifiJobDto {
        job_id: job.job_id,
        request_id: job.request_id,
        operation: job.operation,
        state: job.state,
        revision: job.revision,
        error_code,
        error_message,
    }
}

pub async fn discover(session: &zenoh::Session) -> anyhow::Result<Vec<WifiControllerDto>> {
    let (request_id, payload) = request(json!({ "op": "status" }));
    let replies = session
        .get("hexmeow/*/wifi/status")
        .payload(payload)
        .timeout(DISCOVER_TIMEOUT)
        .await
        .map_err(|error| anyhow!("Wi-Fi discovery: {error}"))?;
    let mut controllers = Vec::new();
    loop {
        let reply = match tokio::time::timeout(DISCOVER_TIMEOUT, replies.recv_async()).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) | Err(_) => break,
        };
        let Ok(sample) = reply.result() else {
            continue;
        };
        let key = sample.key_expr().as_str();
        let Some(cid) = key.strip_suffix("/wifi/status") else {
            continue;
        };
        let Ok(WireReply::Status { status }) =
            decode_response(&sample.payload().to_bytes(), &request_id)
        else {
            continue;
        };
        controllers.push(WifiControllerDto {
            cid: cid.to_owned(),
            status: status_dto(status)?,
        });
    }
    controllers.sort_by(|left, right| left.cid.cmp(&right.cid));
    controllers.dedup_by(|left, right| left.cid == right.cid);
    Ok(controllers)
}

pub async fn status(session: &zenoh::Session, cid: &str) -> anyhow::Result<WifiStatusDto> {
    match query_one(
        session,
        &format!("{cid}/wifi/status"),
        json!({ "op": "status" }),
        QUERY_TIMEOUT,
        false,
    )
    .await?
    {
        WireReply::Status { status } => status_dto(status),
        _ => Err(anyhow!("unexpected status response")),
    }
}

pub async fn scan(session: &zenoh::Session, cid: &str) -> anyhow::Result<Vec<WifiScanEntryDto>> {
    match query_one(
        session,
        &format!("{cid}/wifi/scan"),
        json!({ "op": "scan" }),
        SCAN_TIMEOUT,
        false,
    )
    .await?
    {
        WireReply::Scan { networks } => networks
            .into_iter()
            .map(|network| {
                Ok(WifiScanEntryDto {
                    ssid: ssid(network.ssid)?,
                    signal_dbm: network.signal_dbm,
                    security: network.security,
                })
            })
            .collect(),
        _ => Err(anyhow!("unexpected scan response")),
    }
}

pub async fn networks(
    session: &zenoh::Session,
    cid: &str,
) -> anyhow::Result<Vec<WifiSavedNetworkDto>> {
    match query_one(
        session,
        &format!("{cid}/wifi/networks"),
        json!({ "op": "networks" }),
        QUERY_TIMEOUT,
        false,
    )
    .await?
    {
        WireReply::Networks { networks } => networks
            .into_iter()
            .map(|network| {
                Ok(WifiSavedNetworkDto {
                    ssid: ssid(network.ssid)?,
                    enabled: network.enabled,
                    connected: network.connected,
                })
            })
            .collect(),
        _ => Err(anyhow!("unexpected networks response")),
    }
}

fn profile(ssid: &str, passphrase: String, hidden: bool, country: Option<String>) -> Value {
    json!({
        "ssid": hex::encode(ssid.as_bytes()),
        "passphrase": passphrase,
        "hidden": hidden,
        "country": country,
    })
}

pub async fn validate(
    session: &zenoh::Session,
    cid: &str,
    ssid: &str,
    passphrase: String,
    hidden: bool,
    country: Option<String>,
) -> anyhow::Result<()> {
    let operation = json!({
        "op": "validate",
        "profile": profile(ssid, passphrase, hidden, country),
    });
    match query_one(
        session,
        &format!("{cid}/rpc/wifi/validate"),
        operation,
        QUERY_TIMEOUT,
        true,
    )
    .await?
    {
        WireReply::Validated => Ok(()),
        _ => Err(anyhow!("unexpected validate response")),
    }
}

pub async fn set(
    session: &zenoh::Session,
    cid: &str,
    ssid: &str,
    passphrase: String,
    hidden: bool,
    country: Option<String>,
    expected_revision: Option<u64>,
) -> anyhow::Result<WifiJobDto> {
    let operation = json!({
        "op": "set",
        "profile": profile(ssid, passphrase, hidden, country),
        "expected_revision": expected_revision,
    });
    accepted_job(
        query_one(
            session,
            &format!("{cid}/rpc/wifi/set"),
            operation,
            QUERY_TIMEOUT,
            true,
        )
        .await?,
    )
}

pub async fn forget(
    session: &zenoh::Session,
    cid: &str,
    ssid_hex: &str,
    expected_revision: Option<u64>,
) -> anyhow::Result<WifiJobDto> {
    let operation = json!({
        "op": "forget",
        "ssid": ssid_hex,
        "expected_revision": expected_revision,
    });
    accepted_job(
        query_one(
            session,
            &format!("{cid}/rpc/wifi/forget"),
            operation,
            QUERY_TIMEOUT,
            true,
        )
        .await?,
    )
}

pub async fn forget_all(
    session: &zenoh::Session,
    cid: &str,
    expected_revision: Option<u64>,
) -> anyhow::Result<WifiJobDto> {
    accepted_job(
        query_one(
            session,
            &format!("{cid}/rpc/wifi/forget_all"),
            json!({
                "op": "forget_all",
                "expected_revision": expected_revision,
            }),
            QUERY_TIMEOUT,
            true,
        )
        .await?,
    )
}

fn accepted_job(reply: WireReply) -> anyhow::Result<WifiJobDto> {
    match reply {
        WireReply::JobAccepted { job } => Ok(job_dto(job)),
        WireReply::Accepted { revision } => Err(anyhow!(
            "controller returned legacy synchronous revision {revision}"
        )),
        _ => Err(anyhow!("unexpected job acceptance response")),
    }
}

pub async fn job(session: &zenoh::Session, cid: &str, job_id: &str) -> anyhow::Result<WifiJobDto> {
    match query_one(
        session,
        &format!("{cid}/wifi/jobs/{job_id}"),
        json!({ "op": "job", "job_id": job_id }),
        QUERY_TIMEOUT,
        false,
    )
    .await?
    {
        WireReply::Job { job } | WireReply::JobAccepted { job } => Ok(job_dto(job)),
        _ => Err(anyhow!("unexpected job response")),
    }
}
