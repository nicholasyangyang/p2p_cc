/// Basic smoke test for nostr-sdk types used in relay.rs.
/// Real E2E gift wrap tests require a live relay.
#[tokio::test]
async fn test_nostr_sdk_gift_wrap_types_exist() {
    use nostr_sdk::{Client, Keys, Kind};

    // Verify nostr-sdk types compile correctly
    let keys = Keys::generate();
    let pk = keys.public_key();
    assert!(!pk.to_string().is_empty());
    // Kind implements From<u16>, not From<u64>
    assert_eq!(Kind::GiftWrap, Kind::from(1059u16));

    // send_private_msg API surface (won't actually send without relays)
    let _client = Client::builder()
        .signer(keys)
        .build();
}

#[tokio::test]
async fn test_subscription_id() {
    use nostr_sdk::SubscriptionId;
    let sid = SubscriptionId::new("test-sub");
    assert_eq!(sid.to_string(), "test-sub");
}
