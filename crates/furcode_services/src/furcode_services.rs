use std::sync::Arc;

use furcode_app::{
    AgentRepository, CommandInfra, DirectoryReaderInfra, EnvironmentInfra, FileDirectoryInfra,
    FileInfoInfra, FileReaderInfra, FileRemoverInfra, FileWriterInfra, HttpInfra, KVStore,
    McpServerInfra, Services, StrategyFactory, UserInfra, WalkerInfra,
};
use furcode_domain::{
    ChatRepository, ConversationRepository, FuzzySearchRepository, ProviderRepository,
    SkillRepository, SnapshotRepository, TextPatchRepository, ValidationRepository,
    WorkspaceIndexRepository,
};

use crate::FurcodeProviderAuthService;
use crate::agent_registry::FurcodeAgentRegistryService;
use crate::app_config::FurcodeAppConfigService;
use crate::attachment::FurcodeChatRequest;
use crate::auth::FurcodeAuthService;
use crate::command::CommandLoaderService as FurcodeCommandLoaderService;
use crate::conversation::FurcodeConversationService;
use crate::discovery::FurcodeDiscoveryService;
use crate::fd::FdDefault;
use crate::instructions::FurcodeCustomInstructionsService;
use crate::mcp::{FurcodeMcpManager, FurcodeMcpService};
use crate::policy::FurcodePolicyService;
use crate::provider_service::FurcodeProviderService;
use crate::template::FurcodeTemplateService;
use crate::tool_services::{
    FurcodeFetch, FurcodeFollowup, FurcodeFsPatch, FurcodeFsRead, FurcodeFsRemove, FurcodeFsSearch,
    FurcodeFsUndo, FurcodeFsWrite, FurcodeImageRead, FurcodePlanCreate, FurcodeRemember,
    FurcodeShell, FurcodeSkillFetch,
};

type McpService<F> = FurcodeMcpService<FurcodeMcpManager<F>, F, <F as McpServerInfra>::Client>;
type AuthService<F> = FurcodeAuthService<F>;

/// FurcodeApp is the main application container that implements the App trait.
/// It provides access to all core services required by the application.
///
/// Type Parameters:
/// - F: The infrastructure implementation that provides core services like
///   environment, file reading, vector indexing, and embedding.
/// - R: The repository implementation that provides data persistence
#[derive(Clone)]
pub struct FurcodeServices<
    F: HttpInfra
        + EnvironmentInfra
        + McpServerInfra
        + WalkerInfra
        + SnapshotRepository
        + ConversationRepository
        + KVStore
        + ChatRepository
        + ProviderRepository
        + WorkspaceIndexRepository
        + AgentRepository
        + SkillRepository
        + ValidationRepository,
> {
    chat_service: Arc<FurcodeProviderService<F>>,
    config_service: Arc<FurcodeAppConfigService<F>>,
    conversation_service: Arc<FurcodeConversationService<F>>,
    template_service: Arc<FurcodeTemplateService<F>>,
    attachment_service: Arc<FurcodeChatRequest<F>>,
    discovery_service: Arc<FurcodeDiscoveryService<F>>,
    mcp_manager: Arc<FurcodeMcpManager<F>>,
    file_create_service: Arc<FurcodeFsWrite<F>>,
    plan_create_service: Arc<FurcodePlanCreate<F>>,
    remember_service: Arc<FurcodeRemember<F>>,
    file_read_service: Arc<FurcodeFsRead<F>>,
    image_read_service: Arc<FurcodeImageRead<F>>,
    file_search_service: Arc<FurcodeFsSearch<F>>,
    file_remove_service: Arc<FurcodeFsRemove<F>>,
    file_patch_service: Arc<FurcodeFsPatch<F>>,
    file_undo_service: Arc<FurcodeFsUndo<F>>,
    shell_service: Arc<FurcodeShell<F>>,
    fetch_service: Arc<FurcodeFetch>,
    followup_service: Arc<FurcodeFollowup<F>>,
    mcp_service: Arc<McpService<F>>,
    custom_instructions_service: Arc<FurcodeCustomInstructionsService<F>>,
    auth_service: Arc<AuthService<F>>,
    agent_registry_service: Arc<FurcodeAgentRegistryService<F>>,
    command_loader_service: Arc<FurcodeCommandLoaderService<F>>,
    policy_service: FurcodePolicyService<F>,
    provider_auth_service: FurcodeProviderAuthService<F>,
    workspace_service: Arc<crate::context_engine::FurcodeWorkspaceService<F, FdDefault<F>>>,
    skill_service: Arc<FurcodeSkillFetch<F>>,
    infra: Arc<F>,
}

impl<
    F: McpServerInfra
        + EnvironmentInfra<Config = furcode_config::FurcodeConfig>
        + FileWriterInfra
        + FileInfoInfra
        + FileReaderInfra
        + HttpInfra
        + WalkerInfra
        + DirectoryReaderInfra
        + CommandInfra
        + UserInfra
        + SnapshotRepository
        + ConversationRepository
        + ChatRepository
        + ProviderRepository
        + KVStore
        + WorkspaceIndexRepository
        + AgentRepository
        + SkillRepository
        + ValidationRepository,
> FurcodeServices<F>
{
    pub fn new(infra: Arc<F>) -> Self {
        let mcp_manager = Arc::new(FurcodeMcpManager::new(infra.clone()));
        let mcp_service = Arc::new(FurcodeMcpService::new(mcp_manager.clone(), infra.clone()));
        let template_service = Arc::new(FurcodeTemplateService::new(infra.clone()));
        let attachment_service = Arc::new(FurcodeChatRequest::new(infra.clone()));
        let suggestion_service = Arc::new(FurcodeDiscoveryService::new(infra.clone()));
        let conversation_service = Arc::new(FurcodeConversationService::new(infra.clone()));
        let auth_service = Arc::new(FurcodeAuthService::new(infra.clone()));
        let chat_service = Arc::new(FurcodeProviderService::new(infra.clone()));
        let config_service = Arc::new(FurcodeAppConfigService::new(infra.clone()));
        let file_create_service = Arc::new(FurcodeFsWrite::new(infra.clone()));
        let plan_create_service = Arc::new(FurcodePlanCreate::new(infra.clone()));
        let remember_service = Arc::new(FurcodeRemember::new(infra.clone()));
        let file_read_service = Arc::new(FurcodeFsRead::new(infra.clone()));
        let image_read_service = Arc::new(FurcodeImageRead::new(infra.clone()));
        let file_search_service = Arc::new(FurcodeFsSearch::new(infra.clone()));
        let file_remove_service = Arc::new(FurcodeFsRemove::new(infra.clone()));
        let file_patch_service = Arc::new(FurcodeFsPatch::new(infra.clone()));
        let file_undo_service = Arc::new(FurcodeFsUndo::new(infra.clone()));
        let shell_service = Arc::new(FurcodeShell::new(infra.clone()));
        let fetch_service = Arc::new(FurcodeFetch::new());
        let followup_service = Arc::new(FurcodeFollowup::new(infra.clone()));
        let custom_instructions_service =
            Arc::new(FurcodeCustomInstructionsService::new(infra.clone()));
        let agent_registry_service = Arc::new(FurcodeAgentRegistryService::new(infra.clone()));
        let command_loader_service = Arc::new(FurcodeCommandLoaderService::new(infra.clone()));
        let policy_service = FurcodePolicyService::new(infra.clone());
        let provider_auth_service = FurcodeProviderAuthService::new(infra.clone());
        let discovery = Arc::new(FdDefault::new(infra.clone()));
        let workspace_service = Arc::new(crate::context_engine::FurcodeWorkspaceService::new(
            infra.clone(),
            discovery,
        ));
        let skill_service = Arc::new(FurcodeSkillFetch::new(infra.clone()));

        Self {
            conversation_service,
            attachment_service,
            template_service,
            discovery_service: suggestion_service,
            mcp_manager,
            file_create_service,
            plan_create_service,
            remember_service,
            file_read_service,
            image_read_service,
            file_search_service,
            file_remove_service,
            file_patch_service,
            file_undo_service,
            shell_service,
            fetch_service,
            followup_service,
            mcp_service,
            custom_instructions_service,
            auth_service,
            config_service,
            agent_registry_service,
            command_loader_service,
            policy_service,
            provider_auth_service,
            workspace_service,
            skill_service,
            chat_service,
            infra,
        }
    }
}

impl<
    F: FileReaderInfra
        + FileWriterInfra
        + CommandInfra
        + UserInfra
        + McpServerInfra
        + FileRemoverInfra
        + FileInfoInfra
        + FileDirectoryInfra
        + EnvironmentInfra<Config = furcode_config::FurcodeConfig>
        + DirectoryReaderInfra
        + HttpInfra
        + WalkerInfra
        + Clone
        + SnapshotRepository
        + ConversationRepository
        + KVStore
        + ChatRepository
        + ProviderRepository
        + AgentRepository
        + SkillRepository
        + StrategyFactory
        + WorkspaceIndexRepository
        + ValidationRepository
        + FuzzySearchRepository
        + TextPatchRepository
        + Clone
        + 'static,
> Services for FurcodeServices<F>
{
    type AppConfigService = FurcodeAppConfigService<F>;
    type ConversationService = FurcodeConversationService<F>;
    type TemplateService = FurcodeTemplateService<F>;
    type ProviderAuthService = FurcodeProviderAuthService<F>;

    fn provider_auth_service(&self) -> &Self::ProviderAuthService {
        &self.provider_auth_service
    }
    type AttachmentService = FurcodeChatRequest<F>;
    type CustomInstructionsService = FurcodeCustomInstructionsService<F>;
    type FileDiscoveryService = FurcodeDiscoveryService<F>;
    type McpConfigManager = FurcodeMcpManager<F>;
    type FsWriteService = FurcodeFsWrite<F>;
    type PlanCreateService = FurcodePlanCreate<F>;
    type RememberService = FurcodeRemember<F>;
    type FsPatchService = FurcodeFsPatch<F>;
    type FsReadService = FurcodeFsRead<F>;
    type ImageReadService = FurcodeImageRead<F>;
    type FsRemoveService = FurcodeFsRemove<F>;
    type FsSearchService = FurcodeFsSearch<F>;
    type FollowUpService = FurcodeFollowup<F>;
    type FsUndoService = FurcodeFsUndo<F>;
    type NetFetchService = FurcodeFetch;
    type ShellService = FurcodeShell<F>;
    type McpService = McpService<F>;
    type AuthService = AuthService<F>;
    type AgentRegistry = FurcodeAgentRegistryService<F>;
    type CommandLoaderService = FurcodeCommandLoaderService<F>;
    type PolicyService = FurcodePolicyService<F>;
    type ProviderService = FurcodeProviderService<F>;
    type WorkspaceService = crate::context_engine::FurcodeWorkspaceService<F, FdDefault<F>>;
    type SkillFetchService = FurcodeSkillFetch<F>;

    fn config_service(&self) -> &Self::AppConfigService {
        &self.config_service
    }

    fn conversation_service(&self) -> &Self::ConversationService {
        &self.conversation_service
    }

    fn template_service(&self) -> &Self::TemplateService {
        &self.template_service
    }

    fn attachment_service(&self) -> &Self::AttachmentService {
        &self.attachment_service
    }

    fn custom_instructions_service(&self) -> &Self::CustomInstructionsService {
        &self.custom_instructions_service
    }

    fn file_discovery_service(&self) -> &Self::FileDiscoveryService {
        self.discovery_service.as_ref()
    }

    fn mcp_config_manager(&self) -> &Self::McpConfigManager {
        self.mcp_manager.as_ref()
    }

    fn fs_create_service(&self) -> &Self::FsWriteService {
        &self.file_create_service
    }

    fn plan_create_service(&self) -> &Self::PlanCreateService {
        &self.plan_create_service
    }

    fn remember_service(&self) -> &Self::RememberService {
        &self.remember_service
    }

    fn fs_patch_service(&self) -> &Self::FsPatchService {
        &self.file_patch_service
    }

    fn fs_read_service(&self) -> &Self::FsReadService {
        &self.file_read_service
    }

    fn fs_remove_service(&self) -> &Self::FsRemoveService {
        &self.file_remove_service
    }

    fn fs_search_service(&self) -> &Self::FsSearchService {
        &self.file_search_service
    }

    fn follow_up_service(&self) -> &Self::FollowUpService {
        &self.followup_service
    }

    fn fs_undo_service(&self) -> &Self::FsUndoService {
        &self.file_undo_service
    }

    fn net_fetch_service(&self) -> &Self::NetFetchService {
        &self.fetch_service
    }

    fn shell_service(&self) -> &Self::ShellService {
        &self.shell_service
    }

    fn mcp_service(&self) -> &Self::McpService {
        &self.mcp_service
    }

    fn auth_service(&self) -> &Self::AuthService {
        self.auth_service.as_ref()
    }

    fn agent_registry(&self) -> &Self::AgentRegistry {
        &self.agent_registry_service
    }

    fn command_loader_service(&self) -> &Self::CommandLoaderService {
        &self.command_loader_service
    }

    fn policy_service(&self) -> &Self::PolicyService {
        &self.policy_service
    }

    fn workspace_service(&self) -> &Self::WorkspaceService {
        &self.workspace_service
    }

    fn image_read_service(&self) -> &Self::ImageReadService {
        &self.image_read_service
    }
    fn skill_fetch_service(&self) -> &Self::SkillFetchService {
        &self.skill_service
    }

    fn provider_service(&self) -> &Self::ProviderService {
        &self.chat_service
    }
}

impl<
    F: EnvironmentInfra<Config = furcode_config::FurcodeConfig>
        + HttpInfra
        + McpServerInfra
        + WalkerInfra
        + SnapshotRepository
        + ConversationRepository
        + KVStore
        + ChatRepository
        + ProviderRepository
        + WorkspaceIndexRepository
        + AgentRepository
        + SkillRepository
        + ValidationRepository
        + Send
        + Sync,
> furcode_app::EnvironmentInfra for FurcodeServices<F>
{
    type Config = furcode_config::FurcodeConfig;

    fn get_environment(&self) -> furcode_domain::Environment {
        self.infra.get_environment()
    }

    fn get_config(&self) -> anyhow::Result<furcode_config::FurcodeConfig> {
        self.infra.get_config()
    }

    fn update_environment(
        &self,
        ops: Vec<furcode_domain::ConfigOperation>,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        self.infra.update_environment(ops)
    }

    fn get_env_var(&self, key: &str) -> Option<String> {
        self.infra.get_env_var(key)
    }

    fn get_env_vars(&self) -> std::collections::BTreeMap<String, String> {
        self.infra.get_env_vars()
    }
}
