use async_trait::async_trait;

use crate::error::AppError;

/// Sends messages to the Broker over the established WebSocket connection.
/// Testable via mock.
#[async_trait]
pub trait BrokerSender: Send + Sync {
    async fn send(&self, msg: &str) -> Result<(), AppError>;
}

/// In-memory mock for testing.
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    pub struct MockBroker {
        pub messages: StdMutex<Vec<String>>,
    }

    impl MockBroker {
        pub fn new() -> Self {
            Self { messages: StdMutex::new(Vec::new()) }
        }
    }

    #[async_trait]
    impl BrokerSender for MockBroker {
        async fn send(&self, msg: &str) -> Result<(), AppError> {
            self.messages.lock().unwrap().push(msg.to_string());
            Ok(())
        }
    }
}
