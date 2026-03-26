use rand::RngCore;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nostr_sdk::{Client, Filter, Kind, RelayPoolNotification, SubscriptionId, Timestamp, ToBech32};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::broker::BrokerSender;
use crate::error::AppError;
use crate::keys::KeyStore;
use crate::transport::UserAgentTransport;

/// Max entries in the seen-events dedup cache.
const MAX_SEEN: usize = 5000;
/// TTL for dedup cache entries.
const SEEN_TTL: Duration = Duration::from_secs(7200);

/// Manages multiple per-key NostrClients.
pub struct RelayPool {
    clients: RwLock<HashMap<String, Arc<NostrClient>>>,
    key_store: Arc<KeyStore>,
    broker: Arc<dyn BrokerSender>,
    relays: Vec<String>,
}

impl RelayPool {
    pub fn new(
        key_store: Arc<KeyStore>,
        broker: Arc<dyn BrokerSender>,
        relays: Vec<String>,
    ) -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            key_store,
            broker,
            relays,
        }
    }

    /// Start the relay pool: connect all existing keys from key store and begin listening.
    pub async fn start(&self) -> Result<(), AppError> {
        let keys = self.key_store.all_keys().await;
        for pair in keys {
            self.add_key(&pair.npub, &pair.nsec).await?;
        }
        Ok(())
    }

    /// Register a new key and start its NostrClient.
    /// Idempotent — if already registered, does nothing.
    pub async fn add_key(&self, npub: &str, nsec: &str) -> Result<(), AppError> {
        // Fast path: already registered
        if self.clients.read().await.contains_key(npub) {
            return Ok(());
        }

        let client =
            NostrClient::connect(npub, nsec, &self.relays, self.broker.clone()).await?;
        let client = Arc::new(client);

        self.clients
            .write()
            .await
            .insert(npub.to_string(), client.clone());

        // Spawn listener task
        let npub_owned = npub.to_string();
        let client_clone = client.clone();
        tokio::spawn(async move {
            if let Err(e) = client_clone.listen().await {
                warn!("NostrClient for {} listener error: {}", &npub_owned[..16.min(npub_owned.len())], e);
            }
        });

        info!(
            "Added NostrClient for npub={}",
            &npub[..16.min(npub.len())]
        );
        Ok(())
    }

    /// Send a NIP-17 DM using the key identified by from_npub.
    pub async fn send_dm(
        &self,
        from_npub: &str,
        to_npub: &str,
        content: &str,
    ) -> Result<(), AppError> {
        let clients = self.clients.read().await;
        let client = clients
            .get(from_npub)
            .ok_or_else(|| AppError::Nostr(format!("no client for {from_npub}")))?;
        client.send_dm(to_npub, content).await
    }

    /// Get all registered npubs.
    #[allow(dead_code)]
    pub async fn all_npubs(&self) -> Vec<String> {
        self.clients.read().await.keys().cloned().collect()
    }
}

/// A per-key Nostr client that subscribes to kind:1059 Gift Wraps
/// and forwards unwrapped DMs to the broker.
pub struct NostrClient {
    npub: String,
    client: Arc<Client>,
    broker: Arc<dyn BrokerSender>,
    seen: RwLock<HashMap<String, Instant>>,
}

impl NostrClient {
    /// Connect to relays and subscribe to Gift Wraps.
    pub async fn connect(
        npub: &str,
        nsec: &str,
        relays: &[String],
        broker: Arc<dyn BrokerSender>,
    ) -> Result<Self, AppError> {
        let keys = nostr_sdk::Keys::parse(nsec)
            .map_err(|e| AppError::Nostr(format!("parse nsec failed: {e}")))?;
        let my_pubkey = keys.public_key();

        let client = Client::builder()
            .signer(keys)
            .websocket_transport(UserAgentTransport)
            .build();

        for relay in relays {
            client
                .add_relay(relay)
                .await
                .map_err(|e| AppError::Nostr(e.to_string()))?;
        }
        client.connect().await;

        // Subscribe to kind:1059 Gift Wraps addressed to us (#p tag) for the past 48h.
        // NOTE: Gift wraps are intentionally backdated up to 48h, so we use since(now - 48h)
        // to capture both historical and live events in one subscription.
        let since = Timestamp::now() - Duration::from_secs(2 * 24 * 60 * 60);
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(my_pubkey) // #p tag: only events addressed to us
            .since(since);
        let sub_id = {
            let mut bytes = [0u8; 8];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            SubscriptionId::new(hex::encode(bytes))
        };
        client
            .subscribe_with_id(sub_id, filter, None)
            .await
            .map_err(|e| AppError::Nostr(e.to_string()))?;

        info!("NostrClient connected for npub={}", &npub[..16.min(npub.len())]);

        Ok(Self {
            npub: npub.to_string(),
            client: Arc::new(client),
            broker,
            seen: RwLock::new(HashMap::new()),
        })
    }

    /// Listen for Gift Wrap events and forward unwrapped DMs to broker.
    /// Runs until the relay connection is closed.
    pub async fn listen(&self) -> Result<(), AppError> {
        let client = self.client.clone();
        let broker = self.broker.clone();
        let seen_map: HashMap<String, Instant> = self.seen.read().await.clone();
        let seen: Arc<RwLock<HashMap<String, Instant>>> = Arc::new(RwLock::new(seen_map));
        let npub = self.npub.clone();

        // handle_notifications takes ownership of the client — clone so we can also use it in the closure
        let client_for_handler = client.clone();
        client_for_handler
            .handle_notifications(move |notification| {
                let broker = broker.clone();
                let seen = seen.clone();
                let npub = npub.clone();
                let client = client.clone();
                async move {
                    if let RelayPoolNotification::Event { event, .. } = notification {
                        if event.kind != Kind::GiftWrap {
                            return Ok(false);
                        }

                        // Deduplicate
                        let eid = event.id.to_hex();
                        {
                            let mut seen_guard = seen.write().await;
                            let now = Instant::now();
                            if seen_guard.contains_key(&eid) {
                                return Ok(false);
                            }
                            seen_guard.insert(eid.clone(), now);
                            if seen_guard.len() > MAX_SEEN {
                                let cutoff = now - SEEN_TTL;
                                seen_guard.retain(|_, t| *t > cutoff);
                            }
                        }

                        // Try to unwrap
                        match client.clone().unwrap_gift_wrap(&event).await {
                            Ok(gift) => {
                                let content = gift.rumor.content.clone();
                                let from_npub = gift.rumor.pubkey.to_bech32().unwrap_or_default();
                                let msg = serde_json::json!({
                                    "type": "dm_received",
                                    "from_npub": from_npub,
                                    "to_npub": npub,
                                    "content": content,
                                });
                                if let Err(e) = broker.send(&msg.to_string()).await {
                                    warn!("Failed to send dm_received to broker: {}", e);
                                }
                            }
                            Err(_) => {
                                // Not addressed to this recipient — normal in multi-key setup
                            }
                        }
                    }
                    Ok(false)
                }
            })
            .await
            .map_err(|e| AppError::Nostr(e.to_string()))?;

        Ok(())
    }

    /// Send a NIP-17 private DM.
    pub async fn send_dm(&self, to_npub: &str, content: &str) -> Result<(), AppError> {
        let recipient = nostr_sdk::PublicKey::parse(to_npub)
            .map_err(|e| AppError::Nostr(format!("invalid npub: {e}")))?;

        self.client
            .send_private_msg(recipient, content, std::iter::empty())
            .await
            .map_err(|e| AppError::Nostr(e.to_string()))?;

        info!("Sent DM to {}", &to_npub[..16.min(to_npub.len())]);
        Ok(())
    }
}
