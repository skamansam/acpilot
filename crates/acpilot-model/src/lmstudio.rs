//! LM Studio's native REST surface (`/api/v0`).
//!
//! Chosen as the reference provider because it answers the questions a lab needs:
//! `GET /api/v0/models` reports the model's type, architecture, quantization,
//! load state, and `max_context_length`, and `POST /api/v0/chat/completions`
//! reports time to first token and tokens per second. Those facts are the whole
//! reason the workbench can show gauges instead of adjectives.

use acpilot_core::AcpilotError;
use acpilot_proto::model::{DiscoveredModel, ModelCapabilities, ModelType, ProviderKind};
use serde::Deserialize;
use serde_json::Value;

use crate::wire;
use crate::{
    ChatRequest, ChatResponse, DISCOVERY_TIMEOUT, Provider, post_chat, transport_error, unexpected,
    wire::chat_body,
};

/// The LM Studio adapter.
#[derive(Debug, Clone)]
pub struct LmStudio {
    provider: Provider,
    client: reqwest::Client,
}

impl LmStudio {
    /// Wrap a provider and a shared HTTP client.
    #[must_use]
    pub fn new(provider: Provider, client: reqwest::Client) -> Self {
        debug_assert_eq!(provider.kind, ProviderKind::LmStudio);
        Self { provider, client }
    }

    /// The provider this adapter speaks for.
    #[must_use]
    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// `GET /api/v0/models`.
    ///
    /// A field the server omits stays absent in our types: no default context
    /// window, no guessed load state. A model list is a report, not a form.
    pub async fn models(&self) -> Result<Vec<DiscoveredModel>, AcpilotError> {
        let url = self.provider.url("/api/v0/models");
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
            .map(|row| row.into_model(&self.provider.id))
            .collect())
    }

    /// `POST /api/v0/chat/completions`.
    pub async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, AcpilotError> {
        let url = self.provider.url("/api/v0/chat/completions");
        let mut body = chat_body(request);
        let (value, unsupported) = post_chat(&self.client, &self.provider, &url, &mut body).await?;
        wire::parse(value, unsupported).map_err(|error| unexpected(&self.provider.id, &error))
    }
}

/// `GET /api/v0/models` response.
#[derive(Debug, Deserialize)]
struct ModelList {
    #[serde(default)]
    data: Vec<ModelRow>,
}

/// One row of the model list.
#[derive(Debug, Deserialize)]
struct ModelRow {
    id: String,
    #[serde(default, rename = "type")]
    model_type: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    quantization: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    max_context_length: Option<u32>,
}

impl ModelRow {
    /// Map a row onto the wire type, keeping "unknown" unknown.
    fn into_model(self, provider_id: &str) -> DiscoveredModel {
        let model_type = match self.model_type.as_deref() {
            Some("llm") => ModelType::Chat,
            Some("vlm") => ModelType::Vision,
            Some("embeddings") => ModelType::Embedding,
            _ => ModelType::Other,
        };

        DiscoveredModel {
            id: self.id,
            provider_id: provider_id.to_owned(),
            model_type,
            architecture: self.arch,
            quantization: self.quantization,
            // `state` is `loaded` or `not-loaded`; anything else is not a claim we
            // are willing to make on the server's behalf.
            loaded: self.state.as_deref().map(|state| state == "loaded"),
            context_window: self.max_context_length,
            context_source: crate::source(self.max_context_length),
            capabilities: ModelCapabilities {
                // Anything that is not an embedding model can chat.
                chat: model_type != ModelType::Embedding,
                vision: model_type == ModelType::Vision,
                // Capabilities that discovery cannot establish are reported as
                // absent until the probe demonstrates them.
                streaming: false,
                tools: false,
                structured_output: false,
            },
        }
    }
}
