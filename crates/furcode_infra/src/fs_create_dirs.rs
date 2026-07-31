use std::path::Path;

use furcode_app::FileDirectoryInfra;

#[derive(Default)]
pub struct FurcodeCreateDirsService;

#[async_trait::async_trait]
impl FileDirectoryInfra for FurcodeCreateDirsService {
    async fn create_dirs(&self, path: &Path) -> anyhow::Result<()> {
        Ok(furcode_fs::FurcodeFS::create_dir_all(path).await?)
    }
}
