// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! S3 integration tests - validate the synchronous `S3Client` against a real
//! MinIO (or S3-compatible) instance.
//!
//! These tests are **skipped silently** when no S3 endpoint is reachable
//! (local dev without MinIO). They only run when MinIO is available at the
//! configured endpoint (default: `http://localhost:9000`).
//!
//! Run with:
//! ```bash
//! cargo test --features s3 --test s3_integration
//! ```

#![forbid(unsafe_code)]

#[cfg(feature = "s3")]
use clarus_adapters::storage::s3::{ObjectStore, S3Error};
#[cfg(feature = "s3")]
use clarus_adapters::storage::s3_client::S3Client;
#[cfg(feature = "s3")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "s3")]
static KEY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "s3")]
fn unique_key(prefix: &str) -> String {
    let n = KEY_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}.bin", prefix, n)
}

#[cfg(feature = "s3")]
fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Connect to S3 or skip the test silently.
#[cfg(feature = "s3")]
fn connect_or_skip() -> Option<S3Client> {
    let endpoint = env_or_default("CLARUS_S3_ENDPOINT", "http://localhost:9000");
    let access_key = env_or_default("CLARUS_S3_ACCESS_KEY", "minioadmin");
    let secret_key = env_or_default("CLARUS_S3_SECRET_KEY", "minioadmin");
    let region = env_or_default("CLARUS_S3_REGION", "us-east-1");
    let bucket = env_or_default("CLARUS_S3_BUCKET", "clarus-test");

    let store = S3Client::new(
        &endpoint,
        &access_key,
        &secret_key,
        &region,
        &bucket,
        "store/",
    );

    // Probe with a HEAD on a sentinel key. `NotFound` means the server is
    // reachable (the bucket responds); a connection/credentials error means
    // MinIO is not up and we skip.
    match store.head_object("__clarus_probe__") {
        Ok(_) | Err(S3Error::NotFound(_)) => {
            eprintln!("S3 integration test: connected to {endpoint}/{bucket}");
            Some(store)
        }
        Err(e) => {
            eprintln!("Skipping S3 integration test: {endpoint} unreachable ({e})");
            None
        }
    }
}

// -- Basic CRUD ----------------------------------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_put_get_object() {
    let store = match connect_or_skip() {
        Some(s) => s,
        None => return,
    };

    let key = unique_key("test/put_get");
    let data = b"Hello S3 from Clarus integration test!".to_vec();

    let etag = store
        .put_object(&key, data.clone(), "application/octet-stream")
        .expect("put_object");
    assert!(!etag.is_empty(), "ETag must not be empty");

    let fetched = store.get_object(&key).expect("get_object");
    assert_eq!(fetched, data, "fetched data must match original");

    // Cleanup
    store.delete_object(&key).expect("delete_object");
}

// -- Head / metadata -----------------------------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_head_object() {
    let store = match connect_or_skip() {
        Some(s) => s,
        None => return,
    };

    let key = unique_key("test/head");
    let data = b"metadata test".to_vec();

    store
        .put_object(&key, data.clone(), "text/plain")
        .expect("put_object");

    let meta = store.head_object(&key).expect("head_object");
    assert_eq!(meta.content_length, data.len() as u64);

    // Non-existent key
    let err = store.head_object("test/nonexistent_deadbeef").unwrap_err();
    assert!(
        matches!(err, S3Error::NotFound(_)),
        "Expected NotFound, got: {err}"
    );

    // Cleanup
    store.delete_object(&key).expect("delete_object");
}

// -- Delete --------------------------------------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_delete_object() {
    let store = match connect_or_skip() {
        Some(s) => s,
        None => return,
    };

    let key = unique_key("test/delete");
    store
        .put_object(&key, b"delete me".to_vec(), "text/plain")
        .expect("put_object");

    // Confirm exists
    assert!(store.head_object(&key).is_ok());

    // Delete
    store.delete_object(&key).expect("delete_object");

    // Confirm gone
    let err = store.head_object(&key).unwrap_err();
    assert!(matches!(err, S3Error::NotFound(_)));
}

// -- Range read ----------------------------------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_get_object_range() {
    let store = match connect_or_skip() {
        Some(s) => s,
        None => return,
    };

    let key = unique_key("test/range");
    // Create 1000 bytes: [0, 1, 2, ..., 255, 0, 1, ...]
    let data: Vec<u8> = (0..1000u16).map(|i| (i % 256) as u8).collect();
    store
        .put_object(&key, data, "application/octet-stream")
        .expect("put_object");

    // Range 100-199 = exactly 100 bytes.
    let range_data = store
        .get_object_range(&key, 100, 199)
        .expect("get_object_range");
    assert_eq!(range_data.len(), 100);
    assert_eq!(range_data[0], 100u8);
    assert_eq!(range_data[99], 199u8);

    // Cleanup
    store.delete_object(&key).expect("delete_object");
}

// -- S3 key format -------------------------------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_key_format() {
    // This test does NOT need a live S3 connection.
    // Default prefix `store/`, two-level BLAKE3 layout.
    let store = S3Client::new(
        "http://localhost:9000",
        "a",
        "s",
        "us-east-1",
        "b",
        "store/",
    );
    let key = store.s3_key("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2");
    assert_eq!(
        key,
        "store/a1/b2/a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2.dcm"
    );

    // Custom prefix from config (`[s3] prefix`), e.g. a hospital sub-namespace.
    let custom = S3Client::new(
        "http://localhost:9000",
        "a",
        "s",
        "us-east-1",
        "b",
        "hospitalA/",
    );
    let key2 = custom.s3_key("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2");
    assert_eq!(
        key2,
        "hospitalA/a1/b2/a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2.dcm"
    );

    // Empty prefix (store shares a bucket with no namespace) stays two-level.
    let none = S3Client::new("http://localhost:9000", "a", "s", "us-east-1", "b", "");
    let key3 = none.s3_key("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2");
    assert_eq!(
        key3,
        "a1/b2/a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2.dcm"
    );

    // Invariant guard (owner 21.08): the S3 key must NEVER contain a backslash
    // (a PathBuf-derived key on Windows would diverge from LIST/walk and break
    // DR). The relative layout is the canonical cas_rel_path() - assert the
    // key equals prefix + cas_rel_path(hash).
    let hash = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2";
    for k in [&key, &key2, &key3] {
        assert!(!k.contains('\\'), "S3 key must not contain '\\': {k}");
    }
    assert_eq!(
        key,
        format!("store/{}", clarus_adapters::storage::cas_rel_path(hash))
    );
}

// -- Deduplication (same data -> same ETag) ------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_dedup_same_etag() {
    let store = match connect_or_skip() {
        Some(s) => s,
        None => return,
    };

    let key1 = unique_key("test/dedup_a");
    let key2 = unique_key("test/dedup_b");
    let data = b"identical content for dedup test".to_vec();

    let etag1 = store
        .put_object(&key1, data.clone(), "application/octet-stream")
        .expect("put_object 1");
    let etag2 = store
        .put_object(&key2, data.clone(), "application/octet-stream")
        .expect("put_object 2");

    assert_eq!(etag1, etag2, "Same content must produce same ETag");

    // Cleanup
    store.delete_object(&key1).ok();
    store.delete_object(&key2).ok();
}

// -- Blocking reader (full-object read) ----------------------------------

#[cfg(feature = "s3")]
#[test]
fn test_s3_get_object_range_stream() {
    let store = match connect_or_skip() {
        Some(s) => s,
        None => return,
    };

    let key = unique_key("test/stream");
    let data = b"streaming test data for Phase 2".to_vec();

    store
        .put_object(&key, data.clone(), "application/octet-stream")
        .expect("put_object");

    let (mut reader, size) = store
        .get_object_range_stream(&key, None)
        .expect("get_object_range_stream");
    assert_eq!(size, data.len() as u64);

    use std::io::Read;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).expect("read stream");
    assert_eq!(buf, data);

    // Cleanup
    store.delete_object(&key).ok();
}
