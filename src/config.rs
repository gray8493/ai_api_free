use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default)]
    pub gateway_api_key: String,
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: usize,
    #[serde(default)]
    pub providers: Vec<Provider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub label: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}
fn default_strategy() -> String {
    "round_robin".to_string()
}
fn default_cooldown() -> u64 {
    60
}
fn default_max_attempts() -> usize {
    12
}
fn default_true() -> bool {
    true
}

pub const STRATEGIES: [&str; 3] = ["round_robin", "random", "sequential"];
pub const KINDS: [&str; 2] = ["gemini", "openai"];

impl Config {
    pub fn default_config() -> Self {
        Self {
            listen: default_listen(),
            gateway_api_key: String::new(),
            strategy: default_strategy(),
            cooldown_secs: default_cooldown(),
            max_attempts: default_max_attempts(),
            providers: vec![
                Provider {
                    id: "gemini".into(),
                    kind: "gemini".into(),
                    label: "Google Gemini".into(),
                    enabled: true,
                    base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                    model: "gemini-3.1-flash-lite".into(),
                    extra_headers: BTreeMap::new(),
                    accounts: Vec::new(),
                },
                Provider {
                    id: "openrouter".into(),
                    kind: "openai".into(),
                    label: "OpenRouter".into(),
                    enabled: true,
                    base_url: "https://openrouter.ai/api/v1".into(),
                    model: "openrouter/free".into(),
                    extra_headers: BTreeMap::new(),
                    accounts: Vec::new(),
                },
                Provider {
                    id: "nvidia".into(),
                    kind: "openai".into(),
                    label: "NVIDIA NIM".into(),
                    enabled: true,
                    base_url: "https://integrate.api.nvidia.com/v1".into(),
                    model: "google/gemma-4-31b-it".into(),
                    extra_headers: BTreeMap::new(),
                    accounts: Vec::new(),
                },
                Provider {
                    id: "groq".into(),
                    kind: "openai".into(),
                    label: "Groq".into(),
                    enabled: true,
                    base_url: "https://api.groq.com/openai/v1".into(),
                    model: "qwen/qwen3.8-27b".into(),
                    extra_headers: BTreeMap::new(),
                    accounts: Vec::new(),
                },
                Provider {
                    id: "cloudflare".into(),
                    kind: "openai".into(),
                    label: "Cloudflare Workers AI".into(),
                    enabled: true,
                    base_url: "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1".into(),
                    model: "@cf/meta/llama-3.3-70b-instruct-fp8-fast".into(),
                    extra_headers: BTreeMap::new(),
                    accounts: Vec::new(),
                },
            ],
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if path.exists() {
            let raw = std::fs::read_to_string(path)?;
            let cfg: Config = serde_json::from_str(&raw)?;
            Ok(cfg.sanitized())
        } else {
            let cfg = Config::default_config();
            cfg.save(path)?;
            Ok(cfg)
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn sanitized(mut self) -> Self {
        if self.listen.trim().is_empty() {
            self.listen = default_listen();
        }
        if !STRATEGIES.contains(&self.strategy.as_str()) {
            self.strategy = default_strategy();
        }
        self.cooldown_secs = self.cooldown_secs.clamp(5, 3600);
        self.max_attempts = self.max_attempts.clamp(1, 32);
        if self.providers.is_empty() {
            self.providers = Config::default_config().providers;
        }
        self
    }

    pub fn validate(&mut self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            if p.id.trim().is_empty() {
                return Err("provider id không được rỗng".into());
            }
            if p.id.contains('/') || p.id.contains(':') || p.id.contains('{') {
                return Err(format!("provider id '{}' chứa ký tự không hợp lệ", p.id));
            }
            if !seen.insert(p.id.clone()) {
                return Err(format!("provider id '{}' bị trùng", p.id));
            }
            if !KINDS.contains(&p.kind.as_str()) {
                return Err(format!("provider '{}' có kind không hợp lệ", p.id));
            }
            let mut names = std::collections::HashSet::new();
            for a in &p.accounts {
                if a.name.trim().is_empty() {
                    return Err(format!("provider '{}' có account chưa đặt tên", p.id));
                }
                if !names.insert(a.name.clone()) {
                    return Err(format!("provider '{}' có account trùng tên '{}'", p.id, a.name));
                }
            }
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn has_auth(&self) -> bool {
        !self.gateway_api_key.trim().is_empty()
    }
}
