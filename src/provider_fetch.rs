//! Fetching model lists from providers.
//!
//! Kept separate from the registry storage so the network shape of each provider
//! can be tested without a database or a server. Every fetch is a plain blocking
//! HTTP GET driven from a `tokio::task::spawn_blocking` by the API layer — the
//! request volume here is one call per dropdown refresh, not a hot path.

use crate::providers::{Provider, ProviderKind};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// How long to wait on a provider before calling it unreachable.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

/// Fetch and normalize the model ids a provider currently serves.
pub fn fetch_models(p: &Provider, key: Option<&str>) -> Result<Vec<String>> {
    let kind = ProviderKind::parse(&p.kind)?;
    if kind == ProviderKind::Manual {
        // Nothing to ask: the ids are whatever the cache already holds.
        return Ok(p.models.clone());
    }
    if kind.needs_key() && key.is_none() {
        return Err(anyhow!(
            "no API key set for '{}' — add one in Settings to list its models",
            p.label_or_slug()
        ));
    }
    let base = p.base_url.trim_end_matches('/');
    if base.is_empty() {
        return Err(anyhow!("provider '{}' has no base URL", p.label_or_slug()));
    }

    let (url, headers) = match kind {
        ProviderKind::OpenaiCompatible => (
            format!("{base}/models"),
            vec![("Authorization".to_string(), format!("Bearer {}", key.unwrap_or_default()))],
        ),
        ProviderKind::Anthropic => (
            format!("{base}/v1/models"),
            vec![
                ("x-api-key".to_string(), key.unwrap_or_default().to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ],
        ),
        ProviderKind::Ollama => (format!("{base}/api/tags"), vec![]),
        ProviderKind::Manual => unreachable!("handled above"),
    };

    let agent = build_agent()?;
    // The timeout lives on the agent config: ureq 3 has no per-request timeout.
    let mut req = agent.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req
        .call()
        .with_context(|| format!("could not reach {}", p.label_or_slug()))?;
    let status = resp.status();
    let body = resp
        .into_body()
        .read_to_string()
        .context("reading the provider's model list")?;
    if !status.is_success() {
        // Never echo the body verbatim: some providers repeat the key in an auth
        // error, and this string reaches the board.
        return Err(anyhow!(
            "{} answered {} — check the key and base URL",
            p.label_or_slug(),
            status.as_u16()
        ));
    }

    let mut ids = match kind {
        ProviderKind::Ollama => parse_ollama(&body)?,
        _ => parse_openai_like(&body)?,
    };
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(anyhow!(
            "{} returned an empty model list",
            p.label_or_slug()
        ));
    }
    Ok(ids)
}

/// Minimal client: this only ever does a GET with two headers, so a full client
/// feature set would be dead weight.
fn build_agent() -> Result<ureq::Agent> {
    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into())
}

#[derive(Deserialize)]
struct OllamaResponse {
    /// Required for the same reason as `OpenAiLike::data`.
    models: Vec<OllamaModel>,
}

#[derive(Deserialize)]
struct OllamaModel {
    #[serde(default)]
    name: String,
}

fn parse_ollama(body: &str) -> Result<Vec<String>> {
    let parsed: OllamaResponse =
        serde_json::from_str(body).context("ollama did not return the expected /api/tags shape")?;
    Ok(parsed
        .models
        .into_iter()
        .map(|m| m.name)
        .filter(|n| !n.is_empty())
        .collect())
}

#[derive(Deserialize)]
struct OpenAiLike {
    /// Required, not defaulted: a body with no `data` is a different endpoint's
    /// answer, and treating it as "no models" hides a misconfigured base URL.
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    #[serde(default)]
    id: String,
}

fn parse_openai_like(body: &str) -> Result<Vec<String>> {
    let parsed: OpenAiLike =
        serde_json::from_str(body).context("provider did not return a {data:[{id}]} model list")?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| m.id)
        .filter(|id| !id.is_empty())
        .collect())
}

impl Provider {
    /// What to call this provider in an error message.
    pub fn label_or_slug(&self) -> &str {
        if self.label.is_empty() {
            &self.slug
        } else {
            &self.label
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_shape_is_parsed() {
        let body = r#"{"data":[{"id":"z-ai/glm-5.3-flash"},{"id":"deepseek/deepseek-v4"},{"id":""}]}"#;
        let mut got = parse_openai_like(body).unwrap();
        got.sort();
        assert_eq!(got, vec!["deepseek/deepseek-v4", "z-ai/glm-5.3-flash"]);
    }

    #[test]
    fn ollama_tags_are_parsed_for_the_model_list() {
        // The endpoint serving `{models:[{name}]}` is `GET {base_url}/api/tags`,
        // not the OpenAI `/v1/models` this project fetches for other kinds.
        let body = r#"{"models":[{"name":"llama3:latest"},{"name":"qwen2.5-coder:7b"}]}"#;
        assert_eq!(
            parse_ollama(body).unwrap(),
            vec!["llama3:latest", "qwen2.5-coder:7b"]
        );
    }

    #[test]
    fn a_wrong_shape_is_an_error_not_an_empty_list() {
        assert!(parse_openai_like(r#"{"models":[]}"#).is_err());
        assert!(parse_ollama(r#"{"data":[]}"#).is_err());
    }
}
