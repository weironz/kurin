//! OCI layout round-trips, index resolution, and archive safety.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};

use kurin_artifact::oci::layout::{Layout, LayoutError, REF_NAME_ANNOTATION};
use oci_spec::image::{
    Descriptor, DescriptorBuilder, Digest, ImageIndex, ImageIndexBuilder, MediaType,
};
use tempfile::TempDir;

fn read_all(mut file: std::fs::File) -> Vec<u8> {
    let mut out = Vec::new();
    file.read_to_end(&mut out).unwrap();
    out
}

fn descriptor(media_type: MediaType, digest: Digest, size: u64, tag: Option<&str>) -> Descriptor {
    let mut builder = DescriptorBuilder::default()
        .media_type(media_type)
        .digest(digest)
        .size(size);
    if let Some(tag) = tag {
        builder = builder.annotations(HashMap::from([(
            REF_NAME_ANNOTATION.to_string(),
            tag.to_string(),
        )]));
    }
    builder.build().unwrap()
}

fn index(manifests: Vec<Descriptor>) -> ImageIndex {
    ImageIndexBuilder::default()
        .schema_version(2u32)
        .media_type(MediaType::ImageIndex)
        .manifests(manifests)
        .build()
        .unwrap()
}

/// Build a layout with one tagged manifest blob and one config blob.
fn sample_layout(root: &std::path::Path) -> (Layout, Digest, Digest) {
    let layout = Layout::create(root).unwrap();
    let manifest_bytes = br#"{"schemaVersion":2,"layers":[]}"#;
    let config_bytes = b"{}";
    let manifest = layout.put_blob(manifest_bytes).unwrap();
    let config = layout.put_blob_stream(&config_bytes[..]).unwrap();
    let index = index(vec![descriptor(
        MediaType::ImageManifest,
        manifest.clone(),
        manifest_bytes.len() as u64,
        Some("v1"),
    )]);
    layout.write_index(&index).unwrap();
    (layout, manifest, config)
}

#[test]
fn layout_round_trip_and_resolve() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("layout");
    let (layout, manifest, config) = sample_layout(&root);

    // The marker is exactly the spec's JSON object.
    assert_eq!(
        fs::read_to_string(root.join("oci-layout")).unwrap(),
        r#"{"imageLayoutVersion":"1.0.0"}"#
    );
    assert!(layout.blobs_dir().is_dir());
    assert_eq!(layout.blobs_dir(), root.join("blobs"));

    // Reopen and read the index back.
    let reopened = Layout::open(&root).unwrap();
    let index = reopened.read_index().unwrap();
    assert_eq!(index.manifests().len(), 1);

    // Resolve by tag and by digest.
    let by_tag = reopened.resolve("v1").unwrap();
    assert_eq!(by_tag.digest(), &manifest);
    let by_digest = reopened.resolve(manifest.as_ref()).unwrap();
    assert_eq!(by_digest.digest(), &manifest);
    assert!(matches!(
        reopened.resolve("does-not-exist"),
        Err(LayoutError::ReferenceNotFound { .. })
    ));

    // Both blobs are readable and verified.
    assert_eq!(
        read_all(reopened.blobs().open_blob(&manifest).unwrap()),
        br#"{"schemaVersion":2,"layers":[]}"#
    );
    assert_eq!(
        read_all(reopened.blobs().open_blob(&config).unwrap()),
        b"{}"
    );
}

#[test]
fn tar_round_trip() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("layout");
    let (layout, manifest, config) = sample_layout(&root);

    let mut tar = Vec::new();
    layout.pack_tar(&mut tar).unwrap();
    assert!(!tar.is_empty());

    let dest = tmp.path().join("unpacked");
    let unpacked = Layout::unpack_tar(&tar[..], &dest).unwrap();
    assert_eq!(unpacked.read_index().unwrap().manifests().len(), 1);
    assert_eq!(unpacked.resolve("v1").unwrap().digest(), &manifest);
    assert_eq!(
        read_all(unpacked.blobs().open_blob(&config).unwrap()),
        b"{}"
    );
    assert!(dest.join("oci-layout").is_file());
}

#[test]
fn tar_zstd_round_trip() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("layout");
    let (layout, manifest, _config) = sample_layout(&root);

    let mut packed = Vec::new();
    layout.pack_tar_zstd(&mut packed).unwrap();
    assert!(packed.len() < 1024 * 1024);

    let dest = tmp.path().join("unpacked");
    let unpacked = Layout::unpack_tar_zstd(&packed[..], &dest).unwrap();
    assert_eq!(unpacked.resolve("v1").unwrap().digest(), &manifest);
    let resolved = unpacked.resolve("v1").unwrap();
    unpacked.blobs().verify(resolved.digest()).unwrap();
}

/// Build a tar whose entry names are written verbatim (the `tar` crate's
/// own `set_path` refuses traversal, so the header bytes are filled in
/// directly — this is exactly what a hostile archive looks like).
fn hostile_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        {
            let bytes = header.as_mut_bytes();
            bytes[..name.len()].copy_from_slice(name.as_bytes());
        }
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    builder.into_inner().unwrap()
}

#[test]
fn tar_traversal_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let dest = tmp.path().join("dest");
    let archive = hostile_tar(&[("../evil", b"pwned"), ("oci-layout", b"{}")]);
    let err = Layout::unpack_tar(&archive[..], &dest).unwrap_err();
    assert!(matches!(err, LayoutError::UnsafeEntry { .. }), "{err}");
    assert!(!tmp.path().join("evil").exists());
    assert!(!dest.join("evil").exists());

    let dest = tmp.path().join("dest-abs");
    let archive = hostile_tar(&[("/evil", b"pwned")]);
    let err = Layout::unpack_tar(&archive[..], &dest).unwrap_err();
    assert!(matches!(err, LayoutError::UnsafeEntry { .. }), "{err}");
    assert!(!dest.join("evil").exists());
}

#[test]
fn unpack_rejects_symlinks() {
    let tmp = TempDir::new().unwrap();
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_path("link").unwrap();
    header.set_size(0);
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_link_name("/etc/passwd").unwrap();
    header.set_mode(0o777);
    header.set_cksum();
    builder.append(&header, std::io::empty()).unwrap();
    let archive = builder.into_inner().unwrap();

    let err = Layout::unpack_tar(&archive[..], tmp.path().join("dest")).unwrap_err();
    assert!(matches!(err, LayoutError::UnsupportedEntry { .. }), "{err}");
}

#[test]
fn open_rejects_missing_or_wrong_marker() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("missing");
    fs::create_dir_all(&missing).unwrap();
    assert!(matches!(
        Layout::open(&missing),
        Err(LayoutError::InvalidLayout { .. })
    ));

    let wrong = tmp.path().join("wrong");
    fs::create_dir_all(&wrong).unwrap();
    fs::write(
        wrong.join("oci-layout"),
        r#"{"imageLayoutVersion":"2.0.0"}"#,
    )
    .unwrap();
    assert!(matches!(
        Layout::open(&wrong),
        Err(LayoutError::InvalidLayout { .. })
    ));
}

#[test]
fn index_write_is_atomic_and_leaves_no_parts() {
    let tmp = TempDir::new().unwrap();
    let layout = Layout::create(tmp.path().join("layout")).unwrap();
    layout.write_index(&index(Vec::new())).unwrap();
    let leftovers: Vec<_> = fs::read_dir(layout.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".part"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let mut out = Vec::new();
    writeln!(out, "{}", layout.read_index().unwrap()).unwrap();
    assert!(!out.is_empty());
}
