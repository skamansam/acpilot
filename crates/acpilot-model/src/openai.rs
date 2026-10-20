//! The OpenAI-compatible surface (`/v1`).
//!
//! Ollama, llama.cpp, vLLM, LM Studio in compatible mode, and hosted endpoints
//! all answer the same two routes. The adapter is deliberately dull: a model list
//! that carries almost no metadata, and a chat route that carries no timings.
//!
//! The consequence is visible in the product rather than hidden here. With no
//! `max_context_length` the provider's window is `None` and the recipe must state
//! one; with no `state` the load state is `None` rather than a confident "not
//! loaded". Fewer gauges is the honest outcome of fewer facts.

use acpilot_core::AcpilotError;
use acpilot_proto::model::{
    DiscoveredModel, MetadataSource, ModelCapabilities, ModelType, ProviderKind,
};
use serde::Deserialize;
use serde_json::Value;

use crate::wire;
use crate::{
    ChatRequest, ChatResponse, DISCOVERY_TIMEOUT, Provider, post_chat, transport_error, unexpected,
    wire::chat_body,
};

/// The OpenAI-compatible adapter.
#[derive(Debug, Clone)]
pub struct OpenAi {
    provider: Provider,
    client: reqwest::Client,
}

impl OpenAi {
    /// Wrap a provider and a shared HTTP client.
    #[must_use]
    pub fn new(provider: Provider, client: reqwest::Client) -> Self {
        debug_assert_eq!(provider.kind, ProviderKind::OpenAiCompatible);
        Self { provider, client }
    }

    /// The provider this adapter speaks for.
    #[must_use]
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// `GET /v1/models`.
    pub async fn models(&self) -> Result<Vec<DiscoveredModel>, AcpilotError> {
        let url = self.provider.url("/v1/models");
        let mut request = self.client.get(&url).timeout(DISCOVERY_TIMEOUT);
        if let Some(token) = &self.provider.token {
            request = request.bearer_auth(token);
        }

        let response = request
            .send()
            .await
            .map_err(|error| transport_error(&self.provider.id, &error))?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(unexpected(
                &self.provider.id,
                &format!(
                    "model list answered {status}{}",
                    if detail.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", crate::truncate(&detail, 400))
                    }
                ),
            ));
        }

        let body: Value = response
            .json()
            .await
            .map_err(|error| unexpected(&self.provider.id, &error))?;
        let list: ModelList =
            serde_json::from_value(body).map_err(|error| unexpected(&self.provider.id, &error))?;

        Ok(list
            .data
            .into_iter()
            .map(|row| DiscoveredModel {
                id: row.id,
                provider_id: self.provider.id.clone(),
                model_type: ModelType::Chat,
                architecture: None,
                quantization: None,
                loaded: None,
                context_window: None,
                context_source: MetadataSource::Unknown,
                capabilities: ModelCapabilities {
                    chat: true,
                    vision: false,
                    streaming: false,
                    tools: false,
                    structured_output: false,
                },
            })
            .collect())
    }

    /// `POST /v1/chat/completions`.
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AcpilotError> {
        let url = self.provider.url("/v1/chat/completions");
        let mut body = chat_body(request);
        let (value, unsupported) = post_chat(&self.client, &self.provider, &url, &mut body).await?;
        wire::parse(value, unsupported).map_err(|error| unexpected(&self.provider.id, &error))
    }
}

/// `GET /v1/models` response, which is all model ids and nothing else.
#[derive(Debug, Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelRow>,
}

/// One row: an id, and whatever the server felt like adding.
#[derive(Debug, Deserialize)]
struct ModelRow {
    id: String,
}
