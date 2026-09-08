use crate::domain::{ArtifactId, Principal, ResourceRef, StateVersion, WorkspaceVersion};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt, fs,
    fs::{File, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Sensitive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
    Zstd,
}

#[derive(Clone, Debug)]
pub struct NewArtifact {
    pub media_type: String,
    pub creator: Principal,
    pub source_revision: Option<WorkspaceVersion>,
    pub sensitivity: Sensitivity,
    pub retain_until_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub media_type: String,
    pub digest: StateVersion,
    pub size: u64,
    pub encoded_size: u64,
    pub compression: Compression,
    pub creator: Principal,
    pub source_revision: Option<WorkspaceVersion>,
    pub sensitivity: Sensitivity,
    pub retain_until_ms: u64,
}

impl ArtifactMetadata {
    pub fn resource_ref(&self) -> ResourceRef {
        ResourceRef::new("artifact", self.id.to_string())
            .expect("a UUID is always a valid resource value")
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ArtifactReadLimits {
    pub max_bytes: u64,
    pub max_expansion_ratio: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub references_removed: u64,
    pub objects_removed: u64,
}

/// A stored blob no surviving reference points at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrphanObject {
    pub digest: StateVersion,
    /// Where the blob was actually found. Carried rather than rebuilt from the
    /// digest, so applying the plan removes the file the plan reported: a store
    /// whose layout has drifted is still collected, not turned into an error
    /// about a path nothing ever wrote.
    pub path: PathBuf,
    pub encoded_size: u64,
}

/// What a collection would remove. `bytes` counts only the referenced
/// artifacts, whose encoded size is recorded; orphan blobs carry their own.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcPlan {
    pub references: Vec<ArtifactMetadata>,
    pub objects: Vec<OrphanObject>,
    pub bytes: u64,
}

impl GcPlan {
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.objects.is_empty()
    }

    /// Every byte the plan reclaims, references and orphan blobs together.
    pub fn reclaimed_bytes(&self) -> u64 {
        self.objects.iter().fold(self.bytes, |total, object| {
            total.saturating_add(object.encoded_size)
        })
    }
}

pub trait ArtifactStore: Send + Sync {
    fn put(&self, bytes: &[u8], new: NewArtifact) -> Result<ArtifactMetadata, ArtifactError>;
    fn metadata(&self, id: ArtifactId) -> Result<ArtifactMetadata, ArtifactError>;
    fn read(&self, id: ArtifactId, limits: ArtifactReadLimits) -> Result<Vec<u8>, ArtifactError>;
    fn gc(&self, reachable: &HashSet<ArtifactId>, now_ms: u64) -> Result<GcReport, ArtifactError>;
}

pub struct FileArtifactStore {
    root: PathBuf,
    orphan_retention_ms: u64,
}

impl FileArtifactStore {
    pub fn open(root: impl AsRef<Path>, orphan_retention_ms: u64) -> Result<Self, ArtifactError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(root.join("objects")).map_err(io_error)?;
        fs::create_dir_all(root.join("refs")).map_err(io_error)?;
        sync_dir(&root)?;
        Ok(Self {
            root,
            orphan_retention_ms,
        })
    }

    fn reference_path(&self, id: ArtifactId) -> PathBuf {
        self.root.join("refs").join(format!("{id}.json"))
    }

    fn object_path(&self, digest: StateVersion) -> PathBuf {
        let digest = digest.to_string();
        self.root
            .join("objects")
            .join(&digest[..2])
            .join(format!("{digest}.blob"))
    }

    /// Everything `gc` would remove, without removing it.
    ///
    /// `gc` is defined as "apply this plan", so a dry run and the deletion can
    /// never disagree about what is unreachable — which is the only reason to
    /// trust `arsy gc` before `arsy gc --apply`.
    pub fn gc_plan(
        &self,
        reachable: &HashSet<ArtifactId>,
        now_ms: u64,
    ) -> Result<GcPlan, ArtifactError> {
        let mut plan = GcPlan::default();
        let mut live = HashSet::new();
        for metadata in self.all_metadata()? {
            if !reachable.contains(&metadata.id) && now_ms >= metadata.retain_until_ms {
                plan.bytes = plan.bytes.saturating_add(metadata.encoded_size);
                plan.references.push(metadata);
            } else {
                live.insert(metadata.digest);
            }
        }

        for prefix in fs::read_dir(self.root.join("objects")).map_err(io_error)? {
            let prefix = prefix.map_err(io_error)?;
            if !prefix.file_type().map_err(io_error)?.is_dir() {
                continue;
            }
            for object in fs::read_dir(prefix.path()).map_err(io_error)? {
                let object = object.map_err(io_error)?;
                if !object.file_type().map_err(io_error)?.is_file() {
                    continue;
                }
                let Some(stem) = object
                    .path()
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(str::to_owned)
                else {
                    continue;
                };
                let Ok(digest) = stem.parse::<StateVersion>() else {
                    continue;
                };
                let file = object.metadata().map_err(io_error)?;
                if !live.contains(&digest) && age_ms(&file, now_ms)? >= self.orphan_retention_ms {
                    plan.objects.push(OrphanObject {
                        digest,
                        path: object.path(),
                        encoded_size: file.len(),
                    });
                }
            }
        }
        Ok(plan)
    }

    fn all_metadata(&self) -> Result<Vec<ArtifactMetadata>, ArtifactError> {
        let mut metadata = Vec::new();
        for entry in fs::read_dir(self.root.join("refs")).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            if entry.file_type().map_err(io_error)?.is_file()
                && entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            {
                metadata.push(read_json(&entry.path())?);
            }
        }
        Ok(metadata)
    }
}

impl ArtifactStore for FileArtifactStore {
    fn put(&self, bytes: &[u8], new: NewArtifact) -> Result<ArtifactMetadata, ArtifactError> {
        if new.media_type.trim().is_empty() {
            return Err(ArtifactError::InvalidMetadata("media type cannot be empty"));
        }
        let digest = StateVersion::from_digest(Sha256::digest(bytes).into());
        let compressed = zstd::stream::encode_all(Cursor::new(bytes), 3).map_err(io_error)?;
        let (compression, encoded) = if compressed.len() < bytes.len() {
            (Compression::Zstd, compressed.as_slice())
        } else {
            (Compression::None, bytes)
        };
        let metadata = ArtifactMetadata {
            id: ArtifactId::new(),
            media_type: new.media_type,
            digest,
            size: length(bytes.len())?,
            encoded_size: length(encoded.len())?,
            compression,
            creator: new.creator,
            source_revision: new.source_revision,
            sensitivity: new.sensitivity,
            retain_until_ms: new.retain_until_ms,
        };

        write_once_durable(&self.object_path(digest), encoded)?;
        let metadata_json = serde_json::to_vec_pretty(&metadata).map_err(serialization)?;
        write_atomic_durable(&self.reference_path(metadata.id), &metadata_json)?;
        Ok(metadata)
    }

    fn metadata(&self, id: ArtifactId) -> Result<ArtifactMetadata, ArtifactError> {
        let file = File::open(self.reference_path(id)).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ArtifactError::NotFound(id)
            } else {
                io_error(error)
            }
        })?;
        serde_json::from_reader(file).map_err(serialization)
    }

    fn read(&self, id: ArtifactId, limits: ArtifactReadLimits) -> Result<Vec<u8>, ArtifactError> {
        let metadata = self.metadata(id)?;
        if metadata.size > limits.max_bytes {
            return Err(ArtifactError::TooLarge {
                size: metadata.size,
                max: limits.max_bytes,
            });
        }
        if metadata.encoded_size > metadata.size
            || (metadata.compression == Compression::Zstd
                && (metadata.encoded_size == 0
                    || metadata.size
                        > metadata
                            .encoded_size
                            .saturating_mul(limits.max_expansion_ratio)))
        {
            return Err(ArtifactError::ExpansionRatioExceeded);
        }

        let encoded = read_bounded(&self.object_path(metadata.digest), metadata.encoded_size)?;
        let decoded = match metadata.compression {
            Compression::None => encoded,
            Compression::Zstd => {
                let decoder =
                    zstd::stream::read::Decoder::new(Cursor::new(encoded)).map_err(io_error)?;
                read_limited(decoder, limits.max_bytes)?
            }
        };
        if length(decoded.len())? != metadata.size
            || StateVersion::from_digest(Sha256::digest(&decoded).into()) != metadata.digest
        {
            return Err(ArtifactError::DigestMismatch);
        }
        Ok(decoded)
    }

    fn gc(&self, reachable: &HashSet<ArtifactId>, now_ms: u64) -> Result<GcReport, ArtifactError> {
        let plan = self.gc_plan(reachable, now_ms)?;
        for metadata in &plan.references {
            fs::remove_file(self.reference_path(metadata.id)).map_err(io_error)?;
        }
        sync_dir(&self.root.join("refs"))?;
        for object in &plan.objects {
            fs::remove_file(&object.path).map_err(io_error)?;
            if let Some(prefix) = object.path.parent() {
                sync_dir(prefix)?;
            }
        }
        Ok(GcReport {
            references_removed: plan.references.len() as u64,
            objects_removed: plan.objects.len() as u64,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactError {
    NotFound(ArtifactId),
    InvalidMetadata(&'static str),
    TooLarge { size: u64, max: u64 },
    ExpansionRatioExceeded,
    DigestMismatch,
    Io(String),
    Serialization(String),
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(formatter, "artifact {id} not found"),
            Self::InvalidMetadata(message) => formatter.write_str(message),
            Self::Io(message) | Self::Serialization(message) => formatter.write_str(message),
            Self::TooLarge { size, max } => {
                write!(formatter, "artifact is {size} bytes; maximum is {max}")
            }
            Self::ExpansionRatioExceeded => {
                formatter.write_str("artifact expansion ratio exceeds the configured maximum")
            }
            Self::DigestMismatch => formatter.write_str("artifact digest does not match content"),
        }
    }
}

impl std::error::Error for ArtifactError {}

fn write_once_durable(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or(ArtifactError::InvalidMetadata("object path has no parent"))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = parent.join(format!(".{}.tmp", ArtifactId::new()));
    write_new(&temporary, bytes)?;
    if let Err(error) = fs::rename(&temporary, path) {
        if !path.exists() {
            return Err(io_error(error));
        }
        fs::remove_file(&temporary).map_err(io_error)?;
    }
    sync_dir(parent)
}

fn write_atomic_durable(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let parent = path.parent().ok_or(ArtifactError::InvalidMetadata(
        "reference path has no parent",
    ))?;
    let temporary = parent.join(format!(".{}.tmp", ArtifactId::new()));
    write_new(&temporary, bytes)?;
    fs::rename(&temporary, path).map_err(io_error)?;
    sync_dir(parent)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), ArtifactError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<(), ArtifactError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<(), ArtifactError> {
    // ponytail: std has no portable directory fsync; use a write-through Windows rename if tests show the file sync is insufficient.
    Ok(())
}

fn read_json(path: &Path) -> Result<ArtifactMetadata, ArtifactError> {
    serde_json::from_reader(File::open(path).map_err(io_error)?).map_err(serialization)
}

fn read_bounded(path: &Path, expected: u64) -> Result<Vec<u8>, ArtifactError> {
    let bytes = read_limited(File::open(path).map_err(io_error)?, expected)?;
    if length(bytes.len())? != expected {
        return Err(ArtifactError::DigestMismatch);
    }
    Ok(bytes)
}

fn read_limited(reader: impl Read, max: u64) -> Result<Vec<u8>, ArtifactError> {
    let mut bytes = Vec::new();
    reader
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if length(bytes.len())? > max {
        return Err(ArtifactError::TooLarge {
            size: length(bytes.len())?,
            max,
        });
    }
    Ok(bytes)
}

fn age_ms(metadata: &fs::Metadata, now_ms: u64) -> Result<u64, ArtifactError> {
    let modified = metadata
        .modified()
        .map_err(io_error)?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    Ok(now_ms.saturating_sub(modified))
}

fn length(value: usize) -> Result<u64, ArtifactError> {
    value
        .try_into()
        .map_err(|_| ArtifactError::InvalidMetadata("artifact length exceeds u64"))
}

fn io_error(error: std::io::Error) -> ArtifactError {
    ArtifactError::Io(error.to_string())
}

fn serialization(error: serde_json::Error) -> ArtifactError {
    ArtifactError::Serialization(error.to_string())
}

pub fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{CorrelationId, SessionId},
        event::{
            EventEnvelope, EventPayload, EventStore, MemoryEventStore, SchemaVersion, StreamVersion,
        },
    };

    fn artifact(
        root: &Path,
        retain_until_ms: u64,
    ) -> (FileArtifactStore, ArtifactMetadata, Vec<u8>) {
        let store = FileArtifactStore::open(root, 0).unwrap();
        let bytes = vec![b'a'; 128 * 1024];
        let metadata = store
            .put(
                &bytes,
                NewArtifact {
                    media_type: "application/octet-stream".into(),
                    creator: Principal::System,
                    source_revision: Some(WorkspaceVersion(StateVersion::from_digest([7; 32]))),
                    sensitivity: Sensitivity::Sensitive,
                    retain_until_ms,
                },
            )
            .unwrap();
        (store, metadata, bytes)
    }

    #[test]
    fn writes_before_reference_and_bounds_reads_and_collection() {
        let root = std::env::temp_dir().join(format!("arsy-artifact-test-{}", ArtifactId::new()));
        let now = unix_time_ms();
        let (store, metadata, bytes) = artifact(&root, now + 1_000);
        assert!(store.object_path(metadata.digest).is_file());
        assert!(store.reference_path(metadata.id).is_file());
        assert_eq!(metadata.size, bytes.len() as u64);
        assert_eq!(metadata.sensitivity, Sensitivity::Sensitive);
        assert!(metadata.source_revision.is_some());
        assert_eq!(
            store
                .read(
                    metadata.id,
                    ArtifactReadLimits {
                        max_bytes: metadata.size,
                        max_expansion_ratio: 10_000,
                    },
                )
                .unwrap(),
            bytes
        );
        assert!(matches!(
            store.read(
                metadata.id,
                ArtifactReadLimits {
                    max_bytes: metadata.size - 1,
                    max_expansion_ratio: 10_000,
                }
            ),
            Err(ArtifactError::TooLarge { .. })
        ));
        assert_eq!(store.gc(&HashSet::new(), now).unwrap(), GcReport::default());

        let event_store = MemoryEventStore::default();
        let stream = SessionId::new();
        let event = EventEnvelope::new(
            stream,
            1,
            Principal::System,
            None,
            CorrelationId::new(),
            SchemaVersion(1),
            "artifact.created",
            EventPayload::Artifact {
                reference: metadata.resource_ref(),
                media_type: metadata.media_type.clone(),
                size: metadata.size,
            },
        );
        assert_eq!(
            event_store
                .append(stream, StreamVersion(0), vec![event])
                .unwrap(),
            StreamVersion(1)
        );

        let report = store.gc(&HashSet::new(), now + 1_000).unwrap();
        assert_eq!(report.references_removed, 1);
        assert_eq!(report.objects_removed, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_excessive_decompression_ratio() {
        let root = std::env::temp_dir().join(format!("arsy-artifact-test-{}", ArtifactId::new()));
        let (store, metadata, _) = artifact(&root, 0);
        assert_eq!(metadata.compression, Compression::Zstd);
        assert_eq!(
            store.read(
                metadata.id,
                ArtifactReadLimits {
                    max_bytes: metadata.size,
                    max_expansion_ratio: 1,
                }
            ),
            Err(ArtifactError::ExpansionRatioExceeded)
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Collection removes the file the plan reported, not a path rebuilt from
    /// the digest.
    ///
    /// The two coincide while the store obeys its own layout. They stop
    /// coinciding on a store whose layout has drifted, and a garbage collector
    /// that errors instead of collecting is the one case where drift becomes
    /// permanent.
    #[test]
    fn collection_removes_the_orphan_it_found() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileArtifactStore::open(directory.path(), 0).unwrap();
        let digest = StateVersion::from_digest([0xab; 32]);
        // Deliberately not under the prefix `object_path` would derive.
        let misplaced = directory.path().join("objects/zz");
        fs::create_dir_all(&misplaced).unwrap();
        let stray = misplaced.join(format!("{digest}.blob"));
        fs::write(&stray, b"orphaned").unwrap();

        let plan = store.gc_plan(&HashSet::new(), u64::MAX).unwrap();
        assert_eq!(plan.objects.len(), 1);
        assert_eq!(plan.objects[0].path, stray, "the plan names what it found");
        assert_ne!(
            plan.objects[0].path,
            store.object_path(digest),
            "and that is not what the digest would rebuild"
        );

        let report = ArtifactStore::gc(&store, &HashSet::new(), u64::MAX).unwrap();
        assert_eq!(report.objects_removed, 1);
        assert!(!stray.exists(), "the orphan is gone");
    }
}
