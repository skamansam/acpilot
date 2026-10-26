//! The wire contract, type by type.
//!
//! The OpenAPI snapshot proves the document is stable; this suite proves the
//! document and the serialiser agree. Every JSON document below is parsed into
//! the Rust type and serialised back, so a renamed field, a changed enum spelling
//! or an option that stopped being omitted fails here with the type's name rather
//! than in a client.
//!
//! Field names are written out literally on purpose: `serde(rename_all = ...)`
//! added later would silently move the wire surface, and the API docs promise it
//! does not.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn round_trip<T>(document: Value)
where
    T: Serialize + DeserializeOwned,
{
    let parsed: T = serde_json::from_value(document.clone()).unwrap_or_else(|error| {
        panic!(
            "{} could not be parsed from {document}: {error}",
            std::any::type_name::<T>()
        )
    });
    // Go through the text form, not `to_value`: `f32` widens to an exact `f64`
    // in a `Value`, which would fail on `0.2` even though the bytes on the wire
    // are correct.
    let text = serde_json::to_string(&parsed).expect("re-serialisation cannot fail");
    let reencoded: Value = serde_json::from_str(&text).expect("re-serialised JSON parses");
    assert_eq!(
        reencoded,
        document,
        "{} changed shape on the wire: {text}",
        std::any::type_name::<T>()
    );
}

macro_rules! assert_wire {
    ($variant:expr, $wire:literal) => {
        assert_eq!(
            serde_json::to_value($variant).expect("enum serialises"),
            json!($wire),
            "wire spelling changed"
        );
    };
}

#[test]
fn agent_documents() {
    round_trip::<acpilot_proto::agent::AgentSummary>(json!({
        "id": "claude",
        "name": "Claude Code",
        "transport": "acp_stdio",
        "status": "available",
        "command": "claude --acp",
        "protocol_version": "1.0"
    }));

    // `protocol_version` is absent until a handshake happened; it must be absent,
    // not null, so clients can tell "never asked" from "said nothing".
    round_trip::<acpilot_proto::agent::AgentSummary>(json!({
        "id": "codex",
        "name": "Codex",
        "transport": "pty",
        "status": "untrusted",
        "command": "codex"
    }));
}

#[test]
fn host_documents() {
    round_trip::<acpilot_proto::host::HostSummary>(json!({
        "id": "local",
        "label": "This machine",
        "kind": "local",
        "connected": true,
        "capabilities": {
            "filesystem": true,
            "pty": true,
            "spawn": true,
            "lsp": true
        }
    }));

    // Before the first connection there are no negotiated capabilities.
    round_trip::<acpilot_proto::host::HostSummary>(json!({
        "id": "box",
        "label": "dev@box",
        "kind": "ssh",
        "connected": false
    }));

    round_trip::<acpilot_proto::host::HostCapabilities>(json!({
        "filesystem": true,
        "pty": false,
        "spawn": false,
        "lsp": false
    }));
}

#[test]
fn workspace_documents() {
    round_trip::<acpilot_proto::workspace::WorkspaceSummary>(json!({
        "id": "ws-1",
        "name": "acpilot",
        "host_id": "local",
        "root": "/home/sam/workspace-other/acpilot",
        "session_count": 3
    }));

    round_trip::<acpilot_proto::workspace::WorkspaceOpenRequest>(json!({
        "host_id": "local",
        "root": "/home/sam/workspace-other/acpilot"
    }));

    round_trip::<acpilot_proto::workspace::WorkspaceOpenRequest>(json!({
        "host_id": "local",
        "root": "/home/sam/workspace-other/acpilot",
        "name": "Acpilot"
    }));
}

#[test]
fn session_documents() {
    round_trip::<acpilot_proto::session::SessionSummary>(json!({
        "id": "s-1",
        "workspace_id": "ws-1",
        "agent_id": "claude",
        "title": "Fix the framing bug",
        "status": "awaiting_permission",
        "created_at": "2026-10-21T09:30:00Z",
        "updated_at": "2026-10-21T09:31:12Z"
    }));

    round_trip::<acpilot_proto::session::CreateSessionRequest>(json!({
        "workspace_id": "ws-1",
        "agent_id": "claude"
    }));

    round_trip::<acpilot_proto::session::CreateSessionRequest>(json!({
        "workspace_id": "ws-1",
        "agent_id": "claude",
        "title": "Fix the framing bug"
    }));

    round_trip::<acpilot_proto::session::PromptRequest>(json!({
        "text": "run the tests"
    }));

    // A retry after a dropped connection carries the same key.
    round_trip::<acpilot_proto::session::PromptRequest>(json!({
        "text": "run the tests",
        "idempotency_key": "9f2c"
    }));

    round_trip::<acpilot_proto::session::PromptAccepted>(json!({
        "session_id": "s-1",
        "turn_id": "t-7",
        "first_seq": 42
    }));
}

#[test]
fn event_documents() {
    round_trip::<acpilot_proto::event::SessionEvent>(json!({
        "seq": 42,
        "session_id": "s-1",
        "kind": "tool_call",
        "at": "2026-10-21T09:31:12Z",
        "payload": {
            "tool": "read_file",
            "status": "started"
        }
    }));

    // The payload is adapter-specific and therefore open: an object the daemon
    // has never seen still parses, and is preserved unchanged.
    round_trip::<acpilot_proto::event::SessionEvent>(json!({
        "seq": 43,
        "session_id": "s-1",
        "kind": "notice",
        "at": "2026-10-21T09:31:13Z",
        "payload": ["a", 1, true, null]
    }));

    assert_wire!(
        acpilot_proto::event::SessionEventKind::MessageChunk,
        "message_chunk"
    );
    assert_wire!(acpilot_proto::event::SessionEventKind::Thought, "thought");
    assert_wire!(
        acpilot_proto::event::SessionEventKind::ToolCall,
        "tool_call"
    );
    assert_wire!(acpilot_proto::event::SessionEventKind::Plan, "plan");
    assert_wire!(
        acpilot_proto::event::SessionEventKind::ModeChanged,
        "mode_changed"
    );
    assert_wire!(acpilot_proto::event::SessionEventKind::Notice, "notice");
    assert_wire!(
        acpilot_proto::event::SessionEventKind::Compaction,
        "compaction"
    );
    assert_wire!(
        acpilot_proto::event::SessionEventKind::CommandsChanged,
        "commands_changed"
    );
}

#[test]
fn permission_documents() {
    round_trip::<acpilot_proto::permission::PermissionRequest>(json!({
        "id": "p-1",
        "session_id": "s-1",
        "kind": "file_write",
        "title": "Write src/lib.rs",
        "detail": "+1 -1",
        "options": ["allow_once", "allow_always", "deny"],
        "created_at": "2026-10-21T09:31:12Z"
    }));

    round_trip::<acpilot_proto::permission::PermissionDecision>(json!({
        "option": "allow_once",
        "decided_by": "policy:read-only",
        "at": "2026-10-21T09:31:13Z"
    }));
}

#[test]
fn file_documents() {
    round_trip::<acpilot_proto::file::FileEntry>(json!({
        "path": "src/lib.rs",
        "is_dir": false,
        "size": 42,
        "modified": "2026-10-21T09:31:12Z"
    }));

    round_trip::<acpilot_proto::file::FileEntry>(json!({
        "path": "src",
        "is_dir": true,
        "size": 0
    }));

    round_trip::<acpilot_proto::file::FileContent>(json!({
        "path": "src/lib.rs",
        "text": "fn main() {}\n",
        "etag": "\"mtime-1-size-12\""
    }));

    round_trip::<acpilot_proto::file::FileWriteRequest>(json!({
        "path": "new.txt",
        "text": "hello"
    }));

    round_trip::<acpilot_proto::file::FileWriteRequest>(json!({
        "path": "src/lib.rs",
        "text": "fn main() {}\n",
        "if_match": "\"mtime-1-size-12\""
    }));
}

#[test]
fn terminal_documents() {
    round_trip::<acpilot_proto::terminal::TerminalSummary>(json!({
        "id": "term-1",
        "session_id": "s-1",
        "size": { "cols": 120, "rows": 40 },
        "alive": true,
        "command": "cargo test"
    }));

    round_trip::<acpilot_proto::terminal::TerminalSize>(json!({
        "cols": 80,
        "rows": 24
    }));
}

#[test]
fn profile_documents() {
    round_trip::<acpilot_proto::profile::ProfileSummary>(json!({
        "id": "work",
        "label": "Work",
        "active": true,
        "agent_count": 2,
        "workspace_count": 1
    }));

    round_trip::<acpilot_proto::profile::Preferences>(json!({
        "schema_version": 1,
        "tab_width": 4,
        "insert_spaces": true,
        "format_on_save": false,
        "trim_trailing_whitespace": true,
        "wrap": false,
        "theme": "acpilot.night-flight",
        "theme_follow_terminal": false,
        "diagnostics_min_severity": "warning"
    }));
}

#[test]
fn lsp_documents() {
    round_trip::<acpilot_proto::lsp::LspServerSummary>(json!({
        "id": "rust-analyzer",
        "languages": ["rust"],
        "command": "rust-analyzer",
        "status": "ready",
        "encoding": "utf8"
    }));

    // Before `initialize` no encoding has been agreed.
    round_trip::<acpilot_proto::lsp::LspServerSummary>(json!({
        "id": "pyright",
        "languages": ["python"],
        "command": "pyright-langserver --stdio",
        "status": "starting"
    }));
}

#[test]
fn model_documents() {
    round_trip::<acpilot_proto::model::ProviderSummary>(json!({
        "id": "lmstudio",
        "kind": "lm_studio",
        "label": "LM Studio",
        "base_url": "http://127.0.0.1:1234",
        "status": "reachable",
        "model_count": 2,
        "credential_ref": "keyring:lmstudio",
        "last_probed_at": "2026-10-21T09:31:12Z",
        "detail": "1 runtime, 2 models"
    }));

    round_trip::<acpilot_proto::model::ProviderSummary>(json!({
        "id": "ollama",
        "kind": "open_ai_compatible",
        "label": "Ollama",
        "base_url": "http://127.0.0.1:11434",
        "status": "disabled",
        "model_count": 0
    }));

    round_trip::<acpilot_proto::model::AddProviderRequest>(json!({
        "id": "ollama",
        "kind": "open_ai_compatible",
        "label": "Ollama",
        "base_url": "http://127.0.0.1:11434",
        "enabled": true
    }));

    round_trip::<acpilot_proto::model::DiscoveredModel>(json!({
        "id": "qwen2.5-coder-7b",
        "provider_id": "lmstudio",
        "model_type": "chat",
        "architecture": "qwen2",
        "quantization": "Q4_K_M",
        "loaded": true,
        "context_window": 32768,
        "context_source": "server",
        "capabilities": {
            "chat": true,
            "streaming": true,
            "tools": true,
            "structured_output": false,
            "vision": false
        }
    }));

    // The OpenAI-compatible dialects do not report load state; absence is
    // information, so it is not defaulted to `false`.
    round_trip::<acpilot_proto::model::DiscoveredModel>(json!({
        "id": "llama3",
        "provider_id": "ollama",
        "model_type": "other",
        "context_source": "unknown",
        "capabilities": {
            "chat": true,
            "streaming": true,
            "tools": false,
            "structured_output": false,
            "vision": false
        }
    }));

    round_trip::<acpilot_proto::model::SamplingParams>(json!({
        "temperature": 0.2,
        "top_p": 0.95,
        "max_tokens": 512,
        "top_k": 40,
        "repeat_penalty": 1.1,
        "seed": 7,
        "stop": ["\n\n"]
    }));

    round_trip::<acpilot_proto::model::SamplingParams>(json!({
        "temperature": 0.2,
        "top_p": 0.95,
        "stop": []
    }));

    round_trip::<acpilot_proto::model::ModelRecipe>(json!({
        "schema_version": 1,
        "id": "rust-fixer",
        "label": "Rust fixer",
        "provider_id": "lmstudio",
        "model_id": "qwen2.5-coder-7b",
        "system_prompt": "You fix Rust.",
        "sampling": {
            "temperature": 0.2,
            "top_p": 0.95,
            "stop": []
        },
        "context": {
            "window": 32768,
            "reserve_output": 4096,
            "strategy": "truncate_oldest",
            "max_corpus_bytes": 262144
        },
        "tools": {
            "read_files": true,
            "write_files": false,
            "run_commands": false,
            "search": true,
            "max_steps": 8
        },
        "updated_at": "2026-10-21T09:31:12Z"
    }));

    round_trip::<acpilot_proto::model::RunRequest>(json!({
        "prompt": "fix it",
        "corpus": [
            { "glob": "docs/**/*.md", "max_files": 20, "max_bytes": 8192 }
        ]
    }));

    round_trip::<acpilot_proto::model::RunSummary>(json!({
        "id": "run-1",
        "recipe_id": "rust-fixer",
        "status": "succeeded",
        "started_at": "2026-10-21T09:31:12Z",
        "first_token_ms": 120,
        "total_ms": 2400,
        "tokens": { "prompt": 900, "completion": 80, "estimated": false },
        "tokens_per_second": 33.3,
        "stop_reason": "stop",
        "warnings": [],
        "params": { "temperature": 0.2, "top_p": 0.95, "stop": [] },
        "corpus_files": 2
    }));

    round_trip::<acpilot_proto::model::PromoteRecipeRequest>(json!({
        "agent_id": "rust-fixer"
    }));

    round_trip::<acpilot_proto::model::PromotedAgent>(json!({
        "agent": {
            "id": "rust-fixer",
            "name": "Rust fixer",
            "transport": "acp_stdio",
            "status": "available",
            "command": "acpilot-agent-lm --recipe rust-fixer"
        },
        "manifest_path": "profiles/work/agents.toml",
        "warnings": ["the server has never loaded this model"]
    }));
}

#[test]
fn extension_documents() {
    round_trip::<acpilot_proto::extension::ExtensionSummary>(json!({
        "id": "acpilot.language-servers",
        "name": "Language servers",
        "version": "1.0.0",
        "publisher": "acpilot",
        "description": "Presets for rust-analyzer, pyright and friends.",
        "kinds": ["language_servers"],
        "status": "active",
        "source": "registry",
        "checksum": "sha256:abc",
        "installed_at": "2026-10-21T09:31:12Z",
        "homepage": "https://example.invalid"
    }));

    round_trip::<acpilot_proto::extension::ExtensionSummary>(json!({
        "id": "acpilot.night-flight",
        "name": "Night flight",
        "version": "1.0.0",
        "publisher": "acpilot",
        "description": "Dark theme.",
        "kinds": ["theme"],
        "status": "installed",
        "source": "path"
    }));

    round_trip::<acpilot_proto::extension::ExtensionDetail>(json!({
        "summary": {
            "id": "acpilot.night-flight",
            "name": "Night flight",
            "version": "1.0.0",
            "publisher": "acpilot",
            "description": "Dark theme.",
            "kinds": ["theme"],
            "status": "active",
            "source": "path"
        },
        "contributions": [
            { "kind": "theme", "id": "acpilot.night-flight", "summary": "Dark" }
        ],
        "manifest": "schema_version = 1\n",
        "active_profiles": ["work"]
    }));

    round_trip::<acpilot_proto::extension::InstallExtensionRequest>(json!({
        "id": "acpilot.pack",
        "activate": true
    }));

    round_trip::<acpilot_proto::extension::InstallExtensionRequest>(json!({
        "id": "acpilot.pack",
        "version": "1.2.0",
        "path": "/tmp/pack",
        "activate": false
    }));

    round_trip::<acpilot_proto::extension::ThemeSummary>(json!({
        "id": "acpilot.night-flight",
        "name": "Night flight",
        "appearance": "dark",
        "built_in": true
    }));

    round_trip::<acpilot_proto::extension::ThemeSummary>(json!({
        "id": "acme.hangar",
        "name": "Hangar",
        "appearance": "light",
        "built_in": false,
        "extension_id": "acme.themes"
    }));
}

#[test]
fn bridge_documents() {
    round_trip::<acpilot_proto::bridge::AcpBridgeSummary>(json!({
        "enabled": true,
        "transports": ["stdio", "http_json_rpc"],
        "bind": "127.0.0.1:7465",
        "public_url": "https://acpilot.invalid",
        "require_token": true,
        "connected": 1,
        "known_drivers": 3
    }));

    round_trip::<acpilot_proto::bridge::AcpDriverSummary>(json!({
        "id": "drv-1",
        "name": "devin",
        "client": "devin 2.1",
        "peer": "stdio",
        "status": "connected",
        "scopes": ["observe", "prompt"],
        "connected_at": "2026-10-21T09:31:12Z",
        "last_seen_at": "2026-10-21T09:32:12Z",
        "sessions": 1,
        "prompt_turns": 4
    }));

    round_trip::<acpilot_proto::bridge::AcpDriverSummary>(json!({
        "id": "drv-2",
        "name": "ci",
        "peer": "10.0.0.4:51234",
        "status": "revoked",
        "scopes": [],
        "sessions": 0,
        "prompt_turns": 0
    }));

    round_trip::<acpilot_proto::bridge::PairingCode>(json!({
        "code": "ALFA-NINER",
        "expires_at": "2026-10-21T09:33:12Z"
    }));

    round_trip::<acpilot_proto::bridge::PairingCode>(json!({
        "code": "BRAVO-SIX",
        "expires_at": "2026-10-21T09:33:12Z",
        "handshake_url": "https://acpilot.invalid/v1/acp/bridge"
    }));

    round_trip::<acpilot_proto::bridge::PairDriverRequest>(json!({
        "code": "ALFA-NINER",
        "name": "devin",
        "requested_scopes": ["observe", "prompt", "cancel", "answer_permissions"]
    }));
}

#[test]
fn stats_documents() {
    use acpilot_proto::stats::{
        ContextStats, CreditStats, IntegrationKind, IntegrationStats, StatSource,
    };

    round_trip::<IntegrationStats>(json!({
        "id": "lmstudio",
        "kind": "provider",
        "label": "LM Studio",
        "context": {
            "used_tokens": 4096,
            "window_tokens": 32768,
            "source": "server"
        },
        "credits": {
            "used": 25.0,
            "remaining": 75.0,
            "limit": 100.0,
            "currency": "credits",
            "source": "server"
        },
        "updated_at": "2026-10-21T09:31:12Z"
    }));

    // An agent may know a locally counted context but not the window it counts
    // against, and a plan may report a balance without a ceiling. Both are
    // valid: absence is information, and the bar renders indeterminate rather
    // than guessing.
    round_trip::<IntegrationStats>(json!({
        "id": "claude",
        "kind": "agent",
        "label": "Claude Code",
        "context": {
            "used_tokens": 12000,
            "source": "estimated"
        },
        "credits": {
            "remaining": 3.5,
            "currency": "USD",
            "source": "configured"
        },
        "updated_at": "2026-10-21T09:31:12Z"
    }));

    // Nothing reported at all: the row exists so the bar does not vanish.
    round_trip::<IntegrationStats>(json!({
        "id": "ollama",
        "kind": "provider",
        "label": "Ollama",
        "updated_at": "2026-10-21T09:31:12Z"
    }));

    round_trip::<ContextStats>(json!({
        "used_tokens": 0,
        "source": "unknown"
    }));

    round_trip::<CreditStats>(json!({
        "used": 0.0,
        "remaining": 10.0,
        "currency": "credits",
        "source": "server"
    }));

    assert_wire!(IntegrationKind::Provider, "provider");
    assert_wire!(IntegrationKind::Agent, "agent");
    assert_wire!(StatSource::Server, "server");
    assert_wire!(StatSource::Configured, "configured");
    assert_wire!(StatSource::Estimated, "estimated");
    assert_wire!(StatSource::Unknown, "unknown");
}

#[test]
fn error_documents() {
    round_trip::<acpilot_proto::ErrorBody>(json!({
        "code": "not_found",
        "message": "not found: session s1",
        "details": [
            { "field": "session_id", "message": "does not exist" }
        ]
    }));

    // `details` has a default, so a hand-written envelope without it parses.
    let body: acpilot_proto::ErrorBody =
        serde_json::from_value(json!({ "code": "io", "message": "disk full" }))
            .expect("details are optional on read");
    assert!(body.details.is_empty());
    assert_eq!(
        serde_json::to_value(&body).expect("serialises"),
        json!({ "code": "io", "message": "disk full" }),
        "empty details must not be written as null or []"
    );
}

#[test]
fn every_enum_has_one_stable_wire_spelling() {
    use acpilot_proto::{agent, bridge, extension, host, lsp, model, permission, session};

    assert_wire!(agent::AgentTransport::AcpStdio, "acp_stdio");
    assert_wire!(agent::AgentTransport::Pty, "pty");
    assert_wire!(agent::AgentTransport::HttpJsonRpc, "http_json_rpc");
    assert_wire!(agent::AgentStatus::Available, "available");
    assert_wire!(agent::AgentStatus::Missing, "missing");
    assert_wire!(agent::AgentStatus::Unhealthy, "unhealthy");
    assert_wire!(agent::AgentStatus::Untrusted, "untrusted");

    assert_wire!(bridge::DriverScope::Observe, "observe");
    assert_wire!(bridge::DriverScope::Prompt, "prompt");
    assert_wire!(bridge::DriverScope::Cancel, "cancel");
    assert_wire!(bridge::DriverScope::AnswerPermissions, "answer_permissions");
    assert_wire!(bridge::BridgeTransport::Stdio, "stdio");
    assert_wire!(bridge::BridgeTransport::HttpJsonRpc, "http_json_rpc");
    assert_wire!(bridge::DriverStatus::PendingPairing, "pending_pairing");
    assert_wire!(bridge::DriverStatus::Connected, "connected");
    assert_wire!(bridge::DriverStatus::Disconnected, "disconnected");
    assert_wire!(bridge::DriverStatus::Revoked, "revoked");

    assert_wire!(host::HostKind::Local, "local");
    assert_wire!(host::HostKind::Ssh, "ssh");
    assert_wire!(host::HostKind::Wsl, "wsl");

    assert_wire!(lsp::PositionEncoding::Utf8, "utf8");
    assert_wire!(lsp::PositionEncoding::Utf16, "utf16");
    assert_wire!(lsp::LspServerStatus::Starting, "starting");
    assert_wire!(lsp::LspServerStatus::Ready, "ready");
    assert_wire!(lsp::LspServerStatus::Indexing, "indexing");
    assert_wire!(lsp::LspServerStatus::Crashed, "crashed");
    assert_wire!(lsp::LspServerStatus::Stopped, "stopped");

    assert_wire!(model::ProviderKind::LmStudio, "lm_studio");
    assert_wire!(model::ProviderKind::OpenAiCompatible, "open_ai_compatible");
    assert_wire!(model::ProviderStatus::Disabled, "disabled");
    assert_wire!(model::ProviderStatus::Reachable, "reachable");
    assert_wire!(model::ProviderStatus::Unreachable, "unreachable");
    assert_wire!(model::ProviderStatus::Unauthorized, "unauthorized");
    assert_wire!(model::MetadataSource::Server, "server");
    assert_wire!(model::MetadataSource::Recipe, "recipe");
    assert_wire!(model::MetadataSource::Unknown, "unknown");
    assert_wire!(model::ModelType::Chat, "chat");
    assert_wire!(model::ModelType::Vision, "vision");
    assert_wire!(model::ModelType::Embedding, "embedding");
    assert_wire!(model::ModelType::Other, "other");
    assert_wire!(model::ContextStrategy::TruncateOldest, "truncate_oldest");
    assert_wire!(model::ContextStrategy::Summarize, "summarize");
    assert_wire!(model::ContextStrategy::Refuse, "refuse");
    assert_wire!(model::RunStatus::Running, "running");
    assert_wire!(model::RunStatus::Succeeded, "succeeded");
    assert_wire!(model::RunStatus::Failed, "failed");
    assert_wire!(model::RunStatus::Cancelled, "cancelled");

    assert_wire!(permission::PermissionKind::FileWrite, "file_write");
    assert_wire!(permission::PermissionKind::FileRead, "file_read");
    assert_wire!(permission::PermissionKind::Terminal, "terminal");
    assert_wire!(permission::PermissionKind::ToolCall, "tool_call");
    assert_wire!(permission::PermissionKind::Other, "other");
    assert_wire!(permission::PermissionOption::AllowOnce, "allow_once");
    assert_wire!(permission::PermissionOption::AllowAlways, "allow_always");
    assert_wire!(permission::PermissionOption::Deny, "deny");

    assert_wire!(session::SessionStatus::Idle, "idle");
    assert_wire!(session::SessionStatus::Busy, "busy");
    assert_wire!(
        session::SessionStatus::AwaitingPermission,
        "awaiting_permission"
    );
    assert_wire!(session::SessionStatus::Closed, "closed");
    assert_wire!(session::SessionStatus::Failed, "failed");

    assert_wire!(extension::ExtensionKind::Theme, "theme");
    assert_wire!(
        extension::ExtensionKind::LanguageServers,
        "language_servers"
    );
    assert_wire!(extension::ExtensionKind::Agents, "agents");
    assert_wire!(extension::ExtensionKind::Grammars, "grammars");
    assert_wire!(extension::ExtensionKind::Snippets, "snippets");
    assert_wire!(extension::ExtensionKind::Keymap, "keymap");
    assert_wire!(
        extension::ExtensionKind::TranscriptParser,
        "transcript_parser"
    );
    assert_wire!(extension::ExtensionSource::Registry, "registry");
    assert_wire!(extension::ExtensionSource::Path, "path");
    assert_wire!(extension::ExtensionStatus::Installed, "installed");
    assert_wire!(extension::ExtensionStatus::Active, "active");
    assert_wire!(extension::ExtensionStatus::Disabled, "disabled");
    assert_wire!(extension::ExtensionStatus::Blocked, "blocked");
    assert_wire!(extension::ThemeAppearance::Dark, "dark");
    assert_wire!(extension::ThemeAppearance::Light, "light");
}

#[test]
fn events_and_frames_use_the_documented_sequence() {
    // Sequence numbers start at 1 and are 64-bit; a client resuming from
    // `Last-Event-ID` sends the last one it saw.
    let first: acpilot_proto::event::SessionEvent = serde_json::from_value(json!({
        "seq": 1,
        "session_id": "s-1",
        "kind": "message_chunk",
        "at": "2026-10-21T09:31:12Z",
        "payload": { "text": "hello" }
    }))
    .expect("first event parses");
    assert_eq!(first.seq, 1);

    let huge: acpilot_proto::event::SessionEvent = serde_json::from_value(json!({
        "seq": u64::MAX,
        "session_id": "s-1",
        "kind": "notice",
        "at": "2026-10-21T09:31:12Z",
        "payload": null
    }))
    .expect("full-width sequence parses");
    assert_eq!(huge.seq, u64::MAX);
}
