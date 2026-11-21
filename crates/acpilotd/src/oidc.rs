//! OIDC authorization-code login with PKCE (ADR 0013 §5).
//!
//! OIDC is the second way to obtain the same local session the password path
//! gives: it does not create an account, and it does not match by email. A local
//! `acpilot auth link <provider> <subject>` records `(issuer, subject)` against the
//! owner; the callback only succeeds for an identity that row names.
//!
//! # What is verified
//!
//! The discovery document's issuer must string-match the configured issuer. The ID
//! token's signature is checked against the provider's JWKS, using the algorithm
//! the key advertises, never the token's word for it. `iss`, `aud`, `exp`, `nbf`
//! and the nonce are checked. The `state` names a one-shot pending row, so a
//! replayed callback has no verifier left.

use std::collections::HashMap;

use axum::http::header;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use serde_json::Value;

use crate::accounts::{Accounts, PendingOidc, Session, User};

/// How long a browser has to finish an authorization request.
const PENDING_TTL_SECONDS: i64 = 10 * 60;

/// How long the discovery/JWKS HTTP calls may take.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Provider settings from `<data>/auth.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    /// Stable id used in the URL and the CLI.
    pub id: String,
    /// The exact issuer string the discovery document must repeat.
    pub issuer: String,
    /// The OIDC client id.
    pub client_id: String,
    /// A confidential client's secret, when the provider requires one.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Scopes to request. `openid` is added if omitted.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// A deployment's fixed callback, when it is not derived from the request.
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Config {
    #[serde(default)]
    oidc: Vec<Provider>,
}

/// Read the configured providers. A missing `auth.toml` means none.
///
/// # Errors
///
/// The file exists and cannot be read or parsed, or a provider id is duplicated.
pub fn providers() -> Result<Vec<Provider>, String> {
    let path = acpilot_profile::data_dir().join("auth.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let config: Config =
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut seen = std::collections::HashSet::new();
    for provider in &config.oidc {
        if provider.id.is_empty()
            || !provider
                .id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
        {
            return Err(format!(
                "{}: `{}` is not a provider id",
                path.display(),
                provider.id
            ));
        }
        if !seen.insert(provider.id.as_str()) {
            return Err(format!(
                "{}: provider id `{}` appears twice",
                path.display(),
                provider.id
            ));
        }
    }
    Ok(config.oidc)
}

/// One configured provider by id.
///
/// # Errors
///
/// No provider has that id, or `auth.toml` is unreadable.
pub fn provider(id: &str) -> Result<Provider, String> {
    providers()?
        .into_iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| format!("no OIDC provider `{id}` in auth.toml"))
}

/// Link an identity to the local owner, as `acpilot auth link` does.
///
/// # Errors
///
/// The provider is unknown, no account exists, or the identity is already linked.
pub fn link_local(provider_id: &str, subject: &str, email: Option<&str>) -> Result<User, String> {
    let provider = provider(provider_id)?;
    Accounts::default()
        .link_identity(&provider.issuer, subject, email)
        .map_err(|error| error.to_string())
}

/// Start an authorization-code + PKCE flow and return the provider URL.
///
/// # Errors
///
/// The provider is unknown, its discovery document cannot be fetched or does not
/// name the configured issuer, or the pending row cannot be written.
pub async fn start(provider_id: &str, base_url: &str) -> Result<String, String> {
    let provider = provider(provider_id)?;
    let discovery = discover(&provider).await?;
    let state = random_hex(32)?;
    let nonce = random_hex(32)?;
    let verifier = random_hex(32)?;
    let challenge = base64url(&sha256(verifier.as_bytes()));
    let redirect_uri = redirect_uri(&provider, base_url);
    let pending = PendingOidc {
        provider: provider.id.clone(),
        nonce,
        verifier,
        expires_at: jiff::Timestamp::now().as_second() + PENDING_TTL_SECONDS,
    };
    Accounts::default()
        .save_pending_oidc(&pending, &state)
        .map_err(|error| error.to_string())?;

    let mut url = url::Url::parse(&discovery.authorization_endpoint)
        .map_err(|error| format!("the provider's authorization endpoint is not a URL: {error}"))?;
    let mut scopes = provider.scopes.clone();
    if !scopes.iter().any(|scope| scope == "openid") {
        scopes.insert(0, "openid".to_owned());
    }
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &provider.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("state", &state)
        .append_pair("nonce", &pending.nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// Finish a callback: exchange the code, verify the ID token, and issue a session.
///
/// # Errors
///
/// The state is unknown or expired, the provider refuses the code, the ID token
/// does not verify, or no local account is linked to the identity.
pub async fn callback(state: &str, code: &str, base_url: &str) -> Result<Session, String> {
    let accounts = Accounts::default();
    let pending = accounts
        .take_pending_oidc(state)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the sign-in request expired or was already used".to_owned())?;
    let provider = provider(&pending.provider)?;
    let discovery = discover(&provider).await?;
    let redirect_uri = redirect_uri(&provider, base_url);

    let client = client()?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", redirect_uri),
        ("client_id", provider.client_id.clone()),
        ("code_verifier", pending.verifier.clone()),
    ];
    if let Some(secret) = &provider.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form.iter().map(|(key, value)| (*key, value.as_str())))
        .finish();
    let token: TokenResponse = client
        .post(&discovery.token_endpoint)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(encoded)
        .send()
        .await
        .map_err(|error| format!("the provider's token endpoint could not be reached: {error}"))?
        .error_for_status()
        .map_err(|error| format!("the provider refused the authorization code: {error}"))?
        .json()
        .await
        .map_err(|error| format!("the provider's token answer is not JSON: {error}"))?;

    let claims = verify_id_token(&token.id_token, &provider, &discovery, &pending.nonce).await?;
    let user = accounts
        .user_for_identity(&provider.issuer, &claims.sub)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            format!(
                "the provider identity `{}` is not linked to a local account; run `acpilot auth link {} <subject>` on the daemon's machine",
                claims.sub, provider.id
            )
        })?;
    accounts.issue(&user).map_err(|error| error.to_string())
}

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Debug, Deserialize)]
struct IdClaims {
    iss: String,
    sub: String,
    #[serde(default)]
    aud: Value,
    exp: i64,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default, rename = "email")]
    _email: Option<String>,
}

impl IdClaims {
    fn audience_matches(&self, client_id: &str) -> bool {
        match &self.aud {
            Value::String(audience) => audience == client_id,
            Value::Array(audiences) => audiences.iter().any(|audience| audience == client_id),
            _ => false,
        }
    }
}

async fn discover(provider: &Provider) -> Result<Discovery, String> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        provider.issuer.trim_end_matches('/')
    );
    let discovery: Discovery = client()?
        .get(&url)
        .send()
        .await
        .map_err(|error| {
            format!("the provider's discovery document could not be fetched: {error}")
        })?
        .error_for_status()
        .map_err(|error| format!("the provider's discovery document was refused: {error}"))?
        .json()
        .await
        .map_err(|error| format!("the provider's discovery document is not JSON: {error}"))?;
    if discovery.issuer != provider.issuer {
        return Err(format!(
            "the provider says its issuer is `{}` and auth.toml says `{}`",
            discovery.issuer, provider.issuer
        ));
    }
    Ok(discovery)
}

async fn verify_id_token(
    token: &str,
    provider: &Provider,
    discovery: &Discovery,
    nonce: &str,
) -> Result<IdClaims, String> {
    let header = decode_header(token)
        .map_err(|error| format!("the provider's ID token has no readable header: {error}"))?;
    if header.alg == Algorithm::HS256
        || header.alg == Algorithm::HS384
        || header.alg == Algorithm::HS512
    {
        return Err("the provider signed its ID token with an HMAC algorithm; that is never a public-key login".to_owned());
    }
    let keys: JwkSet = client()?
        .get(&discovery.jwks_uri)
        .send()
        .await
        .map_err(|error| format!("the provider's JWKS could not be fetched: {error}"))?
        .error_for_status()
        .map_err(|error| format!("the provider's JWKS was refused: {error}"))?
        .json()
        .await
        .map_err(|error| format!("the provider's JWKS is not JSON: {error}"))?;
    let jwk = keys
        .keys
        .iter()
        .find(|key| match (&header.kid, &key.common.key_id) {
            (Some(wanted), Some(known)) => wanted == known,
            (None, _) => true,
            _ => false,
        })
        .ok_or_else(|| "the provider's JWKS has no key for this ID token".to_owned())?;
    let key = DecodingKey::from_jwk(jwk)
        .map_err(|error| format!("the provider's JWKS key is unusable: {error}"))?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[provider.issuer.as_str()]);
    validation.set_audience(&[provider.client_id.as_str()]);
    validation.leeway = 30;
    validation.validate_nbf = true;
    let decoded = decode::<IdClaims>(token, &key, &validation)
        .map_err(|error| format!("the provider's ID token did not verify: {error}"))?;
    let claims = decoded.claims;
    if claims.iss != provider.issuer {
        return Err("the ID token's issuer is not the configured issuer".to_owned());
    }
    if !claims.audience_matches(&provider.client_id) {
        return Err("the ID token's audience does not contain this client id".to_owned());
    }
    if claims.exp <= jiff::Timestamp::now().as_second() {
        return Err("the ID token has expired".to_owned());
    }
    match claims.nonce.as_deref() {
        Some(presented) if presented == nonce => {}
        _ => return Err("the ID token's nonce does not match this sign-in request".to_owned()),
    }
    Ok(claims)
}

fn redirect_uri(provider: &Provider, base_url: &str) -> String {
    provider
        .redirect_uri
        .clone()
        .unwrap_or_else(|| format!("{}/v1/auth/oidc/callback", base_url.trim_end_matches('/')))
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| format!("cannot build the OIDC HTTP client: {error}"))
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|error| format!("no random source: {error}"))?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

/// base64url without padding, the alphabet PKCE uses.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let packed = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(char::from(ALPHABET[(packed >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(packed >> 12) as usize & 63]));
        if chunk.len() > 1 {
            out.push(char::from(ALPHABET[(packed >> 6) as usize & 63]));
        }
        if chunk.len() > 2 {
            out.push(char::from(ALPHABET[packed as usize & 63]));
        }
    }
    out
}

/// A map from a configured provider id to the fields a browser page may see.
#[must_use]
pub fn public_providers(configured: &[Provider]) -> Vec<HashMap<&str, &str>> {
    configured
        .iter()
        .map(|provider| {
            HashMap::from([
                ("id", provider.id.as_str()),
                ("issuer", provider.issuer.as_str()),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pkce_challenge_is_s256_base64url() {
        // RFC 7636 appendix B's verifier and challenge.
        assert_eq!(
            base64url(&sha256(b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn an_audience_is_a_string_or_an_array() {
        let one = IdClaims {
            iss: "i".into(),
            sub: "s".into(),
            aud: Value::String("client".into()),
            exp: 1,
            nonce: None,
            _email: None,
        };
        assert!(one.audience_matches("client"));
        assert!(!one.audience_matches("other"));
        let many = IdClaims {
            aud: Value::Array(vec![
                Value::String("other".into()),
                Value::String("client".into()),
            ]),
            ..one
        };
        assert!(many.audience_matches("client"));
    }

    #[test]
    fn a_provider_page_never_gets_the_client_secret() {
        let configured = vec![Provider {
            id: "work".into(),
            issuer: "https://id.example".into(),
            client_id: "acpilot".into(),
            client_secret: Some("secret".into()),
            scopes: vec![],
            redirect_uri: None,
        }];
        let shown = public_providers(&configured);
        assert_eq!(shown[0].get("id"), Some(&"work"));
        assert!(!shown[0].contains_key("client_secret"));
    }
}
