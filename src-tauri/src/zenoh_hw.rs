//! Robot Console 的 controller-HAL 只读观察面。
//!
//! `hw/info` 是资源清单，资源同名 liveliness 表示设备当前在不在，`hw/<id>` 数据流
//! 提供最新样本。这里刻意没有任何 put/RPC：GUI 只能观察，不能解除急停或切换电源。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use prost::Message;
use serde::Serialize;
use zenoh::query::{ConsolidationMode, QueryTarget};
use zenoh::sample::SampleKind;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/robot_api.rs"));
}

const INFO_REFRESH: Duration = Duration::from_secs(3);
const QUERY_TIMEOUT: Duration = Duration::from_millis(900);
const HW_KEY_EXPR: &str = "hexmeow/*/hw/*";
const HW_INFO_EXPR: &str = "hexmeow/*/hw/info";
const LAST_KNOWN_INFO_WARNING: &str =
    "hw/info currently unavailable; showing last-known resources and samples";

#[derive(Clone)]
struct CachedSample {
    payload: Vec<u8>,
    received_at: Instant,
}

#[derive(Clone, Default)]
struct CachedDiscovery {
    refreshed_at: Option<Instant>,
    controllers: Vec<HardwareControllerDto>,
    errors: Vec<String>,
}

/// Long-lived cache attached to RobotConsole's existing Zenoh session.
pub struct HardwareMonitor {
    samples: Arc<Mutex<HashMap<String, CachedSample>>>,
    alive: Arc<Mutex<HashSet<String>>>,
    runtime_errors: Arc<Mutex<BTreeMap<String, String>>>,
    discovery: Mutex<CachedDiscovery>,
}

#[derive(Serialize, Clone, Default)]
pub struct HardwareSnapshotDto {
    pub controllers: Vec<HardwareControllerDto>,
    pub errors: Vec<String>,
}

#[derive(Serialize, Clone, Default)]
pub struct HardwareControllerDto {
    /// Controller id from the queried key; this is the namespace actually observed.
    pub controller_id: String,
    /// Every distinct id reported inside HwInfo (normally exactly the key id).
    pub reported_controller_ids: Vec<String>,
    pub supervisor_versions: Vec<String>,
    /// More than one reply means duplicate hw/info providers and is surfaced loudly.
    pub info_reply_count: u32,
    pub resources: Vec<HardwareResourceDto>,
    pub warnings: Vec<String>,
}

#[derive(Serialize, Clone, Default)]
pub struct HardwareResourceDto {
    pub id: String,
    pub kind: String,
    pub model: String,
    pub key: String,
    pub alive: bool,
    pub sample_age_ms: Option<u64>,
    pub sample_bytes: Option<usize>,
    /// None means no sample/unknown kind/decode failure; Some(false) is an explicit missing Header.
    pub header_present: Option<bool>,
    /// Strings preserve exact 64-bit Header values across the JS number boundary.
    pub seq: Option<String>,
    pub stamp_ns: Option<String>,
    pub sync_ns: Option<String>,
    pub fields: Vec<HardwareFieldDto>,
    pub decode_error: Option<String>,
}

#[derive(Debug, Serialize, Clone, Default, PartialEq, Eq)]
pub struct HardwareFieldDto {
    pub name: String,
    pub value: String,
}

impl HardwareMonitor {
    pub async fn start(session: &zenoh::Session) -> Arc<Self> {
        let monitor = Arc::new(Self {
            samples: Arc::new(Mutex::new(HashMap::new())),
            alive: Arc::new(Mutex::new(HashSet::new())),
            runtime_errors: Arc::new(Mutex::new(BTreeMap::new())),
            discovery: Mutex::new(CachedDiscovery::default()),
        });

        match session.declare_subscriber(HW_KEY_EXPR).await {
            Ok(subscriber) => {
                let samples = monitor.samples.clone();
                let errors = monitor.runtime_errors.clone();
                tokio::spawn(async move {
                    loop {
                        match subscriber.recv_async().await {
                            Ok(sample) => {
                                let key = sample.key_expr().as_str().to_owned();
                                if !is_resource_key(&key) {
                                    continue;
                                }
                                let payload = (sample.kind() == SampleKind::Put)
                                    .then(|| sample.payload().to_bytes().to_vec());
                                apply_data_event(
                                    &mut samples.lock().unwrap(),
                                    key,
                                    sample.kind(),
                                    payload,
                                    Instant::now(),
                                );
                            }
                            Err(error) => {
                                record_runtime_error(
                                    &errors,
                                    "data_subscriber",
                                    format!("hardware data subscriber ended: {error}"),
                                );
                                break;
                            }
                        }
                    }
                });
            }
            Err(error) => record_runtime_error(
                &monitor.runtime_errors,
                "data_subscriber",
                format!("cannot declare hardware data subscriber {HW_KEY_EXPR}: {error}"),
            ),
        }

        match session
            .liveliness()
            .declare_subscriber(HW_KEY_EXPR)
            .history(true)
            .await
        {
            Ok(subscriber) => {
                let alive = monitor.alive.clone();
                let errors = monitor.runtime_errors.clone();
                tokio::spawn(async move {
                    loop {
                        match subscriber.recv_async().await {
                            Ok(sample) => {
                                let key = sample.key_expr().as_str().to_owned();
                                if !is_resource_key(&key) {
                                    continue;
                                }
                                let mut keys = alive.lock().unwrap();
                                match sample.kind() {
                                    SampleKind::Put => {
                                        keys.insert(key);
                                    }
                                    SampleKind::Delete => {
                                        keys.remove(&key);
                                    }
                                }
                            }
                            Err(error) => {
                                alive.lock().unwrap().clear();
                                record_runtime_error(
                                    &errors,
                                    "liveliness_subscriber",
                                    format!("hardware liveliness subscriber ended: {error}"),
                                );
                                break;
                            }
                        }
                    }
                });
            }
            Err(error) => record_runtime_error(
                &monitor.runtime_errors,
                "liveliness_subscriber",
                format!("cannot declare hardware liveliness subscriber: {error}"),
            ),
        }

        monitor
    }

    pub async fn snapshot(&self, session: &zenoh::Session) -> HardwareSnapshotDto {
        let cached = self.discovery.lock().unwrap().clone();
        let refresh = cached
            .refreshed_at
            .map(|at| at.elapsed() >= INFO_REFRESH)
            .unwrap_or(true);
        let discovery = if refresh {
            let next = discover(session).await;
            let merged = retain_missing_controllers(&cached, next);
            *self.discovery.lock().unwrap() = merged.clone();
            merged
        } else {
            cached
        };

        let samples = self.samples.lock().unwrap().clone();
        let alive = self.alive.lock().unwrap().clone();
        let errors = combined_errors(discovery.errors, &self.runtime_errors.lock().unwrap());
        HardwareSnapshotDto {
            controllers: materialize(discovery.controllers, &samples, &alive),
            errors,
        }
    }
}

/// `hw/info` is a live query, not durable inventory. If one controller stops replying, retain its
/// last-known declaration so RobotConsole can keep showing the cached sample, increasing age, and
/// offline liveliness instead of erasing the diagnostic evidence on the next refresh. A fresh
/// reply for the same CID replaces this retained view immediately.
fn retain_missing_controllers(
    previous: &CachedDiscovery,
    mut next: CachedDiscovery,
) -> CachedDiscovery {
    let current = next
        .controllers
        .iter()
        .map(|controller| controller.controller_id.clone())
        .collect::<HashSet<_>>();
    let mut retained = Vec::new();
    for mut controller in previous.controllers.iter().cloned() {
        if current.contains(&controller.controller_id) {
            continue;
        }
        controller.info_reply_count = 0;
        controller
            .warnings
            .retain(|warning| warning != LAST_KNOWN_INFO_WARNING);
        controller.warnings.push(LAST_KNOWN_INFO_WARNING.to_owned());
        retained.push(controller);
    }
    next.controllers.extend(retained);
    next.controllers
        .sort_by(|left, right| left.controller_id.cmp(&right.controller_id));
    next
}

fn combined_errors(
    mut discovery_errors: Vec<String>,
    runtime_errors: &BTreeMap<String, String>,
) -> Vec<String> {
    discovery_errors.extend(runtime_errors.values().cloned());
    discovery_errors
}

fn record_runtime_error(
    errors: &Mutex<BTreeMap<String, String>>,
    component: &str,
    message: String,
) {
    log::warn!("RobotConsole {message}");
    errors.lock().unwrap().insert(component.to_owned(), message);
}

fn apply_data_event(
    samples: &mut HashMap<String, CachedSample>,
    key: String,
    kind: SampleKind,
    payload: Option<Vec<u8>>,
    received_at: Instant,
) {
    match kind {
        SampleKind::Put => {
            if let Some(payload) = payload {
                samples.insert(
                    key,
                    CachedSample {
                        payload,
                        received_at,
                    },
                );
            }
        }
        SampleKind::Delete => {
            samples.remove(&key);
        }
    }
}

fn is_resource_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split('/').collect();
    matches!(parts.as_slice(), ["hexmeow", _, "hw", id] if *id != "info")
}

async fn discover(session: &zenoh::Session) -> CachedDiscovery {
    let mut grouped: BTreeMap<String, Vec<pb::HwInfo>> = BTreeMap::new();
    let mut errors = Vec::new();
    // Preserve every provider reply. Zenoh 1.9 resolves the default `Auto` consolidation to
    // `Latest`, which would merge duplicate providers replying on the same hw/info key.
    match session
        .get(HW_INFO_EXPR)
        .target(QueryTarget::All)
        .consolidation(ConsolidationMode::None)
        .timeout(QUERY_TIMEOUT)
        .await
    {
        Ok(replies) => {
            while let Ok(reply) = replies.recv_async().await {
                match reply.result() {
                    Ok(sample) => {
                        let key = sample.key_expr().as_str();
                        let Some(cid) = info_key_controller_id(key) else {
                            errors.push(format!("unexpected hw/info reply key: {key}"));
                            continue;
                        };
                        match pb::HwInfo::decode(&*sample.payload().to_bytes()) {
                            Ok(info) => grouped.entry(cid.to_owned()).or_default().push(info),
                            Err(error) => {
                                errors.push(format!("cannot decode {key} as HwInfo: {error}"))
                            }
                        }
                    }
                    Err(error) => errors.push(format!("hw/info reply error: {error:?}")),
                }
            }
        }
        Err(error) => errors.push(format!("hw/info query failed: {error}")),
    }

    let controllers = grouped
        .into_iter()
        .map(|(cid, infos)| aggregate_info(cid, infos))
        .collect();
    CachedDiscovery {
        refreshed_at: Some(Instant::now()),
        controllers,
        errors,
    }
}

fn info_key_controller_id(key: &str) -> Option<&str> {
    let parts: Vec<&str> = key.split('/').collect();
    match parts.as_slice() {
        ["hexmeow", cid, "hw", "info"] if !cid.is_empty() => Some(cid),
        _ => None,
    }
}

fn aggregate_info(cid: String, infos: Vec<pb::HwInfo>) -> HardwareControllerDto {
    let mut reported = BTreeSet::new();
    let mut versions = BTreeSet::new();
    let mut resources: BTreeMap<String, pb::HwResource> = BTreeMap::new();
    let mut warnings = Vec::new();

    for info in &infos {
        reported.insert(info.controller_id.clone());
        versions.insert(info.sup_version.clone());
        for resource in &info.resources {
            if let Some(previous) = resources.get(&resource.id) {
                if previous.kind != resource.kind || previous.model != resource.model {
                    warnings.push(format!(
                        "resource {} differs between hw/info replies: {}/{} vs {}/{}",
                        resource.id, previous.kind, previous.model, resource.kind, resource.model
                    ));
                }
            } else {
                resources.insert(resource.id.clone(), resource.clone());
            }
        }
    }

    if infos.len() != 1 {
        warnings.push(format!(
            "expected exactly one hw/info provider, received {} replies",
            infos.len()
        ));
    }
    for reported_cid in &reported {
        if reported_cid != &cid {
            warnings.push(format!(
                "hw/info controller_id mismatch: key={cid}, payload={reported_cid}"
            ));
        }
    }

    HardwareControllerDto {
        controller_id: cid.clone(),
        reported_controller_ids: reported.into_iter().collect(),
        supervisor_versions: versions.into_iter().collect(),
        info_reply_count: infos.len() as u32,
        resources: resources
            .into_values()
            .map(|resource| HardwareResourceDto {
                key: format!("hexmeow/{cid}/hw/{}", resource.id),
                id: resource.id,
                kind: resource.kind,
                model: resource.model,
                ..Default::default()
            })
            .collect(),
        warnings,
    }
}

fn materialize(
    mut controllers: Vec<HardwareControllerDto>,
    samples: &HashMap<String, CachedSample>,
    alive: &HashSet<String>,
) -> Vec<HardwareControllerDto> {
    for controller in &mut controllers {
        for resource in &mut controller.resources {
            resource.alive = alive.contains(&resource.key);
            let Some(sample) = samples.get(&resource.key) else {
                continue;
            };
            resource.sample_age_ms = Some(
                sample
                    .received_at
                    .elapsed()
                    .as_millis()
                    .min(u64::MAX as u128) as u64,
            );
            resource.sample_bytes = Some(sample.payload.len());
            decode_resource(resource, &sample.payload);
        }
    }
    controllers
}

fn header(resource: &mut HardwareResourceDto, header: Option<&pb::Header>) {
    resource.header_present = Some(header.is_some());
    if let Some(header) = header {
        resource.seq = Some(header.seq.to_string());
        resource.stamp_ns = Some(header.stamp_ns.to_string());
        resource.sync_ns = header.sync_ns.map(|value| value.to_string());
    }
}

fn field(resource: &mut HardwareResourceDto, name: &str, value: impl ToString) {
    resource.fields.push(HardwareFieldDto {
        name: name.to_owned(),
        value: value.to_string(),
    });
}

fn vec3(value: &pb::Vec3) -> String {
    format!("[{:.6}, {:.6}, {:.6}]", value.x, value.y, value.z)
}

fn decode_resource(resource: &mut HardwareResourceDto, payload: &[u8]) {
    resource.header_present = None;
    resource.seq = None;
    resource.stamp_ns = None;
    resource.sync_ns = None;
    resource.fields.clear();
    resource.decode_error = None;
    let decoded: Result<(), prost::DecodeError> = match resource.kind.as_str() {
        "estop" => pb::EstopState::decode(payload).map(|state| {
            header(resource, state.header.as_ref());
            field(
                resource,
                "engaged",
                state
                    .engaged
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "unknown".to_owned()),
            );
            if let Some(source) = state.source {
                field(resource, "source", source);
            }
        }),
        "power" => pb::PowerState::decode(payload).map(|state| {
            header(resource, state.header.as_ref());
            field(
                resource,
                "output_state",
                power_output_state(state.output_state),
            );
        }),
        "vbus" => pb::VbusState::decode(payload).map(|state| {
            header(resource, state.header.as_ref());
            field(resource, "voltage_v", state.voltage_v);
            if let Some(current) = state.current_a {
                field(resource, "current_a", current);
            }
        }),
        "imu" => pb::ImuData::decode(payload).map(|state| {
            header(resource, state.header.as_ref());
            if let Some(accel) = state.accel.as_ref() {
                field(resource, "accel_m_s2", vec3(accel));
            }
            if let Some(gyro) = state.gyro.as_ref() {
                field(resource, "gyro_rad_s", vec3(gyro));
            }
            if let Some(quat) = state.quat {
                field(
                    resource,
                    "quat_wxyz",
                    format!(
                        "[{:.6}, {:.6}, {:.6}, {:.6}]",
                        quat.w, quat.x, quat.y, quat.z
                    ),
                );
            }
        }),
        "remote" => pb::RemoteState::decode(payload).map(|state| {
            header(resource, state.header.as_ref());
            field(resource, "active", state.active);
            field(resource, "axes", format!("{:?}", state.axes));
            field(resource, "buttons", format!("{:?}", state.buttons));
        }),
        _ => {
            field(resource, "raw_payload_bytes", payload.len());
            return;
        }
    };
    if let Err(error) = decoded {
        resource.decode_error = Some(error.to_string());
    }
}

fn power_output_state(value: i32) -> String {
    match value {
        0 => "UNSPECIFIED (0)".to_owned(),
        1 => "NOT_OUTPUT (1)".to_owned(),
        2 => "POWERING_ON (2)".to_owned(),
        3 => "OUTPUT (3)".to_owned(),
        4 => "FAULT (4)".to_owned(),
        other => format!("UNKNOWN ({other})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(kind: &str) -> HardwareResourceDto {
        HardwareResourceDto {
            kind: kind.to_owned(),
            ..Default::default()
        }
    }

    fn encoded<M: Message>(message: M) -> Vec<u8> {
        message.encode_to_vec()
    }

    #[test]
    fn estop_preserves_unknown_presence_and_source() {
        let mut output = resource("estop");
        decode_resource(
            &mut output,
            &encoded(pb::EstopState {
                header: Some(pb::Header {
                    seq: 7,
                    stamp_ns: 11,
                    sync_ns: None,
                }),
                engaged: None,
                source: Some("adc-keys:unavailable".into()),
            }),
        );
        assert_eq!(output.seq.as_deref(), Some("7"));
        assert_eq!(output.header_present, Some(true));
        assert_eq!(output.fields[0].value, "unknown");
        assert_eq!(output.fields[1].value, "adc-keys:unavailable");
    }

    #[test]
    fn power_keeps_unknown_future_enum_value() {
        let mut output = resource("power");
        decode_resource(
            &mut output,
            &encoded(pb::PowerState {
                header: None,
                output_state: 99,
            }),
        );
        assert_eq!(output.fields[0].value, "UNKNOWN (99)");
        assert_eq!(output.header_present, Some(false));
    }

    #[test]
    fn known_resource_messages_expose_every_payload_field() {
        let mut vbus = resource("vbus");
        decode_resource(
            &mut vbus,
            &encoded(pb::VbusState {
                header: None,
                voltage_v: 24.5,
                current_a: Some(1.25),
            }),
        );
        assert_eq!(vbus.fields.len(), 2);

        let mut imu = resource("imu");
        decode_resource(
            &mut imu,
            &encoded(pb::ImuData {
                header: None,
                accel: Some(pb::Vec3 {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                }),
                gyro: Some(pb::Vec3 {
                    x: 4.0,
                    y: 5.0,
                    z: 6.0,
                }),
                quat: Some(pb::Quat {
                    w: 1.0,
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                }),
            }),
        );
        assert_eq!(imu.fields.len(), 3);

        let mut remote = resource("remote");
        decode_resource(
            &mut remote,
            &encoded(pb::RemoteState {
                header: None,
                active: true,
                axes: vec![0.25, -0.5],
                buttons: vec![true, false],
            }),
        );
        assert_eq!(remote.fields.len(), 3);
    }

    #[test]
    fn aggregate_reports_duplicate_and_controller_id_mismatch() {
        let controller = aggregate_info(
            "key-cid".into(),
            vec![
                pb::HwInfo {
                    controller_id: "payload-cid".into(),
                    sup_version: "1".into(),
                    resources: vec![],
                },
                pb::HwInfo {
                    controller_id: "payload-cid".into(),
                    sup_version: "1".into(),
                    resources: vec![],
                },
            ],
        );
        assert_eq!(controller.info_reply_count, 2);
        assert_eq!(controller.warnings.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn discovery_preserves_two_providers_replying_on_the_same_info_key() {
        let mut config = zenoh::Config::default();
        config.insert_json5("listen/endpoints", "[]").unwrap();
        config
            .insert_json5("scouting/multicast/enabled", "false")
            .unwrap();
        config
            .insert_json5("scouting/gossip/enabled", "false")
            .unwrap();
        let session = zenoh::open(config).await.unwrap();
        let cid = format!("gui-duplicate-provider-test-{}", std::process::id());
        let key = format!("hexmeow/{cid}/hw/info");
        let q1 = session.declare_queryable(key.clone()).await.unwrap();
        let q2 = session.declare_queryable(key.clone()).await.unwrap();
        let payload = |version: &str| {
            encoded(pb::HwInfo {
                controller_id: cid.clone(),
                sup_version: version.into(),
                resources: vec![],
            })
        };
        let key1 = key.clone();
        let key2 = key.clone();
        let reply1 = async {
            let query = q1.recv_async().await.unwrap();
            query.reply(key1, payload("provider-1")).await.unwrap();
        };
        let reply2 = async {
            let query = q2.recv_async().await.unwrap();
            query.reply(key2, payload("provider-2")).await.unwrap();
        };

        let (discovery, _, _) = tokio::join!(discover(&session), reply1, reply2);
        assert!(discovery.errors.is_empty(), "{:?}", discovery.errors);
        assert_eq!(discovery.controllers.len(), 1);
        let controller = &discovery.controllers[0];
        assert_eq!(controller.info_reply_count, 2);
        assert_eq!(controller.supervisor_versions, ["provider-1", "provider-2"]);
        assert!(controller
            .warnings
            .iter()
            .any(|warning| warning.contains("received 2 replies")));
        let _ = session.close().await;
    }

    #[test]
    fn unknown_kind_is_not_guessed() {
        let mut output = resource("future-sensor");
        decode_resource(&mut output, &[1, 2, 3, 4]);
        assert_eq!(
            output.fields,
            vec![HardwareFieldDto {
                name: "raw_payload_bytes".into(),
                value: "4".into()
            }]
        );
        assert!(output.decode_error.is_none());
    }

    #[test]
    fn delete_event_removes_cached_sample_instead_of_caching_empty_payload() {
        let key = "hexmeow/cid/hw/estop0".to_owned();
        let mut samples = HashMap::new();
        apply_data_event(
            &mut samples,
            key.clone(),
            SampleKind::Put,
            Some(vec![1, 2, 3]),
            Instant::now(),
        );
        assert_eq!(samples[&key].payload, vec![1, 2, 3]);

        apply_data_event(
            &mut samples,
            key.clone(),
            SampleKind::Delete,
            None,
            Instant::now(),
        );
        assert!(!samples.contains_key(&key));
    }

    #[test]
    fn failed_or_unknown_decode_clears_previous_header_and_fields() {
        let mut output = resource("estop");
        decode_resource(
            &mut output,
            &encoded(pb::EstopState {
                header: Some(pb::Header {
                    seq: 42,
                    stamp_ns: 99,
                    sync_ns: Some(123),
                }),
                engaged: Some(false),
                source: Some("old".into()),
            }),
        );
        assert!(output.header_present.unwrap());
        assert_eq!(output.fields.len(), 2);

        output.kind = "power".into();
        decode_resource(&mut output, &[0x0f]); // invalid protobuf wire type
        assert_eq!(output.header_present, None);
        assert_eq!(output.seq, None);
        assert_eq!(output.stamp_ns, None);
        assert_eq!(output.sync_ns, None);
        assert!(output.fields.is_empty());
        assert!(output.decode_error.is_some());

        output.kind = "future-sensor".into();
        decode_resource(&mut output, &[1, 2, 3]);
        assert_eq!(output.header_present, None);
        assert_eq!(output.seq, None);
        assert_eq!(output.fields.len(), 1);
        assert_eq!(output.fields[0].name, "raw_payload_bytes");
        assert!(output.decode_error.is_none());
    }

    #[test]
    fn subscriber_runtime_errors_are_included_in_snapshot_errors() {
        let runtime = BTreeMap::from([
            ("data_subscriber".into(), "data loop ended".into()),
            (
                "liveliness_subscriber".into(),
                "liveliness declaration failed".into(),
            ),
        ]);
        let errors = combined_errors(vec!["hw/info malformed".into()], &runtime);
        assert_eq!(errors.len(), 3);
        assert!(errors.iter().any(|error| error == "data loop ended"));
        assert!(errors
            .iter()
            .any(|error| error == "liveliness declaration failed"));
    }

    #[test]
    fn offline_resource_keeps_last_sample_and_age() {
        let key = "hexmeow/cid/hw/estop0".to_owned();
        let controller = HardwareControllerDto {
            controller_id: "cid".into(),
            resources: vec![HardwareResourceDto {
                id: "estop0".into(),
                kind: "estop".into(),
                key: key.clone(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let samples = HashMap::from([(
            key,
            CachedSample {
                payload: encoded(pb::EstopState {
                    header: None,
                    engaged: Some(true),
                    source: Some("test".into()),
                }),
                received_at: Instant::now() - Duration::from_millis(10),
            },
        )]);
        let output = materialize(vec![controller], &samples, &HashSet::new());
        let resource = &output[0].resources[0];
        assert!(!resource.alive);
        assert!(resource.sample_age_ms.unwrap() >= 10);
        assert_eq!(resource.fields[0].value, "true");
    }

    #[test]
    fn missing_hw_info_retains_last_known_controller_as_offline_inventory() {
        let previous = CachedDiscovery {
            refreshed_at: Some(Instant::now()),
            controllers: vec![HardwareControllerDto {
                controller_id: "cid-a".into(),
                info_reply_count: 1,
                resources: vec![HardwareResourceDto {
                    id: "estop0".into(),
                    kind: "estop".into(),
                    key: "hexmeow/cid-a/hw/estop0".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            errors: vec![],
        };
        let next = CachedDiscovery {
            refreshed_at: Some(Instant::now()),
            controllers: vec![],
            errors: vec!["query timed out".into()],
        };

        let merged = retain_missing_controllers(&previous, next);
        assert_eq!(merged.controllers.len(), 1);
        assert_eq!(merged.controllers[0].controller_id, "cid-a");
        assert_eq!(merged.controllers[0].info_reply_count, 0);
        assert_eq!(merged.controllers[0].resources.len(), 1);
        assert!(merged.controllers[0]
            .warnings
            .iter()
            .any(|warning| warning == LAST_KNOWN_INFO_WARNING));
        assert_eq!(merged.errors, vec!["query timed out"]);
    }
}
