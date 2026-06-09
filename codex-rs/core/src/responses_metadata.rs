use std::collections::BTreeMap;
use std::collections::HashMap;

use codex_analytics::CompactionImplementation;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionStrategy;
use codex_analytics::CompactionTrigger;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadSource;
use codex_utils_string::to_ascii_json_string;
use http::HeaderMap as ApiHeaderMap;
use http::HeaderValue;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

use crate::client::X_CODEX_INSTALLATION_ID_HEADER;
use crate::client::X_CODEX_PARENT_THREAD_ID_HEADER;
use crate::client::X_CODEX_TURN_METADATA_HEADER;
use crate::client::X_CODEX_WINDOW_ID_HEADER;

pub(crate) const INSTALLATION_ID_KEY: &str = "installation_id";
pub(crate) const SESSION_ID_KEY: &str = "session_id";
pub(crate) const THREAD_ID_KEY: &str = "thread_id";
pub(crate) const TURN_ID_KEY: &str = "turn_id";
pub(crate) const WINDOW_ID_KEY: &str = "window_id";
pub(crate) const REQUEST_KIND_KEY: &str = "request_kind";
pub(crate) const COMPACTION_KEY: &str = "compaction";
pub(crate) const TURN_STARTED_AT_UNIX_MS_KEY: &str = "turn_started_at_unix_ms";

const FORKED_FROM_THREAD_ID_KEY: &str = "forked_from_thread_id";
const PARENT_THREAD_ID_KEY: &str = "parent_thread_id";
const SUBAGENT_KIND_KEY: &str = "subagent_kind";
const THREAD_SOURCE_KEY: &str = "thread_source";
const SANDBOX_KEY: &str = "sandbox";
const WORKSPACES_KEY: &str = "workspaces";

/// Metadata present only on outbound model requests that perform compaction.
///
/// These fields describe the operation at dispatch time. Post-response outcomes such as status,
/// error, duration, and token deltas remain in compaction analytics events.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct CompactionTurnMetadata {
    trigger: CompactionTrigger,
    reason: CompactionReason,
    implementation: CompactionImplementation,
    phase: CompactionPhase,
    strategy: CompactionStrategy,
}

impl CompactionTurnMetadata {
    pub(crate) fn new(
        trigger: CompactionTrigger,
        reason: CompactionReason,
        implementation: CompactionImplementation,
        phase: CompactionPhase,
    ) -> Self {
        Self {
            trigger,
            reason,
            implementation,
            phase,
            strategy: CompactionStrategy::Memento,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum CodexResponsesRequestKind {
    Connection,
    Turn,
    Prewarm,
    Compaction(CompactionTurnMetadata),
}

impl CodexResponsesRequestKind {
    fn request_kind_value(self) -> Option<&'static str> {
        match self {
            CodexResponsesRequestKind::Connection => None,
            CodexResponsesRequestKind::Turn => Some("turn"),
            CodexResponsesRequestKind::Prewarm => Some("prewarm"),
            CodexResponsesRequestKind::Compaction(_) => Some("compaction"),
        }
    }

    fn compaction(self) -> Option<CompactionTurnMetadata> {
        match self {
            CodexResponsesRequestKind::Compaction(metadata) => Some(metadata),
            CodexResponsesRequestKind::Connection
            | CodexResponsesRequestKind::Turn
            | CodexResponsesRequestKind::Prewarm => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub(crate) struct TurnMetadataWorkspace {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) associated_remote_urls: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) latest_git_commit_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) has_changes: Option<bool>,
}

pub(crate) struct CodexResponsesMetadataParams {
    pub(crate) installation_id: String,
    pub(crate) session_id: String,
    pub(crate) thread_id: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) window_id: String,
    pub(crate) request_kind: CodexResponsesRequestKind,
    pub(crate) forked_from_thread_id: Option<ThreadId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) subagent_kind: Option<String>,
    pub(crate) thread_source: Option<ThreadSource>,
    pub(crate) sandbox: Option<String>,
    pub(crate) workspaces: BTreeMap<String, TurnMetadataWorkspace>,
    pub(crate) turn_started_at_unix_ms: Option<i64>,
    pub(crate) extra: BTreeMap<String, String>,
}

/// Single source of truth for Codex metadata sent to ResponsesAPI.
///
/// The full Codex turn metadata blob is transported canonically as
/// `client_metadata["x-codex-turn-metadata"]`. Flat `client_metadata` keys and direct HTTP/ws
/// headers are generated compatibility projections of this snapshot, not separate sources of
/// truth.
#[derive(Clone, Debug)]
pub struct CodexResponsesMetadata {
    pub(crate) installation_id: String,
    pub(crate) session_id: String,
    pub(crate) thread_id: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) window_id: String,
    pub(crate) request_kind: CodexResponsesRequestKind,
    pub(crate) forked_from_thread_id: Option<ThreadId>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) subagent_kind: Option<String>,
    pub(crate) thread_source: Option<ThreadSource>,
    pub(crate) sandbox: Option<String>,
    pub(crate) workspaces: BTreeMap<String, TurnMetadataWorkspace>,
    pub(crate) turn_started_at_unix_ms: Option<i64>,
    pub(crate) extra: BTreeMap<String, String>,
}

impl CodexResponsesMetadata {
    pub(crate) fn new(params: CodexResponsesMetadataParams) -> Self {
        Self {
            installation_id: params.installation_id,
            session_id: params.session_id,
            thread_id: params.thread_id,
            turn_id: params.turn_id,
            window_id: params.window_id,
            request_kind: params.request_kind,
            forked_from_thread_id: params.forked_from_thread_id,
            parent_thread_id: params.parent_thread_id,
            subagent_kind: params.subagent_kind,
            thread_source: params.thread_source,
            sandbox: params.sandbox,
            workspaces: params.workspaces,
            turn_started_at_unix_ms: params.turn_started_at_unix_ms,
            // responsesapi_client_metadata is an app-server enrichment hook into the Codex turn
            // metadata blob. It is not literal top-level Responses client_metadata, and empty or
            // conflicting extras must never replace Codex-owned request identity or lineage.
            extra: filter_extra_metadata(params.extra),
        }
    }

    pub(crate) fn connection_only(
        installation_id: String,
        session_id: String,
        thread_id: String,
        window_id: String,
    ) -> Self {
        Self::new(CodexResponsesMetadataParams {
            installation_id,
            session_id,
            thread_id,
            turn_id: None,
            window_id,
            request_kind: CodexResponsesRequestKind::Connection,
            forked_from_thread_id: None,
            parent_thread_id: None,
            subagent_kind: None,
            thread_source: None,
            sandbox: None,
            workspaces: BTreeMap::new(),
            turn_started_at_unix_ms: None,
            extra: BTreeMap::new(),
        })
    }

    pub(crate) fn has_turn_metadata(&self) -> bool {
        self.request_kind.request_kind_value().is_some()
    }

    pub(crate) fn turn_metadata_json(&self) -> Option<String> {
        let request_kind = self.request_kind.request_kind_value()?;
        let mut metadata = Map::from_iter([
            (
                INSTALLATION_ID_KEY.to_string(),
                Value::String(self.installation_id.clone()),
            ),
            (
                SESSION_ID_KEY.to_string(),
                Value::String(self.session_id.clone()),
            ),
            (
                THREAD_ID_KEY.to_string(),
                Value::String(self.thread_id.clone()),
            ),
            (
                WINDOW_ID_KEY.to_string(),
                Value::String(self.window_id.clone()),
            ),
            (
                REQUEST_KIND_KEY.to_string(),
                Value::String(request_kind.to_string()),
            ),
        ]);
        insert_optional_string(&mut metadata, TURN_ID_KEY, self.turn_id.as_deref());
        insert_optional_value(
            &mut metadata,
            FORKED_FROM_THREAD_ID_KEY,
            self.forked_from_thread_id,
        );
        insert_optional_value(&mut metadata, PARENT_THREAD_ID_KEY, self.parent_thread_id);
        insert_optional_string(
            &mut metadata,
            SUBAGENT_KIND_KEY,
            self.subagent_kind.as_deref(),
        );
        insert_optional_value(
            &mut metadata,
            THREAD_SOURCE_KEY,
            self.thread_source.as_ref(),
        );
        insert_optional_string(&mut metadata, SANDBOX_KEY, self.sandbox.as_deref());
        if !self.workspaces.is_empty()
            && let Ok(workspaces) = serde_json::to_value(&self.workspaces)
        {
            metadata.insert(WORKSPACES_KEY.to_string(), workspaces);
        }
        if let Some(turn_started_at_unix_ms) = self.turn_started_at_unix_ms {
            metadata.insert(
                TURN_STARTED_AT_UNIX_MS_KEY.to_string(),
                Value::Number(turn_started_at_unix_ms.into()),
            );
        }
        insert_optional_value(
            &mut metadata,
            COMPACTION_KEY,
            self.request_kind.compaction(),
        );
        insert_extra_metadata(&mut metadata, &self.extra);
        to_ascii_json_string(&metadata).ok()
    }

    pub(crate) fn client_metadata(&self) -> HashMap<String, String> {
        let mut client_metadata = HashMap::from([
            (
                X_CODEX_INSTALLATION_ID_HEADER.to_string(),
                self.installation_id.clone(),
            ),
            (SESSION_ID_KEY.to_string(), self.session_id.clone()),
            (THREAD_ID_KEY.to_string(), self.thread_id.clone()),
            (X_CODEX_WINDOW_ID_HEADER.to_string(), self.window_id.clone()),
        ]);
        if let Some(turn_id) = &self.turn_id {
            client_metadata.insert(TURN_ID_KEY.to_string(), turn_id.clone());
        }
        if let Some(turn_metadata_json) = self.turn_metadata_json() {
            client_metadata.insert(X_CODEX_TURN_METADATA_HEADER.to_string(), turn_metadata_json);
        }
        client_metadata
    }

    pub(crate) fn compatibility_headers(&self) -> ApiHeaderMap {
        let mut headers = ApiHeaderMap::new();
        self.insert_compatibility_headers(&mut headers);
        headers
    }

    pub(crate) fn insert_compatibility_headers(&self, headers: &mut ApiHeaderMap) {
        if let Ok(header_value) = HeaderValue::from_str(&self.window_id) {
            headers.insert(X_CODEX_WINDOW_ID_HEADER, header_value);
        }
        // Direct x-codex-turn-metadata is compatibility output. New per-request consumers should
        // prefer client_metadata["x-codex-turn-metadata"], which is rendered from this same object.
        if let Some(turn_metadata_json) = self.turn_metadata_json()
            && let Ok(header_value) = HeaderValue::from_str(&turn_metadata_json)
        {
            headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value);
        }
        if let Some(parent_thread_id) = self.parent_thread_id
            && let Ok(header_value) = HeaderValue::from_str(&parent_thread_id.to_string())
        {
            headers.insert(X_CODEX_PARENT_THREAD_ID_HEADER, header_value);
        }
    }
}

pub(crate) fn filter_extra_metadata(extra: BTreeMap<String, String>) -> BTreeMap<String, String> {
    extra
        .into_iter()
        .filter(|(key, _)| !is_reserved_metadata_key(key))
        .collect()
}

pub(crate) fn insert_extra_metadata(
    metadata: &mut Map<String, Value>,
    extra: &BTreeMap<String, String>,
) {
    for (key, value) in extra {
        if !is_reserved_metadata_key(key) {
            metadata
                .entry(key.clone())
                .or_insert_with(|| Value::String(value.clone()));
        }
    }
}

fn is_reserved_metadata_key(key: &str) -> bool {
    matches!(
        key,
        INSTALLATION_ID_KEY
            | X_CODEX_INSTALLATION_ID_HEADER
            | SESSION_ID_KEY
            | THREAD_ID_KEY
            | TURN_ID_KEY
            | WINDOW_ID_KEY
            | X_CODEX_WINDOW_ID_HEADER
            | X_CODEX_TURN_METADATA_HEADER
            | REQUEST_KIND_KEY
            | COMPACTION_KEY
            | TURN_STARTED_AT_UNIX_MS_KEY
            | FORKED_FROM_THREAD_ID_KEY
            | PARENT_THREAD_ID_KEY
            | SUBAGENT_KIND_KEY
            | THREAD_SOURCE_KEY
            | SANDBOX_KEY
            | WORKSPACES_KEY
    )
}

fn insert_optional_string(metadata: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        metadata.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn insert_optional_value<T: Serialize>(
    metadata: &mut Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        metadata.insert(key.to_string(), value);
    }
}
