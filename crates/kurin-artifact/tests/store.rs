//! Blob store behaviour: content addressing, atomic concurrent writes,
//! corruption rejection, pruning, and legacy-cache migration.

use std::collections::HashSet;
use std::fs;
use std::io::{Cursor, Read};
use std::sync::{Arc, Barrier};
use std::thread;

use kurin_artifact::artifact::ArtifactStore;
use kurin_artifact::store::{BlobStore, StoreError};
use oci_spec::image::{Descriptor, Digest, MediaType};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

fn read_all(mut file: std::fs::File) -> Vec<u8> {
    let mut out = Vec::new();
    file.read_to_end(&mut out).unwrap();
    out
}

fn sha256_digest(bytes: &[u8]) -> Digest {
    let hex: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Digest::try_from(format!("sha256:{hex}")).expect("valid digest")
}

#[test]
fn put_read_and_size_round_trip() {
    let tmp = TempDir::new().unwrap();
    let store = BlobStore::open(tmp.path().join("blobs"));
    let payload = b"kurin blob store payload";
    let digest = store.put_bytes(payload).unwrap();

    assert_eq!(digest, sha256_digest(payload));
    assert!(store.has(&digest));
    assert_eq!(store.size(&digest).unwrap(), payload.len() as u64);
    assert_eq!(
        store.path(&digest),
        tmp.path().join("blobs/sha256").join(digest.digest())
    );
    assert_eq!(read_all(store.open_blob(&digest).unwrap()), payload);

    // Streaming a chunked reader yields the same digest.
    let streamed = store.put_stream(Cursor::new(payload)).unwrap();
    assert_eq!(streamed, digest);
    store.verify(&digest).unwrap();

    // Missing blobs are reported, not silently treated as empty.
    let missing = sha256_digest(b"never stored");
    assert!(!store.has(&missing));
    assert!(matches!(
        store.verify(&missing),
        Err(StoreError::NotFound { .. })
    ));
}

#[test]
fn corrupt_blob_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let store = BlobStore::open(tmp.path());

    let digest = store.put_bytes(b"original content").unwrap();
    // Tamper with the file behind the digest.
    fs::write(store.path(&digest), b"tampered content!").unwrap();
    assert!(matches!(
        store.verify(&digest),
        Err(StoreError::DigestMismatch { .. })
    ));
    assert!(store.open_blob(&digest).is_err());

    // A file stored under a digest it does not hash to is rejected too.
    let other = sha256_digest(b"other content");
    fs::write(store.path(&other), b"not other content").unwrap();
    assert!(matches!(
        store.verify(&other),
        Err(StoreError::DigestMismatch { .. })
    ));

    // Descriptor verification catches a size mismatch even when the
    // sha256 is correct.
    let good = store.put_bytes(b"size checked").unwrap();
    let wrong_size = Descriptor::new(
        MediaType::Other("application/octet-stream".into()),
        "size checked".len() as u64 + 1,
        good.clone(),
    );
    assert!(matches!(
        store.verify_descriptor(&wrong_size),
        Err(StoreError::SizeMismatch { .. })
    ));
    let right_size = Descriptor::new(
        MediaType::Other("application/octet-stream".into()),
        "size checked".len() as u64,
        good,
    );
    store.verify_descriptor(&right_size).unwrap();
}

#[test]
fn concurrent_writers_leave_one_blob_and_no_parts() {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(BlobStore::open(tmp.path().join("blobs")));
    let payload = vec![0x5a_u8; 256 * 1024];
    let expected = sha256_digest(&payload);
    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));

    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let store = Arc::clone(&store);
            let payload = payload.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                store.put_bytes(&payload).unwrap()
            })
        })
        .collect();

    for handle in handles {
        assert_eq!(handle.join().unwrap(), expected);
    }

    let dir = tmp.path().join("blobs/sha256");
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![expected.digest().to_string()]);
    assert!(names.iter().all(|n| !n.starts_with(".part")));
    store.verify(&expected).unwrap();
}

#[test]
fn prune_keeps_only_listed_digests() {
    let tmp = TempDir::new().unwrap();
    let store = BlobStore::open(tmp.path());
    let keep = store.put_bytes(b"keep me").unwrap();
    let drop_a = store.put_bytes(b"drop a").unwrap();
    let drop_b = store.put_bytes(b"drop b").unwrap();
    let kept_size = store.size(&keep).unwrap();

    let mut keep_set = HashSet::new();
    keep_set.insert(keep.clone());
    let report = store.prune(&keep_set).unwrap();

    assert_eq!(report.files_removed, 2);
    assert_eq!(
        report.bytes_removed,
        b"drop a".len() as u64 + b"drop b".len() as u64
    );
    assert!(store.has(&keep));
    assert!(!store.has(&drop_a));
    assert!(!store.has(&drop_b));
    assert_eq!(store.size(&keep).unwrap(), kept_size);

    // Pruning again is a no-op.
    assert_eq!(store.prune(&keep_set).unwrap().files_removed, 0);
}

#[test]
fn migration_imports_legacy_cache_without_deleting_it() {
    let tmp = TempDir::new().unwrap();
    let cache_root = tmp.path().join("kurin/artifacts");
    let legacy = cache_root.join("kubelet-1.33.1-amd64");
    fs::create_dir_all(&legacy).unwrap();
    let payload = b"legacy kubelet binary";
    let payload_path = legacy.join("kubelet");
    fs::write(&payload_path, payload).unwrap();
    fs::write(legacy.join("kubelet.sha256"), "ignored sidecar").unwrap();
    fs::write(legacy.join("partial.part"), "ignored partial").unwrap();

    let store = ArtifactStore::new(&cache_root);
    assert_eq!(
        store.blob_store().root(),
        tmp.path().join("kurin/blobs"),
        "the blob store must be the sibling of the legacy cache"
    );

    let report = store.migrate_legacy_cache().unwrap();
    assert_eq!(report.files_imported, 1);
    assert_eq!(report.bytes_imported, payload.len() as u64);
    assert_eq!(report.digests, 1);

    let digest = sha256_digest(payload);
    assert!(store.blob_store().has(&digest));
    store.blob_store().verify(&digest).unwrap();

    // The legacy files stay in place.
    assert_eq!(fs::read(&payload_path).unwrap(), payload);
    assert!(legacy.join("kubelet.sha256").is_file());

    // A second run is idempotent and reports the same unique digest.
    let again = store.migrate_legacy_cache().unwrap();
    assert_eq!(again.files_imported, 1);
    assert_eq!(again.digests, 1);
}
