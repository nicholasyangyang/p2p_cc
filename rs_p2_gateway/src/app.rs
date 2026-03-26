use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::broker::BrokerSender as BrokerSenderTrait;
use crate::config::Config;
use crate::error::AppError;
use crate::keys::KeyStore;
use crate::relay::RelayPool;

// ─── Message types ────────────────────────────────────────────────────────────

/// Inbound messages from the Broker.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerMessage {
    #[serde(rename = "request_key")]
    RequestKey { cwd: String },
    #[serde(rename = "register")]
    Register { npub: String },
    #[serde(rename = "send_dm")]
    SendDm {
        from_npub: String,
        to_npub: String,
        content: String,
    },
}

/// Outbound: key assignment response.
#[derive(Serialize)]
struct KeyAssigned {
    #[serde(rename = "type")]
    msg_type: String,
    cwd: String,
    npub: String,
    nsec: String,
}

/// Outbound: DM send result.
#[derive(Serialize)]
struct DmSent {
    #[serde(rename = "type")]
    msg_type: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ─── BrokerSender ────────────────────────────────────────────────────────────

/// Sends messages to the broker over the active WebSocket connection.
#[derive(Clone)]
pub struct BrokerSender {
    tx: broadcast::Sender<String>,
}

impl BrokerSender {
    /// Create the broker sender and its channel.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(100);
        Self { tx }
    }

    /// Send a message to the broker. Silently drops if no broker is connected.
    pub async fn send(&self, msg: &str) -> Result<(), AppError> {
        self.tx
            .send(msg.to_string())
            .map_err(|e| AppError::Broker(format!("broker send failed: {e}")))?;
        Ok(())
    }

    /// Subscribe a new receiver for a new broker WebSocket connection.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }
}

impl Default for BrokerSender {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BrokerSenderTrait for BrokerSender {
    async fn send(&self, msg: &str) -> Result<(), AppError> {
        self.tx
            .send(msg.to_string())
            .map_err(|e| AppError::Broker(format!("broker send failed: {e}")))?;
        Ok(())
    }
}

// ─── AppState ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub relay_pool: Arc<RelayPool>,
    pub key_store: Arc<KeyStore>,
    pub broker: BrokerSender,
}

// ─── HTTP server ─────────────────────────────────────────────────────────────

pub async fn run(cfg: Config) -> Result<()> {
    let key_store = Arc::new(
        KeyStore::new(cfg.keys_file.clone().into())
            .context("failed to open key store")?,
    );
    info!("Key store initialized");

    let broker_sender = BrokerSender::new();

    let relay_pool = Arc::new(RelayPool::new(
        key_store.clone(),
        Arc::new(broker_sender.clone()),
        cfg.relays.clone(),
    ));

    relay_pool.start().await.context("failed to start relay pool")?;
    info!(
        "Relay pool started with {} client(s)",
        key_store.len().await
    );

    let state = Arc::new(AppState {
        relay_pool,
        key_store,
        broker: broker_sender.clone(),
    });

    let app = Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(health_handler))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = TcpListener::bind(&addr).await.context("failed to bind port")?;
    info!("Gateway listening on {}", addr);

    axum::serve(listener, app)
        .await
        .context("server error")?;

    Ok(())
}

async fn health_handler() -> &'static str {
    "OK"
}

// ─── WebSocket handler ────────────────────────────────────────────────────────

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> axum::response::Response {
    // Subscribe a fresh receiver for this broker connection.
    // This gives the new connection all messages sent after it connected.
    let broker_rx = state.broker.subscribe();
    ws.on_upgrade(|socket| handle_socket(socket, state, broker_rx))
}

/// Receives inbound messages from the broker and dispatches them.
/// Also drains the broker channel and forwards messages to the broker WebSocket.
async fn handle_socket(
    socket: WebSocket,
    state: Arc<AppState>,
    mut broker_rx: broadcast::Receiver<String>,
) {
    let (mut ws_sink, mut ws_recv) = socket.split();

    info!("Broker connected");

    // Forward messages from the broker channel to the WebSocket.
    tokio::spawn(async move {
        while let Ok(msg) = broker_rx.recv().await {
            if ws_sink.send(Message::Text(msg.into())).await.is_err() {
                info!("Broker WS closed");
                break;
            }
        }
    });

    // Receive and dispatch inbound messages from the broker.
    while let Some(msg) = ws_recv.next().await {
        let msg = match msg {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => {
                info!("Broker disconnected");
                break;
            }
            Err(e) => {
                warn!("WebSocket error: {}", e);
                break;
            }
            _ => continue,
        };

        let state_clone = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_broker_text(&msg, &state_clone).await {
                warn!("handle_broker_text error: {}", e);
            }
        });
    }
}

async fn handle_broker_text(text: &str, state: &Arc<AppState>) -> Result<(), AppError> {
    let msg: BrokerMessage = serde_json::from_str(text)
        .map_err(|e| AppError::Json(format!("failed to parse broker message: {e}")))?;

    match msg {
        BrokerMessage::RequestKey { cwd } => {
            info!("Broker request_key for cwd={}", cwd);
            let key = state
                .key_store
                .get_or_create_key(&cwd)
                .await
                .map_err(|e| AppError::Keys(e.to_string()))?;
            let resp = KeyAssigned {
                msg_type: "key_assigned".to_string(),
                cwd,
                npub: key.npub,
                nsec: key.nsec,
            };
            state.broker.send(&serde_json::to_string(&resp)?).await?;
        }
        BrokerMessage::Register { npub } => {
            info!("Broker register for npub={}", &npub[..16.min(npub.len())]);
            if let Some(nsec) = state.key_store.get_nsec_by_npub(&npub).await {
                if let Err(e) = state.relay_pool.add_key(&npub, &nsec).await {
                    warn!("Failed to add key on register: {}", e);
                }
            } else {
                warn!("register: npub {} not found in key store", &npub[..16.min(npub.len())]);
            }
        }
        BrokerMessage::SendDm {
            from_npub,
            to_npub,
            content,
        } => {
            info!(
                "Broker send_dm from {} to {}",
                &from_npub[..16.min(from_npub.len())],
                &to_npub[..16.min(to_npub.len())]
            );
            match state.relay_pool.send_dm(&from_npub, &to_npub, &content).await {
                Ok(()) => {
                    let resp = DmSent {
                        msg_type: "dm_sent".to_string(),
                        ok: true,
                        error: None,
                    };
                    state.broker.send(&serde_json::to_string(&resp)?).await?;
                }
                Err(e) => {
                    warn!("send_dm failed: {}", e);
                    let resp = DmSent {
                        msg_type: "dm_sent".to_string(),
                        ok: false,
                        error: Some(e.to_string()),
                    };
                    state.broker.send(&serde_json::to_string(&resp)?).await?;
                }
            }
        }
    }
    Ok(())
}
