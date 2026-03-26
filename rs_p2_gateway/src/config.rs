use std::env;

const DEFAULT_PORT: u16 = 7899;
const DEFAULT_KEYS_FILE: &str = "all_key.json";
const DEFAULT_BROKER_URL: &str = "ws://127.0.0.1:8080";
const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://relay.0xchat.com",
    "wss://nostr.oxtr.dev",
    "wss://nostr-pub.wellorder.net",
    "wss://relay.primal.net",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub broker_url: String,
    pub keys_file: String,
    pub relays: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            broker_url: DEFAULT_BROKER_URL.to_string(),
            keys_file: DEFAULT_KEYS_FILE.to_string(),
            relays: DEFAULT_RELAYS.iter().map(|s| (*s).to_string()).collect(),
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            port: env::var("WS_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PORT),
            broker_url: env::var("WS_BROKER_URL")
                .unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string()),
            keys_file: env::var("ALL_KEYS_FILE")
                .unwrap_or_else(|_| DEFAULT_KEYS_FILE.to_string()),
            relays: (env::var("RELAYS")
                .map(|v| v.split(',').map(String::from).collect::<Vec<String>>())
                .unwrap_or_else(|_| DEFAULT_RELAYS.iter().map(|s| (*s).to_string()).collect())),
        }
    }
}
