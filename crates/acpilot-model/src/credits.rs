//! A provider's own credit balance, where the provider publishes one.
//!
//! Credits are not a property of a wire dialect: two OpenAI-shaped servers, one
//! local and one hosted, answer `/v1/models` identically and only the hosted one
//! has a balance. This build reads the endpoint it knows — OpenRouter's
//! `GET /api/v1/credits` — and reports nothing for every other provider rather
//! than guessing a URL. A `404` drawn as `0.00` is a balance nobody reported.
//!
//! The numbers are OpenRouter's: `total_credits` is what was purchased and
//! `total_usage` is what has been spent, so the bar's ceiling is the first and the
//! balance is the difference. Both are in USD.

use std::time::Duration;

use acpilot_core::AcpilotError;
use acpilot_proto::stats::{CreditStats, StatSource};
use serde::Deserialize;
use serde_json::Value;

use crate::{transport_error, unexpected};

/// How long a balance read may take.
///
/// Shorter than discovery on purpose: `/v1/stats` is polled while somebody
/// watches, and a minute of stale credits is better than a stats request held
/// open behind a firewall.
pub const BALANCE_TIMEOUT: Duration = Duration::from_secs(5);

/// The host whose balance endpoint this build knows.
const OPENROUTER: &str = "openrouter.ai";

/// `GET /api/v1/credits` for an OpenRouter base URL, and `None` for anything else.
///
/// A hosted base is `https://openrouter.ai/api/v1`. A base without the `/v1` path
/// is completed from the same host rather than appended to blindly, so a
/// declaration that stops at the host still asks the endpoint the vendor
/// documents.
#[must_use]
pub fn balance_endpoint(base_url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(base_url).ok()?;
    let host = parsed.host_str()?;
    if host != OPENROUTER && !host.ends_with(".openrouter.ai") {
        return None;
    }
    let base = if parsed.path().trim_end_matches('/').ends_with("/v1") {
        base_url.trim_end_matches('/').to_owned()
    } else {
        format!("https://{host}/api/v1")
    };
    Some(format!("{base}/credits"))
}

/// Read a provider's balance, when this build knows where to ask.
///
/// `Ok(None)` means "this provider publishes nothing this build can read", which
/// is not a zero balance. A refusal or an unreadable body is an `Err`, so the
/// caller can keep the row quiet instead of reporting a number nobody sent.
///
/// # Errors
///
/// The balance endpoint could not be reached, refused the credential, or answered
/// something that is not a balance.
pub async fn balance(
    provider: &str,
    base_url: &str,
    token: Option<&str>,
) -> Result<Option<CreditStats>, AcpilotError> {
    let Some(endpoint) = balance_endpoint(base_url) else {
        return Ok(None);
    };
    fetch(provider, &endpoint, token).await
}

/// The HTTP half, separate so a test can point it at a loopback server.
async fn fetch(
    provider: &str,
    endpoint: &str,
    token: Option<&str>,
) -> Result<Option<CreditStats>, AcpilotError> {
    let client = crate::client()?;
    let mut request = client.get(endpoint).timeout(BALANCE_TIMEOUT);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| transport_error(provider, &error))?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(unexpected(
            provider,
            &format!("the balance endpoint answered {status}"),
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| unexpected(provider, &error))?;
    Ok(reading(&body))
}

/// OpenRouter's `{"data": {"total_credits": …, "total_usage": …}}`.
#[derive(Debug, Default, Deserialize)]
struct Purchases {
    #[serde(default)]
    total_credits: Option<f64>,
    #[serde(default)]
    total_usage: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Option<Purchases>,
}

/// Map a balance body onto the contract.
///
/// A body without `total_credits` reports nothing: the amount purchased is the
/// only denominator OpenRouter states, and inventing one would turn a quiet
/// account into a confident zero.
fn reading(body: &Value) -> Option<CreditStats> {
    let envelope: Envelope = serde_json::from_value(body.clone()).ok()?;
    let purchases = envelope.data?;
    let limit = purchases.total_credits?;
    Some(CreditStats {
        used: purchases.total_usage,
        remaining: Some(limit - purchases.total_usage.unwrap_or(0.0)),
        limit: Some(limit),
        currency: "USD".to_owned(),
        source: StatSource::Server,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;

    use super::*;

    /// Bind a loopback server and return its base URL.
    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        format!("http://{address}")
    }

    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<Option<String>>>>);

    async fn credits(State(seen): State<Seen>, headers: HeaderMap) -> Json<Value> {
        seen.0.lock().expect("seen").push(
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        );
        Json(json!({
            "data": { "total_credits": 100.0, "total_usage": 12.5 }
        }))
    }

    #[test]
    fn only_openrouter_has_a_balance_endpoint() {
        assert_eq!(
            balance_endpoint("https://openrouter.ai/api/v1").as_deref(),
            Some("https://openrouter.ai/api/v1/credits")
        );
        assert_eq!(
            balance_endpoint("https://openrouter.ai").as_deref(),
            Some("https://openrouter.ai/api/v1/credits")
        );
        assert_eq!(
            balance_endpoint("https://eu.openrouter.ai/api/v1/").as_deref(),
            Some("https://eu.openrouter.ai/api/v1/credits")
        );
        assert_eq!(balance_endpoint("http://127.0.0.1:1234/v1"), None);
        assert_eq!(balance_endpoint("https://example.com/api/v1"), None);
    }

    #[test]
    fn a_balance_is_what_was_bought_and_what_was_spent() {
        let stats = reading(&json!({
            "data": { "total_credits": 100.0, "total_usage": 12.5 }
        }))
        .expect("a reading");
        assert_eq!(stats.used, Some(12.5));
        assert_eq!(stats.remaining, Some(87.5));
        assert_eq!(stats.limit, Some(100.0));
        assert_eq!(stats.currency, "USD");
        assert_eq!(stats.source, StatSource::Server);
    }

    #[test]
    fn a_body_without_a_purchase_is_no_reading_rather_than_zero() {
        assert_eq!(reading(&json!({ "data": { "total_usage": 3.0 } })), None);
        assert_eq!(reading(&json!({})), None);
    }

    #[tokio::test]
    async fn the_read_sends_the_token_and_turns_the_body_into_credits() {
        let seen = Seen::default();
        let base = serve(
            Router::new()
                .route("/api/v1/credits", get(credits))
                .with_state(seen.clone()),
        )
        .await;

        let stats = fetch(
            "openrouter",
            &format!("{base}/api/v1/credits"),
            Some("secret"),
        )
        .await
        .expect("the call")
        .expect("a reading");
        assert_eq!(stats.remaining, Some(87.5));
        assert_eq!(
            seen.0.lock().expect("seen").as_slice(),
            &[Some("Bearer secret".to_owned())],
            "the balance is read with the declaration's credential"
        );
    }

    #[tokio::test]
    async fn a_refused_credential_is_an_error_not_a_zero_balance() {
        let base =
            serve(Router::new().route("/credits", get(|| async { StatusCode::UNAUTHORIZED })))
                .await;
        let error = fetch("openrouter", &format!("{base}/credits"), None)
            .await
            .expect_err("a refusal is not a reading");
        assert!(error.to_string().contains("401"), "{error}");
    }
}
