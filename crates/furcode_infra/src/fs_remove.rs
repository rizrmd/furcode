use std::path::Path;

use furcode_app::FileRemoverInfra;

/// Low-level file remove service
///
/// Provides primitive file deletion operations without snapshot coordination.
/// Snapshot management should be handled at the service layer.
#[derive(Default)]
pub struct FurcodeFileRemoveService;

impl FurcodeFileRemoveService {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl FileRemoverInfra for FurcodeFileRemoveService {
    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        Ok(furcode_fs::FurcodeFS::remove_file(path).await?)
    }
}
