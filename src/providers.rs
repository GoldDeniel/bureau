//! Provider registry: what the model picker in Settings is fed from.
//!
//! A provider is a place models come from (an OpenAI-compatible endpoint, an
//! Anthropic API, a local Ollama daemon). It owns three things the board used to
//! make the user type by hand: the base URL, the models it currently serves, and
//! the credential to reach it.
//!
//! ## Why this exists
//!
//! Before this module the board offered a hardcoded array of provider names and a
//! free-text model input. Worse, `rotation::provider_of` derives the provider by
//! splitting the model id on `/`, so a hand-typed provider was discarded anyway.
//! The registry makes the provider a real thing: a model entry carries its
//! provider implicitly, and the adapter follows from the provider rather than the
//! user picking three overlapping fields.
//!
//! ## Key handling
//!
//! Keys never leave the database in the clear and never reach the API response.
//! They are encrypted with XChaCha20-Poly1305 under a key-encryption key (KEK)
//! that lives OUTSIDE the database (`BUREAU_KEK`, or `kek` in the state
//! directory). That split matters: a stolen `bureau.db` alone yields ciphertext
//! only. If no KEK is available we refuse to store a key rather than fall back to
//! plaintext — a silent downgrade here would be the whole feature's defeat.
//!
//! Every response reports `key_state` (`none` / `missing` / `set`) and never the
//! key, not even a masked prefix: length and first/last characters are enough to
//! narrow a guess.

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// How to talk to a provider's model-listing endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// `GET {base}/models` with `Authorization: Bearer <key>`. Covers OpenAI,
    /// OpenRouter, NVIDIA, and the opencode gateway.
    OpenaiCompatible,
    /// Anthropic's API: `GET {base}/v1/models` with `x-api-key` and
    /// `anthropic-version`. Its list shape is `{data:[{id}]}`, same as OpenAI's,
    /// but the auth header differs.
    Anthropic,
    /// Ollama: `GET {base}/api/tags` → `{models:[{name}]}`. No key.
    Ollama,
    /// Nothing to fetch: the model ids are supplied by hand (shell commands,
    /// custom local builds).
    Manual,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::OpenaiCompatible => "openai_compatible",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Ollama => "ollama",
            ProviderKind::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "openai_compatible" => ProviderKind::OpenaiCompatible,
            "anthropic" => ProviderKind::Anthropic,
            "ollama" => ProviderKind::Ollama,
            "manual" => ProviderKind::Manual,
            other => return Err(anyhow!("unknown provider kind: {other}")),
        })
    }

    /// Whether reaching this provider needs a credential.
    pub fn needs_key(self) -> bool {
        matches!(
            self,
            ProviderKind::OpenaiCompatible | ProviderKind::Anthropic
        )
    }
}

/// A provider as the API and board see it. Never carries the key.
#[derive(Debug, Clone, Serialize)]
pub struct Provider {
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    /// The adapter that dispatches this provider's models (`opencode`, `ollama`,
    /// `hermes`). Inferred, not a third thing for the user to choose.
    pub adapter: String,
    /// `none` (no key needed), `missing` (needs one, none set), `set`.
    pub key_state: String,
    /// Display label overriding the slug, when the user set one.
    pub label: String,
    /// `model_id -> display name`, as JSON.
    pub model_labels: String,
    pub models: Vec<String>,
    pub models_fetched_at: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

/// What an update request carries. The slug is in the path, so it must not be
/// required in the body — a PATCH that only changes a base URL should not have
/// to echo back the key it is not touching.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderUpdate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub adapter: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Present only when replacing it.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// What a create/update request carries.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderInput {
    pub slug: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub adapter: String,
    #[serde(default)]
    pub label: String,
    /// Present only when the user is setting/replacing it.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Default base URL per kind, so adding a provider is one field of typing.
pub fn default_base_url(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenaiCompatible => "https://openrouter.ai/api/v1",
        ProviderKind::Anthropic => "https://api.anthropic.com",
        ProviderKind::Ollama => "http://127.0.0.1:11434",
        ProviderKind::Manual => "",
    }
}

/// Adapter that dispatches a provider's models.
pub fn default_adapter(kind: ProviderKind, slug: &str) -> &'static str {
    match kind {
        ProviderKind::Ollama => "ollama",
        ProviderKind::Manual => "shell",
        _ if slug.starts_with("opencode") => "opencode",
        _ => "opencode",
    }
}

// ------------------------------------------------------------------ encryption

/// Crate name kept here so the dependency is greppable from the module that
/// needs the guarantee.
const _CIPHER: &str = "chacha20poly1305";

#[cfg(feature = "keys")]
mod cipher {
    use super::*;
    use chacha20poly1305::{
        XChaCha20Poly1305, XNonce,
        aead::{Aead, KeyInit, OsRng, rand_core::RngCore},
    };

    /// 32-byte KEK, hex in `BUREAU_KEK`, or read from `<state>/kek`.
    pub fn kek() -> Result<Option<[u8; 32]>> {
        if let Ok(hex) = std::env::var("BUREAU_KEK") {
            let bytes = decode_hex(hex.trim())
                .ok_or_else(|| anyhow!("BUREAU_KEK must be 64 hex characters"))?;
            if bytes.len() != 32 {
                return Err(anyhow!("BUREAU_KEK must be 32 bytes (64 hex chars)"));
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            return Ok(Some(out));
        }
        Ok(None)
    }

    fn decode_hex(s: &str) -> Option<Vec<u8>> {
        if s.len() % 2 != 0 {
            return None;
        }
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok())
            .collect()
    }

    /// Returns `(nonce, ciphertext)`.
    pub fn encrypt(key: &[u8; 32], plaintext: &str) -> Result<(Vec<u8>, Vec<u8>)> {
        let cipher = XChaCha20Poly1305::new(key.into());
        let mut nonce = [0u8; 24];
        OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| anyhow!("encrypting the provider key failed"))?;
        Ok((nonce.to_vec(), ct))
    }

    pub fn decrypt(key: &[u8; 32], nonce: &[u8], ct: &[u8]) -> Result<String> {
        if nonce.len() != 24 {
            return Err(anyhow!("stored nonce has the wrong length"));
        }
        let cipher = XChaCha20Poly1305::new(key.into());
        let pt = cipher
            .decrypt(XNonce::from_slice(nonce), ct)
            .map_err(|_| anyhow!("stored provider key could not be decrypted (wrong KEK?)"))?;
        String::from_utf8(pt).context("decrypted key was not UTF-8")
    }
}

#[cfg(feature = "keys")]
pub use cipher::{decrypt, encrypt, kek};

// ---------------------------------------------------------------------- store

fn row_to_provider(r: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
    let kind: String = r.get("kind")?;
    let has_key: bool = r.get::<_, Option<Vec<u8>>>(  "key_ciphertext")?.is_some();
    let needs = ProviderKind::parse(&kind).map(|k| k.needs_key()).unwrap_or(false);
    let key_state = if !needs {
        "none"
    } else if has_key {
        "set"
    } else {
        "missing"
    };
    let models_json: String = r.get("models_cache")?;
    let models: Vec<String> = serde_json::from_str(&models_json).unwrap_or_default();
    Ok(Provider {
        slug: r.get("slug")?,
        name: r.get("name")?,
        kind,
        base_url: r.get("base_url")?,
        adapter: r.get("adapter")?,
        key_state: key_state.to_string(),
        label: r.get("label")?,
        model_labels: r.get("model_labels")?,
        models,
        models_fetched_at: r.get("models_fetched_at")?,
        enabled: r.get::<_, i64>("enabled")? != 0,
        created_at: r.get("created_at")?,
    })
}

const SELECT_COLS: &str = "slug, name, kind, base_url, adapter, label, model_labels, \
                           models_cache, models_fetched_at, key_ciphertext, enabled, created_at";

pub fn list(conn: &Connection) -> Result<Vec<Provider>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM providers ORDER BY name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map([], row_to_provider)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn get(conn: &Connection, slug: &str) -> Result<Option<Provider>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM providers WHERE slug = ?1"
    ))?;
    Ok(stmt
        .query_row([slug], row_to_provider)
        .optional()?)
}

/// Insert a provider. A key is only written when one is supplied AND a KEK
/// exists; otherwise the call fails loudly rather than storing plaintext.
pub fn create(conn: &Connection, input: &ProviderInput) -> Result<Provider> {
    let kind = ProviderKind::parse(&input.kind)?;
    let base = if input.base_url.trim().is_empty() {
        default_base_url(kind).to_string()
    } else {
        input.base_url.trim().to_string()
    };
    if base.is_empty() && kind != ProviderKind::Manual {
        return Err(anyhow!("a base URL is required for a {} provider", input.kind));
    }
    let adapter = if input.adapter.trim().is_empty() {
        default_adapter(kind, &input.slug).to_string()
    } else {
        input.adapter.trim().to_string()
    };

    let (nonce, ct) = match input.api_key.as_deref().map(str::trim) {
        Some(k) if !k.is_empty() => {
            #[cfg(feature = "keys")]
            {
                let kek = kek()?.ok_or_else(|| {
                    anyhow!(
                        "no key-encryption key configured: set BUREAU_KEK (64 hex chars). \
                         Refusing to store an API key unencrypted."
                    )
                })?;
                let (n, c) = encrypt(&kek, k)?;
                (Some(n), Some(c))
            }
            #[cfg(not(feature = "keys"))]
            {
                let _ = k;
                return Err(anyhow!(
                    "this build has no encryption support (feature `keys` is off); \
                     refusing to store an API key"
                ));
            }
        }
        _ => (None, None),
    };

    conn.execute(
        "INSERT INTO providers (slug, name, kind, base_url, adapter, label, model_labels, \
                                key_ciphertext, key_nonce, models_cache, enabled, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, '{}', ?7, ?8, '[]', 1, ?9)",
        params![
            input.slug.trim(),
            input.name.trim(),
            kind.as_str(),
            base,
            adapter,
            input.label.trim(),
            ct,
            nonce,
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ],
    )
    .context("creating the provider")?;
    get(conn, input.slug.trim())?.ok_or_else(|| anyhow!("provider vanished after insert"))
}

/// Replace the stored key. `None` clears it.
pub fn set_key(conn: &Connection, slug: &str, key: Option<&str>) -> Result<()> {
    match key.map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => {
            #[cfg(feature = "keys")]
            {
                let kek = kek()?.ok_or_else(|| {
                    anyhow!("no key-encryption key configured: set BUREAU_KEK (64 hex chars)")
                })?;
                let (n, c) = encrypt(&kek, k)?;
                conn.execute(
                    "UPDATE providers SET key_ciphertext=?1, key_nonce=?2 WHERE slug=?3",
                    params![c, n, slug],
                )?;
            }
            #[cfg(not(feature = "keys"))]
            {
                let _ = k;
                return Err(anyhow!("this build has no encryption support"));
            }
        }
        None => {
            conn.execute(
                "UPDATE providers SET key_ciphertext=NULL, key_nonce=NULL WHERE slug=?1",
                [slug],
            )?;
        }
    }
    Ok(())
}

/// The plaintext key, for the fetch path only. Never serialized.
pub fn plaintext_key(conn: &Connection, slug: &str) -> Result<Option<String>> {
    let row: Option<(Option<Vec<u8>>, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT key_ciphertext, key_nonce FROM providers WHERE slug=?1",
            [slug],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((Some(ct), Some(nonce))) => {
            #[cfg(feature = "keys")]
            {
                let kek = kek()?.ok_or_else(|| anyhow!("no KEK configured"))?;
                Ok(Some(decrypt(&kek, &nonce, &ct)?))
            }
            #[cfg(not(feature = "keys"))]
            {
                let _ = (ct, nonce);
                Err(anyhow!("this build has no encryption support"))
            }
        }
        _ => Ok(None),
    }
}

pub fn store_models(conn: &Connection, slug: &str, models: &[String]) -> Result<()> {
    conn.execute(
        "UPDATE providers SET models_cache=?1, models_fetched_at=?2 WHERE slug=?3",
        params![
            serde_json::to_string(models)?,
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            slug,
        ],
    )?;
    Ok(())
}

/// One rung of an agent's fallback ladder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rung {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub adapter: String,
}

/// Replace an agent's ladder. The provider of each rung is normalized against
/// `rotation::provider_of`, so a picker that saved a mismatched pair cannot make
/// the stored column disagree with what the dispatcher derives.
pub fn set_agent_models(conn: &Connection, agent_id: i64, rungs: &[Rung]) -> Result<()> {
    conn.execute("DELETE FROM agent_models WHERE agent_id = ?1", [agent_id])?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    for (pos, r) in rungs.iter().enumerate() {
        if r.model.trim().is_empty() {
            continue;
        }
        // Trust the model string over a hand-set provider: the dispatcher does.
        let provider = crate::rotation::provider_of(&r.model).to_string();
        conn.execute(
            "INSERT INTO agent_models (agent_id, position, provider, model, adapter, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![agent_id, pos as i64, provider, r.model.trim(), r.adapter.trim(), now],
        )?;
    }
    Ok(())
}

/// Whether any agent ladder rung references this provider slug.
pub fn agents_using(conn: &Connection, slug: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT a.slug FROM agents a \
         JOIN agent_models m ON m.agent_id = a.id WHERE m.provider = ?1",
    )?;
    let rows = stmt.query_map([slug], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn delete(conn: &Connection, slug: &str) -> Result<()> {
    let users = agents_using(conn, slug)?;
    if !users.is_empty() {
        return Err(anyhow!(
            "provider '{}' is used by agent ladder(s): {}. Re-point them first.",
            slug,
            users.join(", ")
        ));
    }
    conn.execute("DELETE FROM providers WHERE slug=?1", [slug])?;
    Ok(())
}
