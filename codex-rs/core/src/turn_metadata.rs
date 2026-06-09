use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_utils_string::to_ascii_json_string;
use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::responses_metadata::CodexResponsesMetadata;
use crate::responses_metadata::CodexResponsesRequestKind;
use crate::responses_metadata::REQUEST_KIND_KEY;
use crate::responses_metadata::TURN_STARTED_AT_UNIX_MS_KEY;
use crate::responses_metadata::TurnMetadataWorkspace;
use crate::responses_metadata::insert_extra_metadata;
use crate::sandbox_tags::permission_profile_sandbox_tag;
use codex_git_utils::get_git_remote_urls_assume_git_repo;
use codex_git_utils::get_git_repo_root;
use codex_git_utils::get_has_changes;
use codex_git_utils::get_head_commit_hash;
use codex_protocol::ThreadId;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSource;
use codex_utils_absolute_path::AbsolutePathBuf;

const MODEL_KEY: &str = "model";
const REASONING_EFFORT_KEY: &str = "reasoning_effort";
const USER_INPUT_REQUESTED_DURING_TURN_KEY: &str = "user_input_requested_during_turn";
const WORKSPACE_KIND_KEY: &str = "workspace_kind";

pub(crate) struct McpTurnMetadataContext<'a> {
    pub(crate) model: &'a str,
    pub(crate) reasoning_effort: Option<ReasoningEffortConfig>,
}

#[derive(Clone, Debug, Default)]
struct WorkspaceGitMetadata {
    associated_remote_urls: Option<BTreeMap<String, String>>,
    latest_git_commit_hash: Option<String>,
    has_changes: Option<bool>,
}

impl WorkspaceGitMetadata {
    fn is_empty(&self) -> bool {
        self.associated_remote_urls.is_none()
            && self.latest_git_commit_hash.is_none()
            && self.has_changes.is_none()
    }
}

impl From<WorkspaceGitMetadata> for TurnMetadataWorkspace {
    fn from(value: WorkspaceGitMetadata) -> Self {
        Self {
            associated_remote_urls: value.associated_remote_urls,
            latest_git_commit_hash: value.latest_git_commit_hash,
            has_changes: value.has_changes,
        }
    }
}

/// Turn-owned fields that feed `CodexResponsesMetadata`.
///
/// Request-scoped fields such as installation id, window id, request kind, and compaction details
/// are added by `CodexResponsesMetadata` at outbound model dispatch. Detached memory requests are
/// still constructed as standalone `memory` blobs because they have no logical Codex turn.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct TurnMetadataBag {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forked_from_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_thread_id: Option<ThreadId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subagent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_source: Option<ThreadSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    workspaces: BTreeMap<String, TurnMetadataWorkspace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox: Option<String>,
}

impl TurnMetadataBag {
    fn with_workspace_git_metadata(
        mut self,
        repo_root: Option<String>,
        workspace_git_metadata: Option<WorkspaceGitMetadata>,
    ) -> Self {
        if let (Some(repo_root), Some(workspace_git_metadata)) = (repo_root, workspace_git_metadata)
            && !workspace_git_metadata.is_empty()
        {
            self.workspaces
                .insert(repo_root, workspace_git_metadata.into());
        }
        self
    }

    fn to_header_value(&self) -> Option<String> {
        to_ascii_json_string(self).ok()
    }
}

pub async fn build_turn_metadata_header(
    cwd: &AbsolutePathBuf,
    sandbox: Option<&str>,
) -> Option<String> {
    let repo_root = get_git_repo_root(cwd).map(|root| root.to_string_lossy().into_owned());

    let (head_commit_hash, associated_remote_urls, has_changes) = tokio::join!(
        get_head_commit_hash(cwd),
        get_git_remote_urls_assume_git_repo(cwd),
        get_has_changes(cwd),
    );
    let latest_git_commit_hash = head_commit_hash.map(|sha| sha.0);
    TurnMetadataBag {
        request_kind: Some("memory".to_string()),
        session_id: None,
        thread_id: None,
        forked_from_thread_id: None,
        parent_thread_id: None,
        subagent_kind: None,
        thread_source: None,
        turn_id: None,
        workspaces: BTreeMap::new(),
        sandbox: sandbox.map(ToString::to_string),
    }
    .with_workspace_git_metadata(
        repo_root,
        Some(WorkspaceGitMetadata {
            associated_remote_urls,
            latest_git_commit_hash,
            has_changes,
        }),
    )
    .to_header_value()
}

#[derive(Clone, Debug)]
pub(crate) struct TurnMetadataState {
    cwd: AbsolutePathBuf,
    repo_root: Option<String>,
    base_metadata: TurnMetadataBag,
    enriched_workspaces: Arc<RwLock<Option<BTreeMap<String, TurnMetadataWorkspace>>>>,
    turn_started_at_unix_ms: Arc<RwLock<Option<i64>>>,
    responsesapi_client_metadata: Arc<RwLock<Option<HashMap<String, String>>>>,
    user_input_requested_during_turn: Arc<AtomicBool>,
    enrichment_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl TurnMetadataState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: String,
        thread_id: String,
        forked_from_thread_id: Option<ThreadId>,
        parent_thread_id: Option<ThreadId>,
        session_source: &SessionSource,
        thread_source: Option<ThreadSource>,
        turn_id: String,
        cwd: AbsolutePathBuf,
        permission_profile: &PermissionProfile,
        windows_sandbox_level: WindowsSandboxLevel,
        enforce_managed_network: bool,
    ) -> Self {
        let repo_root = get_git_repo_root(&cwd).map(|root| root.to_string_lossy().into_owned());
        let sandbox = Some(
            permission_profile_sandbox_tag(
                permission_profile,
                windows_sandbox_level,
                enforce_managed_network,
            )
            .to_string(),
        );
        let subagent_kind = match session_source {
            SessionSource::SubAgent(subagent_source) => Some(subagent_source.kind().to_string()),
            SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Exec
            | SessionSource::Mcp
            | SessionSource::Custom(_)
            | SessionSource::Internal(_)
            | SessionSource::Unknown => None,
        };
        let base_metadata = TurnMetadataBag {
            request_kind: None,
            session_id: Some(session_id),
            thread_id: Some(thread_id),
            forked_from_thread_id,
            parent_thread_id,
            subagent_kind,
            thread_source,
            turn_id: Some(turn_id),
            workspaces: BTreeMap::new(),
            sandbox,
        };
        Self {
            cwd,
            repo_root,
            base_metadata,
            enriched_workspaces: Arc::new(RwLock::new(None)),
            turn_started_at_unix_ms: Arc::new(RwLock::new(None)),
            responsesapi_client_metadata: Arc::new(RwLock::new(None)),
            user_input_requested_during_turn: Arc::new(AtomicBool::new(false)),
            enrichment_task: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn current_header_value(&self) -> Option<String> {
        let mut metadata = serde_json::to_value(self.current_metadata_bag())
            .ok()?
            .as_object()
            .cloned()?;
        if let Some(turn_started_at_unix_ms) = *self
            .turn_started_at_unix_ms
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            metadata.insert(
                TURN_STARTED_AT_UNIX_MS_KEY.to_string(),
                Value::Number(turn_started_at_unix_ms.into()),
            );
        }
        insert_extra_metadata(&mut metadata, &self.current_extra_metadata());
        to_ascii_json_string(&metadata).ok()
    }

    pub(crate) fn current_meta_value_for_mcp_request(
        &self,
        context: McpTurnMetadataContext<'_>,
    ) -> Option<serde_json::Value> {
        let header = self.current_header_value()?;
        let mut metadata = serde_json::from_str::<serde_json::Map<String, Value>>(&header).ok()?;
        metadata.remove(REQUEST_KIND_KEY);
        metadata.insert(
            MODEL_KEY.to_string(),
            Value::String(context.model.to_string()),
        );
        match context.reasoning_effort {
            Some(reasoning_effort) => {
                metadata.insert(
                    REASONING_EFFORT_KEY.to_string(),
                    Value::String(reasoning_effort.to_string()),
                );
            }
            None => {
                metadata.remove(REASONING_EFFORT_KEY);
            }
        }
        if self
            .user_input_requested_during_turn
            .load(Ordering::Relaxed)
        {
            metadata.insert(
                USER_INPUT_REQUESTED_DURING_TURN_KEY.to_string(),
                Value::Bool(true),
            );
        } else {
            metadata.remove(USER_INPUT_REQUESTED_DURING_TURN_KEY);
        }
        Some(Value::Object(metadata))
    }

    pub(crate) fn current_responses_metadata(
        &self,
        installation_id: String,
        window_id: String,
        request_kind: CodexResponsesRequestKind,
    ) -> CodexResponsesMetadata {
        let bag = self.current_metadata_bag();
        CodexResponsesMetadata {
            installation_id,
            session_id: bag
                .session_id
                .expect("TurnMetadataState always has a session_id"),
            thread_id: bag
                .thread_id
                .expect("TurnMetadataState always has a thread_id"),
            turn_id: bag.turn_id,
            window_id,
            request_kind: Some(request_kind),
            forked_from_thread_id: bag.forked_from_thread_id,
            parent_thread_id: bag.parent_thread_id,
            subagent_kind: bag.subagent_kind,
            thread_source: bag.thread_source,
            sandbox: bag.sandbox,
            workspaces: bag.workspaces,
            turn_started_at_unix_ms: *self
                .turn_started_at_unix_ms
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            extra: self.current_extra_metadata(),
        }
    }

    pub(crate) fn mark_user_input_requested_during_turn(&self) {
        self.user_input_requested_during_turn
            .store(true, Ordering::Relaxed);
    }

    pub(crate) fn set_responsesapi_client_metadata(
        &self,
        responsesapi_client_metadata: HashMap<String, String>,
    ) {
        *self
            .responsesapi_client_metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(responsesapi_client_metadata);
    }

    pub(crate) fn workspace_kind(&self) -> Option<String> {
        self.responsesapi_client_metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|metadata| metadata.get(WORKSPACE_KIND_KEY).cloned())
    }

    fn current_metadata_bag(&self) -> TurnMetadataBag {
        let mut metadata = self.base_metadata.clone();
        if let Some(workspaces) = self
            .enriched_workspaces
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()
        {
            metadata.workspaces = workspaces;
        }
        metadata
    }

    fn current_extra_metadata(&self) -> BTreeMap<String, String> {
        let metadata = self
            .responsesapi_client_metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        metadata
    }

    pub(crate) fn set_turn_started_at_unix_ms(&self, turn_started_at_unix_ms: i64) {
        *self
            .turn_started_at_unix_ms
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn_started_at_unix_ms);
    }

    pub(crate) fn spawn_git_enrichment_task(&self) {
        if self.repo_root.is_none() {
            return;
        }

        let mut task_guard = self
            .enrichment_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if task_guard.is_some() {
            return;
        }

        let state = self.clone();
        *task_guard = Some(tokio::spawn(async move {
            let workspace_git_metadata = state.fetch_workspace_git_metadata().await;
            let Some(repo_root) = state.repo_root.clone() else {
                return;
            };

            if workspace_git_metadata.is_empty() {
                return;
            }

            let mut workspaces = BTreeMap::new();
            workspaces.insert(repo_root, workspace_git_metadata.into());
            *state
                .enriched_workspaces
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(workspaces);
        }));
    }

    pub(crate) fn cancel_git_enrichment_task(&self) {
        let mut task_guard = self
            .enrichment_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(task) = task_guard.take() {
            task.abort();
        }
    }

    async fn fetch_workspace_git_metadata(&self) -> WorkspaceGitMetadata {
        let (head_commit_hash, associated_remote_urls, has_changes) = tokio::join!(
            get_head_commit_hash(&self.cwd),
            get_git_remote_urls_assume_git_repo(&self.cwd),
            get_has_changes(&self.cwd),
        );
        let latest_git_commit_hash = head_commit_hash.map(|sha| sha.0);

        WorkspaceGitMetadata {
            associated_remote_urls,
            latest_git_commit_hash,
            has_changes,
        }
    }
}

#[cfg(test)]
#[path = "turn_metadata_tests.rs"]
mod tests;
