use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use axum_extra::extract::CookieJar;
use headers::Host;
use http::Method;

use agentenv_http_server::apis::volumes::*;
use agentenv_http_server::models;

use crate::sandbox::{normalize_mount_path, ExtraDrive};
use crate::snapshot::SnapshotVolume;
use crate::volume::{
    VolumeError, VolumeManager, VolumeMode, VolumeRecord, VolumeStatus, DEFAULT_VOLUME_PAGE_SIZE,
    DEFAULT_VOLUME_SIZE_MB,
};

use super::ApiImpl;

pub(super) struct StagedSnapshotDrives {
    pub(super) drives: Vec<ExtraDrive>,
    pub(super) _dir: tempfile::TempDir,
}

pub(super) async fn stage_snapshot_volume_drives(
    manager: &VolumeManager,
    snapshots: &[SnapshotVolume],
) -> Result<StagedSnapshotDrives, models::Error> {
    let root = manager.staging_root();
    std::fs::create_dir_all(&root)
        .map_err(|error| ApiImpl::error(500, format!("create volume staging root: {error}")))?;
    let dir = tempfile::tempdir_in(root).map_err(|error| {
        ApiImpl::error(500, format!("create volume staging directory: {error}"))
    })?;
    let mut snapshots = snapshots
        .iter()
        .map(|snapshot| {
            normalize_mount_path(PathBuf::from(&snapshot.mount_path))
                .map(|path| (path, snapshot))
                .map_err(|error| ApiImpl::error(400, error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // resolve_volume_mounts assigns reserved slots in mount-path order. Snapshot
    // manifests have no slot field, so restore the same order before loading memory.
    snapshots.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    let mut drives = Vec::with_capacity(snapshots.len());
    for (index, (mount_path, snapshot)) in snapshots.into_iter().enumerate() {
        let config = manager
            .materialize_staging_layers(
                &snapshot.layers,
                &dir.path().join(index.to_string()).join("image.json"),
            )
            .await
            .map_err(|error| error_response(error).1)?;
        let drive = ExtraDrive::try_new_overlaybd_with_mount_path(
            format!("staged_{index}"),
            config,
            snapshot.mode == VolumeMode::ReadOnly,
            mount_path,
            None::<PathBuf>,
        )
        .and_then(|drive| {
            drive.try_with_virtual_size(
                snapshot
                    .size_mb
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| anyhow::anyhow!("volume size is too large"))?,
            )
        })
        .map_err(|error| ApiImpl::error(400, error.to_string()))?;
        drives.push(drive);
    }
    Ok(StagedSnapshotDrives { drives, _dir: dir })
}

pub(super) async fn resolve_volume_mounts(
    manager: &VolumeManager,
    mounts: &HashMap<String, String>,
    owner: &str,
) -> Result<(Vec<ExtraDrive>, HashMap<String, String>), models::Error> {
    let mut reserved = Vec::with_capacity(mounts.len());
    let result = resolve_volume_mounts_inner(manager, mounts, owner, &mut reserved).await;
    if result.is_err() {
        if let Err(error) = manager.replace_owner_for(owner, None, &reserved).await {
            tracing::warn!(%owner, %error, "failed to clean up volume reservations after mount resolution failed");
        }
    }
    result
}

async fn resolve_volume_mounts_inner(
    manager: &VolumeManager,
    mounts: &HashMap<String, String>,
    owner: &str,
    reserved: &mut Vec<String>,
) -> Result<(Vec<ExtraDrive>, HashMap<String, String>), models::Error> {
    let mut drives = Vec::with_capacity(mounts.len());
    let limits = manager.limits();
    if mounts.len() > limits.max_mounts {
        return Err(error_response(VolumeError::TooManyMountedVolumes {
            max_count: limits.max_mounts,
        })
        .1);
    }
    let mut volume_ids = HashSet::with_capacity(mounts.len());
    let mut normalized_mounts = HashMap::with_capacity(mounts.len());

    let mut mounts = mounts.iter().collect::<Vec<_>>();
    mounts.sort_unstable_by_key(|(mount_path, _)| *mount_path);
    for (mount_path, reference) in mounts {
        let mount_path = normalize_mount_path(PathBuf::from(mount_path))
            .map_err(|error| ApiImpl::error(400, error.to_string()))?;
        if normalized_mounts.keys().any(|existing| {
            let existing = PathBuf::from(existing);
            existing.starts_with(&mount_path) || mount_path.starts_with(existing)
        }) {
            return Err(ApiImpl::error(
                400,
                format!(
                    "volume mount path overlaps another mount: {}",
                    mount_path.display()
                ),
            ));
        }
        let volume = manager
            .get(reference)
            .await
            .map_err(|error| error_response(error).1)?;
        if volume.size_mb > limits.max_size_mb {
            return Err(error_response(VolumeError::SizeLimitExceeded {
                max_size_mb: limits.max_size_mb,
            })
            .1);
        }
        if !volume_ids.insert(volume.id.clone()) {
            return Err(ApiImpl::error(
                400,
                format!("volume {} is mounted more than once", volume.id),
            ));
        }
        manager
            .reserve(&volume.id, owner)
            .await
            .map_err(|error| error_response(error).1)?;
        reserved.push(volume.id.clone());

        let volume = manager
            .materialize_backing(&volume.id)
            .await
            .map_err(|error| error_response(error).1)?;
        let image_config_path = volume.backing_image_config.clone().ok_or_else(|| {
            ApiImpl::error(500, format!("volume {} has no backing image", volume.id))
        })?;

        let normalized_path = mount_path.to_string_lossy().into_owned();
        let drive = ExtraDrive::try_new_overlaybd_with_mount_path(
            volume.id.clone(),
            image_config_path,
            volume.mode == VolumeMode::ReadOnly,
            mount_path,
            None::<PathBuf>,
        )
        .and_then(|drive| {
            drive.try_with_virtual_size(
                volume
                    .size_mb
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| anyhow::anyhow!("volume size is too large"))?,
            )
        })
        .map_err(|error| ApiImpl::error(400, error.to_string()))?;
        let snapshot_output_dir =
            (volume.mode == VolumeMode::Exclusive).then(|| manager.data_dir(&volume.id));
        drives.push(drive.with_volume_snapshot_output_dir(snapshot_output_dir));
        normalized_mounts.insert(normalized_path, volume.id);
    }

    Ok((drives, normalized_mounts))
}

impl From<VolumeRecord> for models::Volume {
    fn from(record: VolumeRecord) -> Self {
        let status = match record.status {
            VolumeStatus::Ready => "ready",
            VolumeStatus::Uploading => "uploading",
            VolumeStatus::Failed => "failed",
        }
        .to_string();
        let mode = match record.mode {
            VolumeMode::ReadOnly => "ro".to_owned(),
            VolumeMode::Exclusive => "exclusive".to_owned(),
        };
        models::Volume {
            volume_id: record.id,
            name: record.name,
            mode,
            size_mb: record.size_mb,
            status,
        }
    }
}

pub(super) fn error_response(error: VolumeError) -> (i32, models::Error) {
    let code = match error {
        VolumeError::InvalidName
        | VolumeError::MultipleSources
        | VolumeError::SourceNotFound(_)
        | VolumeError::InvalidSize
        | VolumeError::InvalidNextToken
        | VolumeError::InvalidPageLimit
        | VolumeError::SizeLimitExceeded { .. }
        | VolumeError::SizeMismatch => 400,
        VolumeError::TooManyMountedVolumes { .. } => 400,
        VolumeError::NotFound(_) => 404,
        VolumeError::NameConflict(_)
        | VolumeError::Reserved(_)
        | VolumeError::Uploading(_)
        | VolumeError::Failed(_) => 409,
        VolumeError::Storage(_) => 500,
    };
    (code, ApiImpl::error(code, error.to_string()))
}

#[async_trait]
impl Volumes<()> for ApiImpl {
    type Claims = super::Claims;

    async fn volumes_get(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        query_params: &models::VolumesGetQueryParams,
    ) -> Result<VolumesGetResponse, ()> {
        match self
            .volume_manager
            .list_page(
                query_params.next_token.as_deref(),
                query_params
                    .limit
                    .map(|limit| limit as usize)
                    .unwrap_or(DEFAULT_VOLUME_PAGE_SIZE),
            )
            .await
        {
            Ok(page) => Ok(
                VolumesGetResponse::Status200_SuccessfullyListedTeamVolumes {
                    body: page.records.into_iter().map(Into::into).collect(),
                    x_next_token: page.next_token,
                },
            ),
            Err(error @ (VolumeError::InvalidNextToken | VolumeError::InvalidPageLimit)) => Ok(
                VolumesGetResponse::Status400_BadRequest(error_response(error).1),
            ),
            Err(error) => Ok(VolumesGetResponse::Status500_ServerError(ApiImpl::error(
                500,
                error.to_string(),
            ))),
        }
    }

    async fn volumes_post(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        body: &models::NewVolume,
    ) -> Result<VolumesPostResponse, ()> {
        let mode = match body.mode.as_deref() {
            None | Some("exclusive") => VolumeMode::Exclusive,
            Some("ro") => VolumeMode::ReadOnly,
            Some(mode) => {
                return Ok(VolumesPostResponse::Status400_BadRequest(ApiImpl::error(
                    400,
                    format!("unsupported volume mode: {mode}"),
                )))
            }
        };
        let resolved_image = match (body.from_volume.is_none(), body.image.as_deref()) {
            (true, Some(image)) => match self.image_resolver.resolve(image).await {
                Ok(resolved) => Some(resolved.overlaybd_config_path),
                Err(error) => {
                    let code = if error.is_user_error() { 400 } else { 500 };
                    return Ok(match code {
                        400 => VolumesPostResponse::Status400_BadRequest(ApiImpl::error(
                            code,
                            format!("resolve volume image: {error:#}"),
                        )),
                        _ => VolumesPostResponse::Status500_ServerError(ApiImpl::error(
                            code,
                            format!("resolve volume image: {error:#}"),
                        )),
                    });
                }
            },
            _ => None,
        };

        match self
            .volume_manager
            .create(
                body.name.clone(),
                mode,
                body.from_volume.clone(),
                resolved_image,
                body.size_mb.unwrap_or(DEFAULT_VOLUME_SIZE_MB),
            )
            .await
        {
            Ok(record) => {
                Ok(VolumesPostResponse::Status201_SuccessfullyCreatedANewTeamVolume(record.into()))
            }
            Err(error) => {
                let (code, error) = error_response(error);
                match code {
                    400 => Ok(VolumesPostResponse::Status400_BadRequest(error)),
                    409 => Ok(VolumesPostResponse::Status409_Conflict(error)),
                    _ => Ok(VolumesPostResponse::Status500_ServerError(error)),
                }
            }
        }
    }

    async fn volumes_volume_id_delete(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::VolumesVolumeIdDeletePathParams,
    ) -> Result<VolumesVolumeIdDeleteResponse, ()> {
        match self.volume_manager.delete(&path_params.volume_id).await {
            Ok(()) => Ok(VolumesVolumeIdDeleteResponse::Status204_SuccessfullyDeletedATeamVolume),
            Err(error) => {
                let (code, error) = error_response(error);
                match code {
                    404 => Ok(VolumesVolumeIdDeleteResponse::Status404_NotFound(error)),
                    409 => Ok(VolumesVolumeIdDeleteResponse::Status409_Conflict(error)),
                    _ => Ok(VolumesVolumeIdDeleteResponse::Status500_ServerError(error)),
                }
            }
        }
    }

    async fn volumes_volume_id_get(
        &self,
        _method: &Method,
        _host: &Host,
        _cookies: &CookieJar,
        _claims: &Self::Claims,
        path_params: &models::VolumesVolumeIdGetPathParams,
    ) -> Result<VolumesVolumeIdGetResponse, ()> {
        match self.volume_manager.get(&path_params.volume_id).await {
            Ok(record) => Ok(
                VolumesVolumeIdGetResponse::Status200_SuccessfullyRetrievedATeamVolume(
                    record.into(),
                ),
            ),
            Err(error) => {
                let (code, error) = error_response(error);
                match code {
                    404 => Ok(VolumesVolumeIdGetResponse::Status404_NotFound(error)),
                    _ => Ok(VolumesVolumeIdGetResponse::Status500_ServerError(error)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::backends::{PosixFsBackend, PosixFsBackendConfig};

    #[tokio::test]
    async fn staged_snapshot_drives_preserve_slot_order_mode_and_size_without_volumes() {
        let root = tempfile::tempdir().unwrap();
        let backend = PosixFsBackend::new(PosixFsBackendConfig {
            root: root.path().join("repository"),
            cache_root: Some(root.path().join("cache")),
            runtime_cache_root: Some(root.path().join("runtime")),
        })
        .unwrap();
        let manager =
            VolumeManager::open_with_repository(root.path().join("volumes"), backend.repository())
                .await
                .unwrap();
        let layers = vec![crate::snapshot::OverlaybdLayerRef::External(
            crate::snapshot::ExternalLayer {
                repo_blob_url: "https://registry.example/v2/data/blobs".into(),
                digest: format!("sha256:{}", "0".repeat(64)),
                size: 4096,
            },
        )];
        let snapshots = vec![
            SnapshotVolume {
                mount_path: "/z//data/".into(),
                mode: VolumeMode::Exclusive,
                size_mb: 32,
                layers: layers.clone(),
            },
            SnapshotVolume {
                mount_path: "/a".into(),
                mode: VolumeMode::ReadOnly,
                size_mb: 16,
                layers,
            },
        ];
        let staged = stage_snapshot_volume_drives(&manager, &snapshots)
            .await
            .unwrap();
        assert_eq!(staged.drives.len(), 2);
        for (index, (path, read_only, size)) in [("/a", true, 16), ("/z/data", false, 32)]
            .into_iter()
            .enumerate()
        {
            let drive = &staged.drives[index];
            assert_eq!(drive.drive_id(), format!("staged_{index}"));
            assert_eq!(drive.mount_path(), std::path::Path::new(path));
            assert_eq!(drive.read_only(), read_only);
            assert_eq!(drive.virtual_size(), Some(size * 1024 * 1024));
            assert!(!drive.is_volume());
            assert!(drive.snapshot_output_dir().is_none());
            let config = overlaybd::config::load_image_config(drive.image_config_path()).unwrap();
            assert!(config.upper.data.is_empty());
            assert!(manager.get(drive.drive_id()).await.is_err());
        }
        let staging_path = staged._dir.path().to_path_buf();
        drop(staged);
        assert!(!staging_path.exists());
        assert!(manager
            .list_page(None, 100)
            .await
            .unwrap()
            .records
            .is_empty());
    }
}
