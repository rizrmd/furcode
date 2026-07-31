use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use bytes::Bytes;
use furcode_app::{
    CommandInfra, DirectoryReaderInfra, EnvironmentInfra, FileDirectoryInfra, FileInfoInfra,
    FileReaderInfra, FileRemoverInfra, FileWriterInfra, GrpcInfra, HttpInfra, McpServerInfra,
    StrategyFactory, UserInfra, WalkerInfra,
};
use furcode_domain::{
    AuthMethod, CommandOutput, FileInfo as FileInfoData, McpServerConfig, ProviderId, URLParamSpec,
};
use furcode_eventsource::EventSource;
use reqwest::header::HeaderMap;
use reqwest::{Response, Url};

use crate::auth::{AnyAuthStrategy, FurcodeAuthStrategyFactory};
use crate::console::StdConsoleWriter;
use crate::env::{FurcodeEnvironmentInfra, to_environment};
use crate::executor::FurcodeCommandExecutorService;
use crate::fs_create_dirs::FurcodeCreateDirsService;
use crate::fs_meta::FurcodeFileMetaService;
use crate::fs_read::FurcodeFileReadService;
use crate::fs_read_dir::FurcodeDirectoryReaderService;
use crate::fs_remove::FurcodeFileRemoveService;
use crate::fs_write::FurcodeFileWriteService;
use crate::grpc::FurcodeGrpcClient;
use crate::http::FurcodeHttpInfra;
use crate::inquire::FurcodeInquire;
use crate::mcp_client::FurcodeMcpClient;
use crate::mcp_server::FurcodeMcpServer;
use crate::walker::FurcodeWalkerService;

#[derive(Clone)]
pub struct FurcodeInfra {
    // TODO: Drop the "Service" suffix. Use names like FurcodeFileReader, FurcodeFileWriter,
    // FurcodeHttpClient etc.
    file_read_service: Arc<FurcodeFileReadService>,
    file_write_service: Arc<FurcodeFileWriteService>,
    file_remove_service: Arc<FurcodeFileRemoveService>,
    config_infra: Arc<FurcodeEnvironmentInfra>,
    file_meta_service: Arc<FurcodeFileMetaService>,
    create_dirs_service: Arc<FurcodeCreateDirsService>,
    directory_reader_service: Arc<FurcodeDirectoryReaderService>,
    command_executor_service: Arc<FurcodeCommandExecutorService>,
    inquire_service: Arc<FurcodeInquire>,
    mcp_server: FurcodeMcpServer,
    walker_service: Arc<FurcodeWalkerService>,
    http_service: Arc<FurcodeHttpInfra<FurcodeFileWriteService>>,
    strategy_factory: Arc<FurcodeAuthStrategyFactory>,
    grpc_client: Arc<FurcodeGrpcClient>,
    output_printer: Arc<StdConsoleWriter>,
}

impl FurcodeInfra {
    /// Creates a new [`FurcodeInfra`] with all infrastructure services
    /// initialized.
    ///
    /// # Arguments
    /// * `cwd` - The working directory for command execution and environment
    ///   resolution
    /// * `config` - Pre-read application configuration; used only at
    ///   construction time to initialize infrastructure services
    /// * `services_url` - Pre-validated URL for the gRPC workspace server
    pub fn new(cwd: PathBuf, config: furcode_config::FurcodeConfig) -> Self {
        let env = to_environment(cwd.clone());
        let config_infra = Arc::new(FurcodeEnvironmentInfra::new(cwd, config.clone()));
        let file_write_service = Arc::new(FurcodeFileWriteService::new());
        let config = config_infra.cached_config().unwrap_or(config);

        let http_service = Arc::new(FurcodeHttpInfra::new(
            config.clone(),
            file_write_service.clone(),
        ));
        let file_read_service = Arc::new(FurcodeFileReadService::new());
        let file_meta_service = Arc::new(FurcodeFileMetaService);
        let directory_reader_service = Arc::new(FurcodeDirectoryReaderService::new(
            config_infra
                .cached_config()
                .map(|c| c.max_parallel_file_reads)
                .unwrap_or(4),
        ));
        let grpc_client = Arc::new(FurcodeGrpcClient::new(config.services_url.clone()));
        let output_printer = Arc::new(StdConsoleWriter::default());

        Self {
            file_read_service,
            file_write_service,
            file_remove_service: Arc::new(FurcodeFileRemoveService::new()),
            config_infra,
            file_meta_service,
            create_dirs_service: Arc::new(FurcodeCreateDirsService),
            directory_reader_service,
            command_executor_service: Arc::new(FurcodeCommandExecutorService::new(
                env.clone(),
                output_printer.clone(),
            )),
            inquire_service: Arc::new(FurcodeInquire::new()),
            mcp_server: FurcodeMcpServer,
            walker_service: Arc::new(FurcodeWalkerService::new()),
            strategy_factory: Arc::new(FurcodeAuthStrategyFactory::new(env.clone())),
            http_service,
            grpc_client,
            output_printer,
        }
    }
}

impl FurcodeInfra {
    /// Returns the current application configuration, re-reading from disk if
    /// the cache was invalidated by a prior `update_environment` call.
    ///
    /// # Errors
    ///
    /// Returns an error if the disk read fails.
    pub fn config(&self) -> anyhow::Result<furcode_config::FurcodeConfig> {
        self.config_infra.cached_config()
    }
}

impl EnvironmentInfra for FurcodeInfra {
    type Config = furcode_config::FurcodeConfig;

    fn get_env_var(&self, key: &str) -> Option<String> {
        self.config_infra.get_env_var(key)
    }

    fn get_env_vars(&self) -> BTreeMap<String, String> {
        self.config_infra.get_env_vars()
    }

    fn get_environment(&self) -> furcode_domain::Environment {
        self.config_infra.get_environment()
    }

    fn get_config(&self) -> anyhow::Result<furcode_config::FurcodeConfig> {
        self.config_infra.get_config()
    }

    async fn update_environment(
        &self,
        ops: Vec<furcode_domain::ConfigOperation>,
    ) -> anyhow::Result<()> {
        self.config_infra.update_environment(ops).await
    }
}

#[async_trait::async_trait]
impl FileReaderInfra for FurcodeInfra {
    async fn read_utf8(&self, path: &Path) -> anyhow::Result<String> {
        self.file_read_service.read_utf8(path).await
    }

    fn read_batch_utf8(
        &self,
        batch_size: usize,
        paths: Vec<PathBuf>,
    ) -> impl futures::Stream<Item = (PathBuf, anyhow::Result<String>)> + Send {
        self.file_read_service.read_batch_utf8(batch_size, paths)
    }

    async fn read(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        self.file_read_service.read(path).await
    }

    async fn range_read_utf8(
        &self,
        path: &Path,
        start_line: u64,
        end_line: u64,
    ) -> anyhow::Result<(String, FileInfoData)> {
        self.file_read_service
            .range_read_utf8(path, start_line, end_line)
            .await
    }
}

#[async_trait::async_trait]
impl FileWriterInfra for FurcodeInfra {
    async fn write(&self, path: &Path, contents: Bytes) -> anyhow::Result<()> {
        self.file_write_service.write(path, contents).await
    }

    async fn append(&self, path: &Path, contents: Bytes) -> anyhow::Result<()> {
        self.file_write_service.append(path, contents).await
    }

    async fn write_temp(&self, prefix: &str, ext: &str, content: &str) -> anyhow::Result<PathBuf> {
        self.file_write_service
            .write_temp(prefix, ext, content)
            .await
    }
}

#[async_trait::async_trait]
impl FileInfoInfra for FurcodeInfra {
    async fn is_binary(&self, path: &Path) -> anyhow::Result<bool> {
        self.file_meta_service.is_binary(path).await
    }

    async fn is_file(&self, path: &Path) -> anyhow::Result<bool> {
        self.file_meta_service.is_file(path).await
    }

    async fn exists(&self, path: &Path) -> anyhow::Result<bool> {
        self.file_meta_service.exists(path).await
    }

    async fn file_size(&self, path: &Path) -> anyhow::Result<u64> {
        self.file_meta_service.file_size(path).await
    }
}
#[async_trait::async_trait]
impl FileRemoverInfra for FurcodeInfra {
    async fn remove(&self, path: &Path) -> anyhow::Result<()> {
        self.file_remove_service.remove(path).await
    }
}

#[async_trait::async_trait]
impl FileDirectoryInfra for FurcodeInfra {
    async fn create_dirs(&self, path: &Path) -> anyhow::Result<()> {
        self.create_dirs_service.create_dirs(path).await
    }
}

#[async_trait::async_trait]
impl CommandInfra for FurcodeInfra {
    async fn execute_command(
        &self,
        command: String,
        working_dir: PathBuf,
        silent: bool,
        env_vars: Option<Vec<String>>,
    ) -> anyhow::Result<CommandOutput> {
        self.command_executor_service
            .execute_command(command, working_dir, silent, env_vars)
            .await
    }

    async fn execute_command_raw(
        &self,
        command: &str,
        working_dir: PathBuf,
        env_vars: Option<Vec<String>>,
    ) -> anyhow::Result<ExitStatus> {
        self.command_executor_service
            .execute_command_raw(command, working_dir, env_vars)
            .await
    }
}

#[async_trait::async_trait]
impl UserInfra for FurcodeInfra {
    async fn prompt_question(&self, question: &str) -> anyhow::Result<Option<String>> {
        self.inquire_service.prompt_question(question).await
    }

    async fn select_one<T: Clone + std::fmt::Display + Send + 'static>(
        &self,
        message: &str,
        options: Vec<T>,
    ) -> anyhow::Result<Option<T>> {
        self.inquire_service.select_one(message, options).await
    }

    async fn select_many<T: std::fmt::Display + Clone + Send + 'static>(
        &self,
        message: &str,
        options: Vec<T>,
    ) -> anyhow::Result<Option<Vec<T>>> {
        self.inquire_service.select_many(message, options).await
    }
}

#[async_trait::async_trait]
impl McpServerInfra for FurcodeInfra {
    type Client = FurcodeMcpClient;

    async fn connect(
        &self,
        config: McpServerConfig,
        env_vars: &BTreeMap<String, String>,
        environment: &furcode_domain::Environment,
    ) -> anyhow::Result<Self::Client> {
        self.mcp_server.connect(config, env_vars, environment).await
    }
}

#[async_trait::async_trait]
impl WalkerInfra for FurcodeInfra {
    async fn walk(&self, config: furcode_app::Walker) -> anyhow::Result<Vec<furcode_app::WalkedFile>> {
        self.walker_service.walk(config).await
    }
}

#[async_trait::async_trait]
impl HttpInfra for FurcodeInfra {
    async fn http_get(&self, url: &Url, headers: Option<HeaderMap>) -> anyhow::Result<Response> {
        self.http_service.http_get(url, headers).await
    }

    async fn http_post(
        &self,
        url: &Url,
        headers: Option<HeaderMap>,
        body: Bytes,
    ) -> anyhow::Result<Response> {
        self.http_service.http_post(url, headers, body).await
    }

    async fn http_delete(&self, url: &Url) -> anyhow::Result<Response> {
        self.http_service.http_delete(url).await
    }

    async fn http_eventsource(
        &self,
        url: &Url,
        headers: Option<HeaderMap>,
        body: Bytes,
    ) -> anyhow::Result<EventSource> {
        self.http_service.http_eventsource(url, headers, body).await
    }
}
#[async_trait::async_trait]
impl DirectoryReaderInfra for FurcodeInfra {
    async fn list_directory_entries(
        &self,
        directory: &Path,
    ) -> anyhow::Result<Vec<(PathBuf, bool)>> {
        self.directory_reader_service
            .list_directory_entries(directory)
            .await
    }

    async fn read_directory_files(
        &self,
        directory: &Path,
        pattern: Option<&str>,
    ) -> anyhow::Result<Vec<(PathBuf, String)>> {
        self.directory_reader_service
            .read_directory_files(directory, pattern)
            .await
    }
}

impl StrategyFactory for FurcodeInfra {
    type Strategy = AnyAuthStrategy;
    fn create_auth_strategy(
        &self,
        provider_id: ProviderId,
        method: AuthMethod,
        required_params: Vec<URLParamSpec>,
    ) -> anyhow::Result<Self::Strategy> {
        self.strategy_factory
            .create_auth_strategy(provider_id, method, required_params)
    }
}

impl GrpcInfra for FurcodeInfra {
    fn channel(&self) -> anyhow::Result<tonic::transport::Channel> {
        self.grpc_client.channel()
    }

    fn hydrate(&self) {
        self.grpc_client.hydrate();
    }
}

impl furcode_domain::ConsoleWriter for FurcodeInfra {
    fn write(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.output_printer.write(buf)
    }

    fn write_err(&self, buf: &[u8]) -> std::io::Result<usize> {
        self.output_printer.write_err(buf)
    }

    fn flush(&self) -> std::io::Result<()> {
        self.output_printer.flush()
    }

    fn flush_err(&self) -> std::io::Result<()> {
        self.output_printer.flush_err()
    }
}
