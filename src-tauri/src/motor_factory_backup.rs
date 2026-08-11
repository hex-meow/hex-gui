//! Durable, opaque pre-DFU snapshots of Meow Motor manufacturer data.
//!
//! The updater must preserve every word reported by `0x4001:00`, including
//! future words that the current calibration decoder does not understand.  A
//! successful snapshot is a prerequisite for entering the supplier IAP on a
//! Meow Motor profile; semantic decoding is deliberately not an authorization
//! requirement here.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use hexmeow_stm32_can_dfu::{ObjectAddress, SdoTransport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const FACTORY_DATA_INDEX: u16 = 0x4001;
const MIN_CURRENT_SUBINDEX: u8 = 7;
const MAX_BACKUP_SUBINDEX: u8 = 64;
const SDO_TIMEOUT: Duration = Duration::from_millis(700);
const UPLOAD_ATTEMPTS: usize = 3;
const SNAPSHOT_ATTEMPTS: usize = 3;
const BACKUP_FORMAT: &str = "hexmeow-dfu/meow-motor-0x4001-backup/1";

enum SnapshotReadError {
    Fatal(String),
    Retryable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawFactorySnapshot {
    pub(crate) highest_subindex: u8,
    pub(crate) words: Vec<u32>,
    pub(crate) record_sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct BackupIdentity {
    pub(crate) vendor_id: u32,
    pub(crate) product_code: u32,
    pub(crate) revision_number: u32,
    pub(crate) serial_number: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct BackupArtifact {
    pub(crate) source: &'static str,
    pub(crate) release_id: Option<String>,
    pub(crate) sha256: String,
    pub(crate) bytes: usize,
    pub(crate) device_id: u32,
    pub(crate) firmware_id: u32,
    pub(crate) firmware_version: u32,
    pub(crate) start_address: u32,
    pub(crate) end_address: u32,
    pub(crate) bin_size: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct BackupContext {
    pub(crate) can_interface: String,
    pub(crate) node_id: u8,
    pub(crate) profile_id: String,
    pub(crate) identity: BackupIdentity,
    pub(crate) artifact: BackupArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PersistedBackup {
    pub(crate) path: PathBuf,
    pub(crate) file_sha256: String,
    pub(crate) snapshot: RawFactorySnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct BackupDocument {
    format: String,
    captured_at: String,
    gui_version: String,
    can_interface: String,
    node_id_at_capture: u8,
    profile_id: String,
    identity: IdentityDocument,
    artifact: ArtifactDocument,
    factory_data: FactoryDataDocument,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct IdentityDocument {
    vendor_id: u32,
    vendor_id_hex: String,
    product_code: u32,
    product_code_hex: String,
    revision_number: u32,
    revision_number_hex: String,
    serial_number: u32,
    serial_number_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ArtifactDocument {
    source: String,
    release_id: Option<String>,
    sha256: String,
    bytes: usize,
    device_id_hex: String,
    firmware_id_hex: String,
    firmware_version_hex: String,
    start_address_hex: String,
    end_address_hex: String,
    bin_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct FactoryDataDocument {
    index_hex: String,
    highest_subindex: u8,
    record_sha256: String,
    words: Vec<RawWordDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RawWordDocument {
    subindex: u8,
    value_u32: u32,
    value_hex: String,
    le_bytes_hex: String,
}

/// Read two complete copies of the record and accept only a byte-for-byte
/// stable result. Individual uploads may be retried, but words from different
/// record copies are never combined.
pub(crate) async fn read_stable_snapshot(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
) -> Result<RawFactorySnapshot, String> {
    let mut last_error = "0x4001 did not produce a stable complete snapshot".to_owned();
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let first = match read_complete_record(sdo, node_id).await {
            Ok(snapshot) => snapshot,
            Err(SnapshotReadError::Fatal(error)) => return Err(error),
            Err(SnapshotReadError::Retryable(error)) => {
                last_error = error;
                continue;
            }
        };
        let second = match read_complete_record(sdo, node_id).await {
            Ok(snapshot) => snapshot,
            Err(SnapshotReadError::Fatal(error)) => return Err(error),
            Err(SnapshotReadError::Retryable(error)) => {
                last_error = error;
                continue;
            }
        };
        if first == second {
            return Ok(first);
        }
        last_error = "0x4001 changed while comparing two complete snapshots".into();
    }
    Err(format!(
        "0x4001 backup failed after {SNAPSHOT_ATTEMPTS} complete-snapshot attempts: {last_error}"
    ))
}

async fn read_complete_record(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
) -> Result<RawFactorySnapshot, SnapshotReadError> {
    let highest_before = upload_exact::<1>(sdo, node_id, 0).await?[0];
    if highest_before < MIN_CURRENT_SUBINDEX {
        return Err(SnapshotReadError::Fatal(format!(
            "0x4001:00 is {highest_before}; this Meow Motor profile requires at least {MIN_CURRENT_SUBINDEX} factory-data words"
        )));
    }
    if highest_before > MAX_BACKUP_SUBINDEX {
        return Err(SnapshotReadError::Fatal(format!(
            "0x4001:00 is {highest_before}, above the updater's {MAX_BACKUP_SUBINDEX}-word host resource limit"
        )));
    }

    let mut words = Vec::with_capacity(usize::from(highest_before));
    for subindex in 1..=highest_before {
        let bytes = upload_exact::<4>(sdo, node_id, subindex).await?;
        words.push(u32::from_le_bytes(bytes));
    }

    let highest_after = upload_exact::<1>(sdo, node_id, 0).await?[0];
    if highest_after != highest_before {
        return Err(SnapshotReadError::Retryable(format!(
            "0x4001:00 changed from {highest_before} to {highest_after} while reading"
        )));
    }

    Ok(RawFactorySnapshot {
        highest_subindex: highest_before,
        record_sha256: record_sha256(highest_before, &words),
        words,
    })
}

async fn upload_exact<const N: usize>(
    sdo: &(impl SdoTransport + ?Sized),
    node_id: u8,
    subindex: u8,
) -> Result<[u8; N], SnapshotReadError> {
    let address = ObjectAddress::new(FACTORY_DATA_INDEX, subindex);
    let mut last_error = None;
    for attempt in 0..UPLOAD_ATTEMPTS {
        match sdo.upload(node_id, address, SDO_TIMEOUT).await {
            Ok(bytes) => {
                return bytes.try_into().map_err(|bytes: Vec<u8>| {
                    SnapshotReadError::Fatal(format!(
                        "{address} returned {} bytes; expected exactly {N}",
                        bytes.len()
                    ))
                })
            }
            Err(error) if error.is_definitive_rejection() => {
                return Err(SnapshotReadError::Fatal(format!(
                    "SDO upload {address} was rejected: {error}"
                )))
            }
            Err(error) => {
                last_error = Some(error.to_string());
                if attempt + 1 == UPLOAD_ATTEMPTS {
                    break;
                }
            }
        }
    }
    Err(SnapshotReadError::Retryable(format!(
        "SDO upload {address} failed after {UPLOAD_ATTEMPTS} attempts: {}",
        last_error.unwrap_or_else(|| "unknown transport error".into())
    )))
}

pub(crate) fn persist_backup(
    root: &Path,
    context: &BackupContext,
    snapshot: &RawFactorySnapshot,
) -> Result<PersistedBackup, String> {
    create_private_directory(root)?;

    let captured_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let document = BackupDocument {
        format: BACKUP_FORMAT.into(),
        captured_at: captured_at.clone(),
        gui_version: env!("CARGO_PKG_VERSION").into(),
        can_interface: context.can_interface.clone(),
        node_id_at_capture: context.node_id,
        profile_id: context.profile_id.clone(),
        identity: IdentityDocument {
            vendor_id: context.identity.vendor_id,
            vendor_id_hex: hex_u32(context.identity.vendor_id),
            product_code: context.identity.product_code,
            product_code_hex: hex_u32(context.identity.product_code),
            revision_number: context.identity.revision_number,
            revision_number_hex: hex_u32(context.identity.revision_number),
            serial_number: context.identity.serial_number,
            serial_number_hex: hex_u32(context.identity.serial_number),
        },
        artifact: ArtifactDocument {
            source: context.artifact.source.into(),
            release_id: context.artifact.release_id.clone(),
            sha256: context.artifact.sha256.clone(),
            bytes: context.artifact.bytes,
            device_id_hex: hex_u32(context.artifact.device_id),
            firmware_id_hex: hex_u32(context.artifact.firmware_id),
            firmware_version_hex: hex_u32(context.artifact.firmware_version),
            start_address_hex: hex_u32(context.artifact.start_address),
            end_address_hex: hex_u32(context.artifact.end_address),
            bin_size: context.artifact.bin_size,
        },
        factory_data: FactoryDataDocument {
            index_hex: hex_u16(FACTORY_DATA_INDEX),
            highest_subindex: snapshot.highest_subindex,
            record_sha256: snapshot.record_sha256.clone(),
            words: snapshot
                .words
                .iter()
                .enumerate()
                .map(|(offset, value)| RawWordDocument {
                    subindex: u8::try_from(offset + 1).expect("0x4001 highest subindex is an u8"),
                    value_u32: *value,
                    value_hex: hex_u32(*value),
                    le_bytes_hex: hex::encode(value.to_le_bytes()),
                })
                .collect(),
        },
    };
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("serializing 0x4001 backup: {error}"))?;
    let file_sha256 = hex_sha256(&bytes);

    let stamp = captured_at
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    let sha_prefix = context
        .artifact
        .sha256
        .get(..8)
        .unwrap_or(&context.artifact.sha256);
    let nonce = getrandom::u64().map_err(|error| format!("creating backup nonce: {error}"))?;
    let filename = format!(
        "{stamp}-{:08x}-{:08x}-sn{:08x}-{sha_prefix}-{nonce:016x}.json",
        context.identity.vendor_id, context.identity.product_code, context.identity.serial_number,
    );
    let final_path = root.join(filename);
    let temporary_path = root.join(format!(".0x4001-backup-{nonce:016x}.tmp"));

    let result = (|| -> Result<(), String> {
        let mut file = private_new_file(&temporary_path)?;
        file.write_all(&bytes)
            .map_err(|error| format!("writing temporary 0x4001 backup: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("syncing temporary 0x4001 backup: {error}"))?;
        drop(file);
        // Linking a fully synced, create-new temporary file gives us an
        // atomic no-replace commit. `rename` would silently overwrite an
        // existing backup on Unix, which is forbidden for calibration data.
        fs::hard_link(&temporary_path, &final_path)
            .map_err(|error| format!("committing 0x4001 backup without overwrite: {error}"))?;
        fs::remove_file(&temporary_path)
            .map_err(|error| format!("removing committed backup temporary link: {error}"))?;
        sync_directory(root)?;

        let persisted = fs::read(&final_path)
            .map_err(|error| format!("reopening committed 0x4001 backup: {error}"))?;
        if hex_sha256(&persisted) != file_sha256 {
            return Err("committed 0x4001 backup SHA-256 changed after reopen".into());
        }
        let decoded: BackupDocument = serde_json::from_slice(&persisted)
            .map_err(|error| format!("reparsing committed 0x4001 backup: {error}"))?;
        if decoded != document {
            return Err("committed 0x4001 backup changed after reopen".into());
        }
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result?;

    Ok(PersistedBackup {
        path: final_path,
        file_sha256,
        snapshot: snapshot.clone(),
    })
}

fn record_sha256(highest_subindex: u8, words: &[u32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update([highest_subindex]);
    for word in words {
        hasher.update(word.to_le_bytes());
    }
    hex::encode(hasher.finalize())
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hex_u16(value: u16) -> String {
    format!("0x{value:04X}")
}

fn hex_u32(value: u32) -> String {
    format!("0x{value:08X}")
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("creating firmware backup directory: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("protecting firmware backup directory: {error}"))?;
    }
    Ok(())
}

fn private_new_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| format!("creating temporary 0x4001 backup: {error}"))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("syncing firmware backup directory: {error}"))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hexmeow_stm32_can_dfu::TransportError;

    use super::*;

    struct ScriptedSdo {
        replies: Mutex<VecDeque<(ObjectAddress, Vec<u8>)>>,
    }

    #[async_trait]
    impl SdoTransport for ScriptedSdo {
        async fn upload(
            &self,
            _node_id: u8,
            object: ObjectAddress,
            _timeout: Duration,
        ) -> Result<Vec<u8>, TransportError> {
            let (expected, bytes) = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| TransportError::new("unexpected upload"))?;
            assert_eq!(object, expected);
            Ok(bytes)
        }

        async fn download(
            &self,
            _node_id: u8,
            _object: ObjectAddress,
            _data: &[u8],
            _timeout: Duration,
        ) -> Result<(), TransportError> {
            panic!("backup is read-only")
        }
    }

    fn stable_script(words: &[u32]) -> ScriptedSdo {
        let mut replies = VecDeque::new();
        for _ in 0..2 {
            replies.push_back((
                ObjectAddress::new(FACTORY_DATA_INDEX, 0),
                vec![words.len() as u8],
            ));
            for (offset, word) in words.iter().enumerate() {
                replies.push_back((
                    ObjectAddress::new(FACTORY_DATA_INDEX, (offset + 1) as u8),
                    word.to_le_bytes().to_vec(),
                ));
            }
            replies.push_back((
                ObjectAddress::new(FACTORY_DATA_INDEX, 0),
                vec![words.len() as u8],
            ));
        }
        ScriptedSdo {
            replies: Mutex::new(replies),
        }
    }

    #[tokio::test]
    async fn reads_every_reported_word_twice() {
        let words = (1..=9).map(|value| value * 17).collect::<Vec<_>>();
        let sdo = stable_script(&words);
        let snapshot = read_stable_snapshot(&sdo, 1).await.unwrap();
        assert_eq!(snapshot.highest_subindex, 9);
        assert_eq!(snapshot.words, words);
        assert!(sdo.replies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_records_shorter_than_the_current_contract() {
        let sdo = ScriptedSdo {
            replies: Mutex::new(VecDeque::from([(
                ObjectAddress::new(FACTORY_DATA_INDEX, 0),
                vec![6],
            )])),
        };
        let error = read_stable_snapshot(&sdo, 1).await.unwrap_err();
        assert!(error.contains("requires at least 7"));
    }

    #[test]
    fn durable_document_preserves_future_words_and_reopens() {
        let nonce = getrandom::u64().unwrap();
        let root = std::env::temp_dir().join(format!("hexmeow-dfu-backup-test-{nonce:016x}"));
        let words = (1..=9).map(|value| value * 0x10203).collect::<Vec<_>>();
        let snapshot = RawFactorySnapshot {
            highest_subindex: words.len() as u8,
            record_sha256: record_sha256(words.len() as u8, &words),
            words: words.clone(),
        };
        let context = BackupContext {
            can_interface: "vcan0".into(),
            node_id: 1,
            profile_id: "custom-motor-4310-v1".into(),
            identity: BackupIdentity {
                vendor_id: 0x0068_6578,
                product_code: 0x6C64_BC78,
                revision_number: 0x6578_0001,
                serial_number: 0x2510_4409,
            },
            artifact: BackupArtifact {
                source: "local",
                release_id: None,
                sha256: "51af1058197a0df08381a05e19fb8ed4ada8b6988492d280b5c9d650d8c7bf58".into(),
                bytes: 177_212,
                device_id: 0xAAAA_0001,
                firmware_id: 0x2025_1025,
                firmware_version: 0x6578_0001,
                start_address: 0x1000_C000,
                end_address: 0x1003_73AF,
                bin_size: 177_072,
            },
        };

        let persisted = persist_backup(&root, &context, &snapshot).unwrap();
        assert_eq!(persisted.snapshot.words, words);
        assert!(persisted.path.is_file());
        assert_eq!(persisted.file_sha256.len(), 64);

        let document: BackupDocument =
            serde_json::from_slice(&fs::read(&persisted.path).unwrap()).unwrap();
        assert_eq!(document.factory_data.highest_subindex, 9);
        assert_eq!(document.factory_data.words.len(), 9);
        assert_eq!(document.artifact.sha256, context.artifact.sha256);

        fs::remove_dir_all(root).unwrap();
    }
}
