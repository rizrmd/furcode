use std::path::{Path, PathBuf};

use anyhow::Result;
use furcode_app::FileReaderInfra;
use futures::{StreamExt, stream};

#[derive(Clone)]
pub struct FurcodeFileReadService;

impl Default for FurcodeFileReadService {
    fn default() -> Self {
        Self
    }
}

impl FurcodeFileReadService {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl FileReaderInfra for FurcodeFileReadService {
    async fn read_utf8(&self, path: &Path) -> Result<String> {
        furcode_fs::FurcodeFS::read_utf8(path).await
    }

    fn read_batch_utf8(
        &self,
        batch_size: usize,
        paths: Vec<PathBuf>,
    ) -> impl futures::Stream<Item = (PathBuf, anyhow::Result<String>)> + Send {
        let batches: Vec<Vec<PathBuf>> = paths
            .chunks(batch_size)
            .map(|chunk| chunk.to_vec())
            .collect();

        stream::iter(batches)
            .then(move |batch| async move {
                let futures = batch.into_iter().map(|path| async move {
                    let result = self.read_utf8(&path).await;
                    (path, result)
                });

                futures::future::join_all(futures).await
            })
            .flat_map(stream::iter)
    }

    async fn read(&self, path: &Path) -> Result<Vec<u8>> {
        furcode_fs::FurcodeFS::read(path).await
    }

    async fn range_read_utf8(
        &self,
        path: &Path,
        start_line: u64,
        end_line: u64,
    ) -> Result<(String, furcode_domain::FileInfo)> {
        furcode_fs::FurcodeFS::read_range_utf8(path, start_line, end_line).await
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use futures::StreamExt;
    use tempfile::NamedTempFile;

    use super::*;

    #[tokio::test]
    async fn test_read_batch_utf8() {
        let fixture = FurcodeFileReadService::new();

        // Create temporary test files
        let mut file1 = NamedTempFile::new().unwrap();
        let mut file2 = NamedTempFile::new().unwrap();
        let mut file3 = NamedTempFile::new().unwrap();

        writeln!(file1, "content1").unwrap();
        writeln!(file2, "content2").unwrap();
        writeln!(file3, "content3").unwrap();

        let paths = vec![
            file1.path().to_path_buf(),
            file2.path().to_path_buf(),
            file3.path().to_path_buf(),
        ];

        // Read with batch size of 2
        let stream = fixture.read_batch_utf8(2, paths.clone());
        futures::pin_mut!(stream);

        let item1 = stream.next().await.unwrap();
        assert_eq!(item1.0, paths[0]);
        assert_eq!(item1.1.as_deref().unwrap().trim(), "content1");

        let item2 = stream.next().await.unwrap();
        assert_eq!(item2.0, paths[1]);
        assert_eq!(item2.1.as_deref().unwrap().trim(), "content2");

        let item3 = stream.next().await.unwrap();
        assert_eq!(item3.0, paths[2]);
        assert_eq!(item3.1.as_deref().unwrap().trim(), "content3");

        // No more items
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_read_batch_utf8_single_batch() {
        let fixture = FurcodeFileReadService::new();

        let mut file1 = NamedTempFile::new().unwrap();
        let mut file2 = NamedTempFile::new().unwrap();

        writeln!(file1, "test1").unwrap();
        writeln!(file2, "test2").unwrap();

        let paths = vec![file1.path().to_path_buf(), file2.path().to_path_buf()];

        // Read with batch size larger than number of files
        let stream = fixture.read_batch_utf8(10, paths.clone());
        futures::pin_mut!(stream);

        let item1 = stream.next().await.unwrap();
        assert_eq!(item1.0, paths[0]);

        let item2 = stream.next().await.unwrap();
        assert_eq!(item2.0, paths[1]);

        // No more items
        assert!(stream.next().await.is_none());
    }
}
