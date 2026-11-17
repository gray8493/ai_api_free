use crate::config::Config;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn stat_key(provider: &str, account: &str) -> String {
    format!("{}|{}", provider, account)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AccountStat {
    pub requests: u64,
    pub success: u64,
    pub errors: u64,
    pub last_used: Option<u64>,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
}

pub struct AppState {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub client: reqwest::Client,
    pub stats: Mutex<HashMap<String, AccountStat>>,
    pub cooldowns: Mutex<HashMap<String, Instant>>,
    pub cursors: Mutex<HashMap<String, usize>>,
    pub global_cursor: AtomicUsize,
    pub started: Instant,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .pool_idle_timeout(Duration::from_secs(90))
            .user_agent("ai-gateway/0.1")
            .build()
            .expect("build http client");
        Self {
            config: RwLock::new(config),
            config_path,
            client,
            stats: Mutex::new(HashMap::new()),
            cooldowns: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            global_cursor: AtomicUsize::new(0),
            started: Instant::now(),
        }
    }

    pub fn record_success(&self, key: &str, status: u16) {
        let mut map = self.stats.lock().unwrap();
        let e = map.entry(key.to_string()).or_default();
        e.requests += 1;
        e.success += 1;
        e.last_used = Some(now_ms());
        e.last_status = Some(status);
        e.last_error = None;
    }

    pub fn record_error(&self, key: &str, status: Option<u16>, message: String) {
        let mut map = self.stats.lock().unwrap();
        let e = map.entry(key.to_string()).or_default();
        e.requests += 1;
        e.errors += 1;
        e.last_used = Some(now_ms());
        e.last_status = status;
        let trimmed: String = message.chars().take(400).collect();
        e.last_error = Some(trimmed);
    }

    pub fn cooldown(&self, key: &str, secs: u64) {
        let until = Instant::now() + Duration::from_secs(secs);
        self.cooldowns.lock().unwrap().insert(key.to_string(), until);
    }

    pub fn cooldown_remaining(&self, key: &str) -> u64 {
        let mut map = self.cooldowns.lock().unwrap();
        match map.get(key) {
            Some(until) => {
                let now = Instant::now();
                if *until > now {
                    until.duration_since(now).as_secs() + 1
                } else {
                    map.remove(key);
                    0
                }
            }
            None => 0,
        }
    }

    pub fn next_cursor(&self, key: &str, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        let mut map = self.cursors.lock().unwrap();
        let c = map.entry(key.to_string()).or_insert(0);
        let v = *c % len;
        *c = c.wrapping_add(1);
        v
    }

    pub fn next_global(&self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        self.global_cursor.fetch_add(1, Ordering::Relaxed) % len
    }

    pub fn status_json(&self, cfg: &Config) -> serde_json::Value {
        let stats = self.stats.lock().unwrap();
        let providers: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .map(|p| {
                let accounts: Vec<serde_json::Value> = p
                    .accounts
                    .iter()
                    .map(|a| {
                        let key = stat_key(&p.id, &a.name);
                        let stat = stats.get(&key).cloned().unwrap_or_default();
                        serde_json::json!({
                            "name": a.name,
                            "enabled": a.enabled,
                            "cooldown_secs": self.cooldown_remaining(&key),
                            "stat": stat,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "id": p.id,
                    "label": p.label,
                    "enabled": p.enabled,
                    "model": p.model,
                    "active_accounts": p.accounts.iter().filter(|a| a.enabled && !a.api_key.trim().is_empty()).count(),
                    "accounts": accounts,
                })
            })
            .collect();

        serde_json::json!({
            "uptime_secs": self.started.elapsed().as_secs(),
            "strategy": cfg.strategy,
            "cooldown_secs": cfg.cooldown_secs,
            "providers": providers,
        })
    }

    pub fn prune_stats(&self, cfg: &Config) {
        let valid: HashSet<String> = cfg
            .providers
            .iter()
            .flat_map(|p| p.accounts.iter().map(move |a| stat_key(&p.id, &a.name)))
            .collect();
        self.stats.lock().unwrap().retain(|k, _| valid.contains(k));
        self.cooldowns.lock().unwrap().retain(|k, _| valid.contains(k));
    }
}
