use tempfile::TempDir;

fn temp_dir() -> TempDir { tempfile::tempdir().unwrap() }

#[tokio::test]
async fn test_get_or_create_new_key() {
    let dir = temp_dir();
    let path = dir.path().join("keys.json");
    let store = rs_p2_gateway::keys::KeyStore::new(path).unwrap();

    let key = store.get_or_create_key("/tmp/test").await.unwrap();
    assert!(key.npub.starts_with("npub1"), "npub should start with npub1, got: {}", key.npub);
    assert!(key.nsec.starts_with("nsec1"), "nsec should start with nsec1, got: {}", key.nsec);
    assert_eq!(store.len().await, 1);
}

#[tokio::test]
async fn test_get_or_create_existing_key() {
    let dir = temp_dir();
    let path = dir.path().join("keys.json");
    let store = rs_p2_gateway::keys::KeyStore::new(path).unwrap();

    let key1 = store.get_or_create_key("/tmp/test").await.unwrap();
    let key2 = store.get_or_create_key("/tmp/test").await.unwrap();

    assert_eq!(key1.npub, key2.npub);
    assert_eq!(store.len().await, 1);
}

#[tokio::test]
async fn test_get_nsec_by_npub() {
    let dir = temp_dir();
    let path = dir.path().join("keys.json");
    let store = rs_p2_gateway::keys::KeyStore::new(path).unwrap();

    let key = store.get_or_create_key("/tmp/test").await.unwrap();
    let found_nsec = store.get_nsec_by_npub(&key.npub).await;
    assert_eq!(found_nsec, Some(key.nsec));
}

#[tokio::test]
async fn test_get_nsec_by_npub_not_found() {
    let dir = temp_dir();
    let path = dir.path().join("keys.json");
    let store = rs_p2_gateway::keys::KeyStore::new(path).unwrap();

    let result = store.get_nsec_by_npub("npub1xxxx").await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_all_npubs() {
    let dir = temp_dir();
    let path = dir.path().join("keys.json");
    let store = rs_p2_gateway::keys::KeyStore::new(path).unwrap();

    store.get_or_create_key("/tmp/a").await.unwrap();
    store.get_or_create_key("/tmp/b").await.unwrap();

    let npubs = store.all_npubs().await;
    assert_eq!(npubs.len(), 2);
}

#[tokio::test]
async fn test_keys_file_format() {
    let dir = temp_dir();
    let path = dir.path().join("keys.json");
    let store = rs_p2_gateway::keys::KeyStore::new(path.clone()).unwrap();

    store.get_or_create_key("/tmp/test").await.unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["keys"].as_array().unwrap().len(), 1);
    let key = &parsed["keys"][0];
    assert!(key["npub"].as_str().unwrap().starts_with("npub1"));
    assert!(key["nsec"].as_str().unwrap().starts_with("nsec1"));
    assert_eq!(key["cwd"].as_str().unwrap(), "/tmp/test");
    assert!(key["created_at"].as_str().unwrap().contains("T"));
}
