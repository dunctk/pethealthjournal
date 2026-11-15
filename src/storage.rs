//! Where pet photos and original lab reports live.
//!
//! Local disk by default (self-hosted). When `S3_*` settings are present
//! (the hosted service uses Cloudflare R2), new files are written to the
//! bucket. Reads try the bucket first and fall back to local disk, so files
//! written before object storage was switched on keep working.
//!
//! Files are never served from the bucket directly: every read goes through
//! the app's household-scoped routes.

use crate::config::{Config, ObjectStoreConfig};
use anyhow::{Context, anyhow, bail};
use object_store::{
    ObjectStore, ObjectStoreExt, PutPayload, aws::AmazonS3Builder, path::Path as ObjectPath,
};
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// A stored file, identified the same way whatever the backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKey<'a> {
    Photo {
        household_id: i64,
        pet_id: i64,
        storage_name: &'a str,
    },
    /// Path relative to the household's blood-test directory, as stored in
    /// `lab_reports.original_file` (e.g. `originals/report-3-abc.pdf`).
    Report {
        household_id: i64,
        relative: &'a str,
    },
}

pub struct FileStore {
    photos_dir: PathBuf,
    reports_dir: PathBuf,
    remote: Option<Remote>,
}

struct Remote {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl FileStore {
    pub fn from_config(config: &Config) -> anyhow::Result<Self> {
        let remote = config
            .object_store
            .as_ref()
            .map(|settings| -> anyhow::Result<Remote> {
                Ok(Remote {
                    store: Arc::new(build_s3(settings)?),
                    prefix: settings.prefix.clone(),
                })
            })
            .transpose()?;
        Ok(Self {
            photos_dir: PathBuf::from(&config.pet_photos_dir),
            reports_dir: PathBuf::from(&config.blood_tests_dir),
            remote,
        })
    }

    #[cfg(test)]
    pub fn with_store(config: &Config, store: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        Self {
            photos_dir: PathBuf::from(&config.pet_photos_dir),
            reports_dir: PathBuf::from(&config.blood_tests_dir),
            remote: Some(Remote {
                store,
                prefix: prefix.to_owned(),
            }),
        }
    }

    pub fn remote_enabled(&self) -> bool {
        self.remote.is_some()
    }

    pub async fn put(&self, key: &FileKey<'_>, bytes: Vec<u8>) -> anyhow::Result<()> {
        if let Some(remote) = &self.remote {
            remote
                .store
                .put(&remote.path(key)?, PutPayload::from(bytes))
                .await
                .context("failed to store file in object storage")?;
            return Ok(());
        }
        let path = self.local_path(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        tokio::fs::write(&path, bytes)
            .await
            .with_context(|| format!("failed to store {}", path.display()))
    }

    /// `None` when the file exists in neither place.
    pub async fn get(&self, key: &FileKey<'_>) -> anyhow::Result<Option<Vec<u8>>> {
        if let Some(remote) = &self.remote {
            match remote.store.get(&remote.path(key)?).await {
                Ok(result) => return Ok(Some(result.bytes().await?.to_vec())),
                Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(anyhow!(error).context("object storage read failed")),
            }
        }
        match tokio::fs::read(self.local_path(key)?).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Best-effort removal from both places; missing files are fine.
    pub async fn delete(&self, key: &FileKey<'_>) {
        if let Some(remote) = &self.remote
            && let Ok(path) = remote.path(key)
            && let Err(error) = remote.store.delete(&path).await
            && !matches!(error, object_store::Error::NotFound { .. })
        {
            tracing::warn!(%error, "object storage delete failed");
        }
        if let Ok(path) = self.local_path(key) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// Remove every photo of one pet, from both places.
    pub async fn delete_pet_photos(&self, household_id: i64, pet_id: i64) {
        if let Some(remote) = &self.remote {
            use futures_util::StreamExt;
            let prefix =
                ObjectPath::from(format!("{}photos/{household_id}/{pet_id}", remote.prefix));
            let mut listing = remote.store.list(Some(&prefix));
            while let Some(entry) = listing.next().await {
                match entry {
                    Ok(meta) => {
                        if let Err(error) = remote.store.delete(&meta.location).await {
                            tracing::warn!(%error, "object storage delete failed");
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "object storage listing failed");
                        break;
                    }
                }
            }
        }
        let _ = tokio::fs::remove_dir_all(
            self.photos_dir
                .join(household_id.to_string())
                .join(pet_id.to_string()),
        )
        .await;
    }

    fn local_path(&self, key: &FileKey<'_>) -> anyhow::Result<PathBuf> {
        match key {
            FileKey::Photo {
                household_id,
                pet_id,
                storage_name,
            } => Ok(self
                .photos_dir
                .join(positive(*household_id)?.to_string())
                .join(positive(*pet_id)?.to_string())
                .join(single_segment(storage_name)?)),
            FileKey::Report {
                household_id,
                relative,
            } => Ok(self
                .reports_dir
                .join(positive(*household_id)?.to_string())
                .join(contained_relative(relative)?)),
        }
    }
}

impl Remote {
    fn path(&self, key: &FileKey<'_>) -> anyhow::Result<ObjectPath> {
        let key = match key {
            FileKey::Photo {
                household_id,
                pet_id,
                storage_name,
            } => format!(
                "photos/{}/{}/{}",
                positive(*household_id)?,
                positive(*pet_id)?,
                single_segment(storage_name)?
            ),
            FileKey::Report {
                household_id,
                relative,
            } => format!(
                "reports/{}/{}",
                positive(*household_id)?,
                contained_relative(relative)?.to_string_lossy()
            ),
        };
        Ok(ObjectPath::parse(format!("{}{key}", self.prefix))?)
    }
}

fn build_s3(settings: &ObjectStoreConfig) -> anyhow::Result<object_store::aws::AmazonS3> {
    AmazonS3Builder::new()
        .with_endpoint(&settings.endpoint)
        .with_bucket_name(&settings.bucket)
        .with_region(&settings.region)
        .with_access_key_id(&settings.access_key_id)
        .with_secret_access_key(&settings.secret_access_key)
        .with_virtual_hosted_style_request(false)
        .build()
        .context("invalid S3 object storage settings")
}

fn positive(id: i64) -> anyhow::Result<i64> {
    if id < 1 {
        bail!("ids must be positive");
    }
    Ok(id)
}

fn single_segment(name: &str) -> anyhow::Result<&str> {
    if name.is_empty()
        || name.contains(['/', '\\'])
        || name == "."
        || name == ".."
        || name.starts_with('.')
    {
        bail!("invalid file name");
    }
    Ok(name)
}

/// A relative path that cannot escape its household directory.
fn contained_relative(relative: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid stored file path");
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    fn config(root: &Path) -> Config {
        crate::ocr::tests::test_config(root)
    }

    #[tokio::test]
    async fn local_round_trip_and_escape_attempts_rejected() {
        let root = tempfile::tempdir().unwrap();
        let store = FileStore::from_config(&config(root.path())).unwrap();
        let key = FileKey::Photo {
            household_id: 1,
            pet_id: 2,
            storage_name: "abc.jpg",
        };
        store.put(&key, b"jpeg".to_vec()).await.unwrap();
        assert_eq!(
            store.get(&key).await.unwrap().as_deref(),
            Some(&b"jpeg"[..])
        );
        store.delete(&key).await;
        assert_eq!(store.get(&key).await.unwrap(), None);

        for relative in ["../2/originals/x.pdf", "/etc/passwd", "originals/../../x"] {
            let key = FileKey::Report {
                household_id: 1,
                relative,
            };
            assert!(store.put(&key, b"x".to_vec()).await.is_err(), "{relative}");
        }
        let key = FileKey::Photo {
            household_id: 1,
            pet_id: 2,
            storage_name: "../x.jpg",
        };
        assert!(store.put(&key, b"x".to_vec()).await.is_err());
    }

    #[tokio::test]
    async fn bucket_writes_and_falls_back_to_local_files() {
        let root = tempfile::tempdir().unwrap();
        let cfg = config(root.path());
        // A file written before object storage was enabled.
        let local_only = FileStore::from_config(&cfg).unwrap();
        let old = FileKey::Report {
            household_id: 3,
            relative: "originals/report-1-aaa.pdf",
        };
        local_only.put(&old, b"old pdf".to_vec()).await.unwrap();

        let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = FileStore::with_store(&cfg, memory.clone(), "files/");
        assert_eq!(
            store.get(&old).await.unwrap().as_deref(),
            Some(&b"old pdf"[..])
        );

        let new = FileKey::Photo {
            household_id: 3,
            pet_id: 9,
            storage_name: "new.png",
        };
        store.put(&new, b"png".to_vec()).await.unwrap();
        assert!(
            memory
                .get(&ObjectPath::from("files/photos/3/9/new.png"))
                .await
                .is_ok(),
            "new files land in the bucket under the prefix"
        );
        assert!(
            local_only.get(&new).await.unwrap().is_none(),
            "and not on local disk"
        );
        store.delete_pet_photos(3, 9).await;
        assert_eq!(store.get(&new).await.unwrap(), None);
    }
}
