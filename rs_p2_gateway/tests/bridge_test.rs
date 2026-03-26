use async_trait::async_trait;
use rs_p2_gateway::broker::BrokerSender;
use rs_p2_gateway::error::AppError;
use std::sync::{Arc, Mutex};

// In-memory mock that records sent messages
struct MockBroker {
    received: Mutex<Vec<String>>,
}

impl MockBroker {
    fn new() -> Self {
        Self { received: Mutex::new(Vec::new()) }
    }
}

#[async_trait]
impl BrokerSender for MockBroker {
    async fn send(&self, msg: &str) -> Result<(), AppError> {
        self.received.lock().unwrap().push(msg.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn test_mock_broker_records_messages() {
    let mock = Arc::new(MockBroker::new());
    mock.send(r#"{"type":"dm_received","content":"hello"}"#).await.unwrap();
    mock.send(r#"{"type":"dm_sent","ok":true}"#).await.unwrap();

    let received = mock.received.lock().unwrap();
    assert_eq!(received.len(), 2);
    assert!(received[0].contains("dm_received"));
    assert!(received[1].contains("dm_sent"));
}

#[tokio::test]
async fn test_mock_broker_concurrent_sends() {
    let mock = Arc::new(MockBroker::new());
    let mut handles = vec![];
    for i in 0..10 {
        let m = mock.clone();
        handles.push(tokio::spawn(async move {
            m.send(&format!(r#"{{"msg":{}}}"#, i)).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert_eq!(mock.received.lock().unwrap().len(), 10);
}
