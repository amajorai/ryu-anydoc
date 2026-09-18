//! Core hop-token and standalone API-key authentication.

use std::collections::HashMap;

use axum::http::HeaderMap;
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct AuthConfig {
    core_token: Option<String>,
    api_tokens: HashMap<String, String>,
}

impl AuthConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        let core_token = non_empty_env("RYU_EXT_TOKEN");
        let api_tokens = if let Some(raw) = non_empty_env("RYU_ANYDOC_API_TOKENS") {
            parse_api_tokens(&raw)?
        } else if let Some(token) = non_empty_env("RYU_ANYDOC_API_TOKEN") {
            let tenant =
                non_empty_env("RYU_ANYDOC_TENANT_ID").unwrap_or_else(|| "default".to_owned());
            [(token, tenant)].into_iter().collect()
        } else {
            HashMap::new()
        };

        Ok(Self {
            core_token,
            api_tokens,
        })
    }

    /// Authenticate a request according to the route it is trying to reach.
    ///
    /// Core's injected token is accepted on the mounted Ryu surface and on the
    /// OpenAPI discovery route. The versioned root API accepts only customer API
    /// keys. A key can also call the mounted API when the binary is run directly.
    #[must_use]
    pub fn authorized(&self, path: &str, headers: &HeaderMap) -> bool {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let core_scope = path == "/openapi.json" || path.starts_with("/api/anydoc");
        if core_scope && ryu_sidecar_runtime::token_ok(presented, self.core_token.as_deref()) {
            return true;
        }

        self.api_tokens
            .keys()
            .any(|expected| ryu_sidecar_runtime::token_ok(presented, Some(expected)))
    }

    #[must_use]
    pub fn has_any_token(&self) -> bool {
        self.core_token.is_some() || !self.api_tokens.is_empty()
    }

    #[must_use]
    pub fn tenant_for(&self, headers: &HeaderMap) -> Option<&str> {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty());
        self.api_tokens.iter().find_map(|(expected, tenant)| {
            ryu_sidecar_runtime::token_ok(presented, Some(expected)).then_some(tenant.as_str())
        })
    }
}

fn parse_api_tokens(raw: &str) -> anyhow::Result<HashMap<String, String>> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| anyhow::anyhow!("RYU_ANYDOC_API_TOKENS must be valid JSON: {error}"))?;
    let object = value.as_object().ok_or_else(|| {
        anyhow::anyhow!("RYU_ANYDOC_API_TOKENS must be an object mapping tenants to API keys")
    })?;
    let mut tokens = HashMap::with_capacity(object.len());
    for (tenant, value) in object {
        let token = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if tenant.trim().is_empty() || token.is_none() {
            return Err(anyhow::anyhow!(
                "RYU_ANYDOC_API_TOKENS must contain non-empty tenant ids and API keys"
            ));
        }
        let token = token.expect("checked above").to_owned();
        if tokens.contains_key(&token) {
            return Err(anyhow::anyhow!(
                "RYU_ANYDOC_API_TOKENS must use a distinct API key for every tenant"
            ));
        }
        tokens.insert(token, tenant.trim().to_owned());
    }
    if tokens.is_empty() {
        return Err(anyhow::anyhow!(
            "RYU_ANYDOC_API_TOKENS must contain at least one tenant"
        ));
    }
    Ok(tokens)
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::AuthConfig;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).expect("valid header"),
        );
        headers
    }

    #[test]
    fn standalone_keys_are_scoped_and_core_tokens_do_not_authenticate_root_api() {
        let mut api_tokens = std::collections::HashMap::new();
        api_tokens.insert("customer-secret".to_owned(), "acme".to_owned());
        let config = AuthConfig {
            core_token: Some("core-secret".to_owned()),
            api_tokens,
        };

        assert!(config.authorized("/v1/extract", &headers("customer-secret")));
        assert!(!config.authorized("/v1/extract", &headers("core-secret")));
        assert!(config.authorized("/api/anydoc/parse", &headers("core-secret")));
        assert!(!config.authorized("/v1/extract", &HeaderMap::new()));
    }

    #[test]
    fn tenant_for_resolves_only_standalone_api_keys() {
        let mut api_tokens = std::collections::HashMap::new();
        api_tokens.insert("customer-secret".to_owned(), "acme".to_owned());
        let config = AuthConfig {
            core_token: Some("core-secret".to_owned()),
            api_tokens,
        };

        assert_eq!(config.tenant_for(&headers("customer-secret")), Some("acme"));
        assert_eq!(config.tenant_for(&headers("core-secret")), None);
    }
}
