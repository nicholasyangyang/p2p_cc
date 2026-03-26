// src/transport.rs
use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_wsocket::futures_util::stream::SplitSink;
use async_wsocket::futures_util::{Sink, StreamExt, TryStreamExt};
use async_wsocket::{ConnectionMode, Message, WebSocket};
use nostr_sdk::nostr::util::BoxedFuture;
use nostr_sdk::nostr::Url;
use nostr_relay_pool::transport::error::TransportError;
use nostr_relay_pool::transport::websocket::{WebSocketSink, WebSocketStream, WebSocketTransport};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

const USER_AGENT: &str = concat!("rs_tg_nostr/", env!("CARGO_PKG_VERSION"));

// Newtype wrapper around SplitSink — do NOT replace with sink_map_err,
// as that can cause panics (see rust-nostr issue #984).
struct OurSink(SplitSink<WebSocket, Message>);

impl fmt::Debug for OurSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OurSink").finish()
    }
}

impl Sink<Message> for OurSink {
    type Error = TransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_ready(cx)
            .map_err(TransportError::backend)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        Pin::new(&mut self.0)
            .start_send(item)
            .map_err(TransportError::backend)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_flush(cx)
            .map_err(TransportError::backend)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.0)
            .poll_close(cx)
            .map_err(TransportError::backend)
    }
}

/// WebSocket transport that injects a `User-Agent` header into the handshake
/// request, fixing HTTP 403 on relays that require it (e.g. relay.0xchat.com).
///
/// Only supports `ConnectionMode::Direct`.
#[derive(Debug, Clone, Default)]
pub struct UserAgentTransport;

impl WebSocketTransport for UserAgentTransport {
    fn support_ping(&self) -> bool {
        true
    }

    fn connect<'a>(
        &'a self,
        url: &'a Url,
        mode: &'a ConnectionMode,
        _timeout: Duration,
    ) -> BoxedFuture<'a, Result<(WebSocketSink, WebSocketStream), TransportError>> {
        Box::pin(async move {
            if !matches!(mode, ConnectionMode::Direct) {
                return Err(TransportError::backend(io::Error::new(
                    io::ErrorKind::Other,
                    "UserAgentTransport only supports Direct mode",
                )));
            }

            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(TransportError::backend)?;
            request.headers_mut().insert(
                "User-Agent",
                HeaderValue::from_static(USER_AGENT),
            );

            let (ws_stream, _response) =
                tokio_tungstenite::connect_async_tls_with_config(request, None, false, None)
                    .await
                    .map_err(TransportError::backend)?;

            // Wrap as async-wsocket WebSocket so Message/Sink/Stream types align
            let socket = WebSocket::Tokio(ws_stream);
            let (tx, rx) = socket.split();

            let sink: WebSocketSink = Box::new(OurSink(tx));
            let stream: WebSocketStream = Box::pin(rx.map_err(TransportError::backend));

            Ok((sink, stream))
        })
    }
}
