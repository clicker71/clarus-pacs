// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! S3-compatible object storage backend (Amazon S3, MinIO, Yandex Object Storage,
//! Google Cloud Storage, Ceph RGW). WARNING: NOT to be confused with S3 Trio 3D -
//! that's a 1990s graphics card.
//!
//! Enabled via `--features s3`.
//!
//! # Architecture
//! - `ObjectStore` trait - abstract interface (6 methods)
//! - `S3Client` (s3_client.rs) - production implementation using reqwest + SigV4
//! - `InMemoryObjectStore` - test double, reference implementation
//! - `s3_http.rs` - global reqwest::Client pool (OnceLock)

#[cfg(feature = "s3")]
mod s3_impl {
    use sha2::Digest;
    use std::sync::Arc;

    // -- Error type ----------------------------------------------

    #[derive(Debug)]
    pub enum S3Error {
        Connection(String),
        NotFound(String),
        Request(String),
        Credentials(String),
    }

    impl std::fmt::Display for S3Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                S3Error::Connection(s) => write!(f, "S3 connection error: {s}"),
                S3Error::NotFound(s) => write!(f, "Object not found: {s}"),
                S3Error::Request(s) => write!(f, "S3 request error: {s}"),
                S3Error::Credentials(s) => write!(f, "S3 credentials error: {s}"),
            }
        }
    }

    impl std::error::Error for S3Error {}

    // -- Metadata types ------------------------------------------

    #[derive(Debug, Clone)]
    pub struct ObjectMeta {
        pub content_length: u64,
        pub etag: String,
    }

    #[derive(Debug, Clone)]
    pub struct ObjectInfo {
        pub key: String,
        pub size: u64,
        pub last_modified: String,
    }

    // -- ObjectStore trait ---------------------------------------

    /// Object storage backend - S3-compatible blob store abstraction.
    ///
    /// Synchronous (S3 -> blocking). Runs on its own OS thread.
    pub trait ObjectStore: Send + Sync + Clone + 'static {
        /// Store an object. Returns ETag. `data` is owned `Vec<u8>` (diet 22.08:
        /// reqwest blocking takes an owned body, so `bytes::Bytes` is unnecessary).
        fn put_object(
            &self,
            key: &str,
            data: Vec<u8>,
            content_type: &str,
        ) -> Result<String, S3Error>;

        /// Retrieve entire object. S3-read is a rare path; the one `Vec<u8>` copy
        /// (from reqwest's `Bytes`) is documented at the boundary in s3_client.
        fn get_object(&self, key: &str) -> Result<Vec<u8>, S3Error>;

        /// Retrieve byte range (for WADO-RS Range requests).
        fn get_object_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error>;

        /// Check if object exists. Returns metadata.
        fn head_object(&self, key: &str) -> Result<ObjectMeta, S3Error>;

        /// Delete object.
        fn delete_object(&self, key: &str) -> Result<(), S3Error>;

        /// List objects with prefix (for scrub/audit).
        fn list_objects(
            &self,
            prefix: &str,
            max_keys: Option<u32>,
        ) -> Result<Vec<ObjectInfo>, S3Error>;
    }

    // -- InMemoryObjectStore for testing -------------------------

    #[derive(Clone, Default)]
    pub struct InMemoryObjectStore {
        objects: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    impl ObjectStore for InMemoryObjectStore {
        fn put_object(
            &self,
            key: &str,
            data: Vec<u8>,
            _content_type: &str,
        ) -> Result<String, S3Error> {
            let etag = crate::hex_encode(&sha2::Sha256::digest(&data));
            self.objects
                .lock()
                .map_err(|e| S3Error::Request(e.to_string()))?
                .insert(key.to_string(), data);
            Ok(etag)
        }

        fn get_object(&self, key: &str) -> Result<Vec<u8>, S3Error> {
            self.objects
                .lock()
                .map_err(|e| S3Error::Request(e.to_string()))?
                .get(key)
                .cloned()
                .ok_or_else(|| S3Error::NotFound(key.to_string()))
        }

        fn get_object_range(&self, key: &str, start: u64, end: u64) -> Result<Vec<u8>, S3Error> {
            let data = self.get_object(key)?;
            let start = start as usize;
            let end = (end as usize).min(data.len().saturating_sub(1));
            if start > end || start >= data.len() {
                return Err(S3Error::Request(format!(
                    "range {start}-{end} out of bounds (len={})",
                    data.len()
                )));
            }
            Ok(data[start..=end].to_vec())
        }

        fn head_object(&self, key: &str) -> Result<ObjectMeta, S3Error> {
            let guard = self
                .objects
                .lock()
                .map_err(|e| S3Error::Request(e.to_string()))?;
            let data = guard
                .get(key)
                .ok_or_else(|| S3Error::NotFound(key.to_string()))?;
            let etag = crate::hex_encode(&sha2::Sha256::digest(data));
            Ok(ObjectMeta {
                content_length: data.len() as u64,
                etag,
            })
        }

        fn delete_object(&self, key: &str) -> Result<(), S3Error> {
            self.objects
                .lock()
                .map_err(|e| S3Error::Request(e.to_string()))?
                .remove(key);
            Ok(())
        }

        fn list_objects(
            &self,
            prefix: &str,
            _max_keys: Option<u32>,
        ) -> Result<Vec<ObjectInfo>, S3Error> {
            let guard = self
                .objects
                .lock()
                .map_err(|e| S3Error::Request(e.to_string()))?;
            let objects: Vec<ObjectInfo> = guard
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| ObjectInfo {
                    key: k.clone(),
                    size: v.len() as u64,
                    last_modified: String::new(),
                })
                .collect();
            Ok(objects)
        }
    }
}

#[cfg(feature = "s3")]
pub use s3_impl::*;

// -- Tests ----------------------------------------------------------

#[cfg(test)]
#[cfg(feature = "s3")]
mod tests {
    use super::s3_impl::*;

    #[test]
    fn test_inmemory_put_get() {
        let store = InMemoryObjectStore::default();
        let data = b"hello world".to_vec();
        let etag = store
            .put_object("test/key1", data.clone(), "application/octet-stream")
            .unwrap();
        assert!(!etag.is_empty());

        let retrieved = store.get_object("test/key1").unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_inmemory_head() {
        let store = InMemoryObjectStore::default();
        let data = b"hello".to_vec();
        store
            .put_object("test/head", data.clone(), "text/plain")
            .unwrap();

        let meta = store.head_object("test/head").unwrap();
        assert_eq!(meta.content_length, 5);

        // Non-existent key
        let err = store.head_object("test/nope").unwrap_err();
        assert!(matches!(err, S3Error::NotFound(_)));
    }

    #[test]
    fn test_inmemory_delete() {
        let store = InMemoryObjectStore::default();
        store
            .put_object("test/del", b"data".to_vec(), "text/plain")
            .unwrap();

        // Exists before delete
        assert!(store.head_object("test/del").is_ok());

        store.delete_object("test/del").unwrap();

        // Gone after delete
        let err = store.head_object("test/del").unwrap_err();
        assert!(matches!(err, S3Error::NotFound(_)));
    }

    #[test]
    fn test_inmemory_list() {
        let store = InMemoryObjectStore::default();
        for i in 0..3u8 {
            store
                .put_object(
                    &format!("prefix/item{i}"),
                    vec![i; 10],
                    "application/octet-stream",
                )
                .unwrap();
        }

        let list = store.list_objects("prefix/", None).unwrap();
        assert_eq!(list.len(), 3);
        for item in &list {
            assert!(item.key.starts_with("prefix/item"));
            assert_eq!(item.size, 10);
        }
    }

    #[test]
    fn test_inmemory_range() {
        let store = InMemoryObjectStore::default();
        // Create 1000 bytes: [0, 1, 2, ..., 255, 0, 1, ...]
        let data: Vec<u8> = (0..1000u16).map(|i| (i % 256) as u8).collect();
        store
            .put_object("test/range", data, "application/octet-stream")
            .unwrap();

        // Range 100-199 -> exactly 100 bytes
        let range_data = store.get_object_range("test/range", 100, 199).unwrap();
        assert_eq!(range_data.len(), 100);
        assert_eq!(range_data[0], 100u8);
        assert_eq!(range_data[99], 199u8);
    }
}
