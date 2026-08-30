use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const NAME_CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789._-";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub default_profile: String,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub profiles: BTreeMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    pub provider: String,
    pub protocol: String,
    pub model: String,
    pub max_output_tokens: Option<u64>,
    pub reasoning_effort: Option<String>,
    pub request_timeout_ms: Option<u64>,
    pub idle_timeout_ms: Option<u64>,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedProfile {
    pub name: String,
    pub protocol: String,
    #[serde(rename = "baseUrl")]
    pub base_url: String,
    #[serde(rename = "apiKeyEnv", skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    pub model: String,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(rename = "reasoningEffort", skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(rename = "requestTimeoutMs", skip_serializing_if = "Option::is_none")]
    pub request_timeout_ms: Option<u64>,
    #[serde(rename = "idleTimeoutMs", skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
    #[serde(rename = "contextWindow", skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut config: Self = toml::from_str(text).map_err(|e| format!("invalid config: {e}"))?;
        for provider in config.providers.values_mut() {
            provider.base_url = provider.base_url.trim_end_matches('/').to_string();
        }
        config.validate()?;
        Ok(config)
    }

    pub fn from_path(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("version must equal 1 (got {})", self.version));
        }
        validate_name("default_profile", &self.default_profile)?;
        if !self.profiles.contains_key(&self.default_profile) {
            return Err(format!(
                "default_profile references missing profile {:?}",
                self.default_profile
            ));
        }
        for (name, provider) in &self.providers {
            validate_name("provider", name)?;
            validate_base_url(&format!("providers.{name}.base_url"), &provider.base_url)?;
            if let Some(env) = &provider.api_key_env {
                if env.is_empty() || env.contains('=') || env.chars().any(|c| c.is_whitespace()) {
                    return Err(format!(
                        "providers.{name}.api_key_env must be a valid environment variable name"
                    ));
                }
            }
        }
        for (name, profile) in &self.profiles {
            validate_name("profile", name)?;
            if !self.providers.contains_key(&profile.provider) {
                return Err(format!(
                    "profiles.{name}.provider references missing provider {:?}",
                    profile.provider
                ));
            }
            if !crate::llm::protocol_supported(&profile.protocol) {
                return Err(format!(
                    "profiles.{name}.protocol is unsupported: {:?}",
                    profile.protocol
                ));
            }
            if profile.model.is_empty() {
                return Err(format!("profiles.{name}.model must not be empty"));
            }
            if profile.max_output_tokens == Some(0) {
                return Err(format!(
                    "profiles.{name}.max_output_tokens must be positive"
                ));
            }
            if let Some(effort) = &profile.reasoning_effort {
                if !matches!(effort.as_str(), "low" | "medium" | "high") {
                    return Err(format!(
                        "profiles.{name}.reasoning_effort must be low, medium, or high"
                    ));
                }
            }
            for (field, value) in [
                ("request_timeout_ms", profile.request_timeout_ms),
                ("idle_timeout_ms", profile.idle_timeout_ms),
                ("context_window", profile.context_window),
            ] {
                if value == Some(0) {
                    return Err(format!("profiles.{name}.{field} must be positive"));
                }
            }
        }
        Ok(())
    }

    pub fn resolve(&self, name: &str) -> Result<ResolvedProfile, String> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            let names = self.profiles.keys().cloned().collect::<Vec<_>>().join(", ");
            format!("profile {name:?} not found; valid profiles: {names}")
        })?;
        let provider = self
            .providers
            .get(&profile.provider)
            .expect("validated provider");
        Ok(ResolvedProfile {
            name: name.to_string(),
            protocol: profile.protocol.clone(),
            base_url: provider.base_url.clone(),
            api_key_env: provider.api_key_env.clone(),
            model: profile.model.clone(),
            max_output_tokens: profile.max_output_tokens,
            reasoning_effort: profile.reasoning_effort.clone(),
            request_timeout_ms: profile.request_timeout_ms,
            idle_timeout_ms: profile.idle_timeout_ms,
            context_window: profile.context_window,
        })
    }
}

fn validate_name(kind: &str, name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(format!("{kind} name must not be empty"));
    };
    if !first.is_ascii_alphanumeric() || !chars.all(|c| NAME_CHARS.contains(c)) {
        return Err(format!(
            "invalid {kind} name {name:?}; expected [A-Za-z0-9][A-Za-z0-9._-]*"
        ));
    }
    Ok(())
}

fn validate_base_url(field: &str, value: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(value).map_err(|e| format!("{field} is invalid: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("{field} must use HTTP or HTTPS"));
    }
    if parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(format!(
            "{field} must not contain credentials, query, or fragment"
        ));
    }
    Ok(())
}

pub fn config_path(explicit: Option<&Path>) -> Result<Option<PathBuf>, String> {
    if let Some(path) = explicit {
        return Ok(Some(path.to_path_buf()));
    }
    let base = if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(value)
    } else {
        dirs_fallback("config")?
    };
    let path = base.join("terrarium/config.toml");
    Ok(path.is_file().then_some(path))
}

fn dirs_fallback(kind: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| format!("cannot determine user {kind} directory"))?;
    Ok(PathBuf::from(home).join(match kind {
        "state" => ".local/state",
        _ => ".config",
    }))
}

pub fn load(explicit: Option<&Path>) -> Result<Config, String> {
    if let Some(path) = config_path(explicit)? {
        return Config::from_path(&path);
    }
    let legacy_present = [
        "TERRARIUM_LLM_API_KEY",
        "TERRARIUM_LLM_BASE_URL",
        "TERRARIUM_LLM_MODEL",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some());
    if !legacy_present {
        return Err(
            "no Terrarium config found; create config.toml or set legacy TERRARIUM_LLM_* variables"
                .into(),
        );
    }
    let endpoint = std::env::var("TERRARIUM_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".into());
    let base_url = endpoint
        .strip_suffix("/chat/completions")
        .unwrap_or(&endpoint)
        .trim_end_matches('/')
        .to_string();
    let model = std::env::var("TERRARIUM_LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());
    let key_env =
        std::env::var_os("TERRARIUM_LLM_API_KEY").map(|_| "TERRARIUM_LLM_API_KEY".to_string());
    let mut providers = BTreeMap::new();
    providers.insert(
        "default".into(),
        ProviderConfig {
            base_url,
            api_key_env: key_env,
        },
    );
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "default".into(),
        ProfileConfig {
            provider: "default".into(),
            protocol: "openai-chat-completions".into(),
            model,
            max_output_tokens: None,
            reasoning_effort: None,
            request_timeout_ms: None,
            idle_timeout_ms: None,
            context_window: None,
        },
    );
    let config = Config {
        version: 1,
        default_profile: "default".into(),
        providers,
        profiles,
    };
    config.validate()?;
    Ok(config)
}

pub fn state_dir() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(value).join("terrarium/sessions"));
    }
    Ok(dirs_fallback("state")?.join("terrarium/sessions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_profile_resolution() {
        let c = Config::parse(
            r#"
version = 1
default_profile = "main"
[providers.p]
base_url = "https://example.test/v1"
api_key_env = "KEY"
[profiles.main]
provider = "p"
protocol = "openai-chat-completions"
model = "vendor/model"
reasoning_effort = "high"
"#,
        )
        .unwrap();
        assert_eq!(
            c.resolve("main").unwrap().base_url,
            "https://example.test/v1"
        );
        assert!(Config::parse("version=1\ndefault_profile=\"x\"\n[providers.p]\nbase_url=\"https://x.test\"\n[profiles.x]\nprovider=\"p\"\nprotocol=\"openai-chat-completions\"\nmodel=\"m\"\nunknown=1").is_err());
    }

    #[test]
    fn accepts_all_three_protocols_and_transport_knobs() {
        for protocol in ["openai-responses", "anthropic-messages"] {
            let c = Config::parse(&format!(
                r#"
version = 1
default_profile = "main"
[providers.p]
base_url = "https://example.test"
[profiles.main]
provider = "p"
protocol = "{protocol}"
model = "m"
request_timeout_ms = 60000
idle_timeout_ms = 5000
context_window = 131072
"#
            ))
            .unwrap();
            let resolved = c.resolve("main").unwrap();
            assert_eq!(resolved.protocol, protocol);
            assert_eq!(resolved.request_timeout_ms, Some(60_000));
            assert_eq!(resolved.idle_timeout_ms, Some(5_000));
            assert_eq!(resolved.context_window, Some(131_072));
        }
        assert!(Config::parse(
            "version=1\ndefault_profile=\"x\"\n[providers.p]\nbase_url=\"https://x.test\"\n[profiles.x]\nprovider=\"p\"\nprotocol=\"grpc\"\nmodel=\"m\""
        )
        .is_err());
        assert!(Config::parse(
            "version=1\ndefault_profile=\"x\"\n[providers.p]\nbase_url=\"https://x.test\"\n[profiles.x]\nprovider=\"p\"\nprotocol=\"openai-responses\"\nmodel=\"m\"\nrequest_timeout_ms=0"
        )
        .is_err());
    }
}
