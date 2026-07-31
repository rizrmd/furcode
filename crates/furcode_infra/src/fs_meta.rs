use std::path::Path;

use anyhow::Result;
use furcode_app::FileInfoInfra;

pub struct FurcodeFileMetaService;
#[async_trait::async_trait]
impl FileInfoInfra for FurcodeFileMetaService {
    async fn is_file(&self, path: &Path) -> Result<bool> {
        Ok(furcode_fs::FurcodeFS::is_file(path))
    }

    async fn is_binary(&self, path: &Path) -> Result<bool> {
        furcode_fs::FurcodeFS::is_binary_file(path).await
    }

    async fn exists(&self, path: &Path) -> Result<bool> {
        Ok(furcode_fs::FurcodeFS::exists(path))
    }

    async fn file_size(&self, path: &Path) -> Result<u64> {
        furcode_fs::FurcodeFS::file_size(path).await
    }
}
