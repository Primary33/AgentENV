use anyhow::Result;

use super::{
    service::ImageCacheService,
    source_config::{
        read_source_image_metadata, source_image_config_is_usable,
        source_image_metadata_path_for_config_path,
    },
};

impl ImageCacheService {
    /// Inventory the same unscoped cache entries that resolve_local can consume.
    /// Do not touch their access times: heartbeat reporting must not defeat GC.
    pub(crate) async fn local_image_digests(&self) -> Result<Vec<String>> {
        let mut entries = match tokio::fs::read_dir(&self.config_dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut digests = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let Some(hex) = name
                .to_str()
                .and_then(|name| name.strip_prefix("sha256-"))
                .and_then(|name| name.strip_suffix("-image.json"))
            else {
                continue;
            };
            let digest = format!("sha256:{hex}");
            if crate::image::buildkit::validate_digest(&digest).is_err() {
                continue;
            }
            let path = entry.path();
            if !source_image_config_is_usable(&path) {
                continue;
            }
            // A concurrent eviction is an absent entry, not a failed heartbeat.
            match read_source_image_metadata(&source_image_metadata_path_for_config_path(&path))
                .await
            {
                Ok(Some(_)) => digests.push(digest),
                Ok(_) => {}
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
                Err(error) => tracing::warn!(
                    path = %path.display(), error = %format_args!("{error:#}"),
                    "omitting invalid image metadata from cache inventory"
                ),
            }
        }
        digests.sort();
        Ok(digests)
    }
}
