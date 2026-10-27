//! cosign public-key signature verification (04 §6, ADR-14, WP-39).
//!
//! ## Dependency choice
//!
//! The `sigstore` crate was evaluated and rejected: its public-key
//! verification surface is inseparable from the full cosign client
//! (keyless Fulcio flow, rekor bundles, registry helpers), pulling
//! `openidconnect`, `reqwest`, `x509-parser` and a second crypto
//! stack on top of `ring`/`aws-lc-rs` — and it is not available in
//! the offline crate cache this build runs against. The needed
//! surface is small and exactly what 04 §6 anticipated as the
//! fallback: **cosign's simple-signing payload, verified as ECDSA
//! P-256 with the `p256` crate**. `p256` is pure Rust and already in
//! the dependency graph (via `russh`/`ssh-key`), so this adds no new
//! crates to cross-compile.
//!
//! ## Format
//!
//! What cosign signs (the [signature spec]) is a detached JSON
//! payload in Red Hat's simple-signing format:
//!
//! ```json
//! {"critical":{"identity":{"docker-reference":"ghcr.io/weironz/kurin/base"},
//!              "image":{"docker-manifest-digest":"sha256:<package index>"},
//!              "type":"cosign container image signature"},
//!  "optional":null}
//! ```
//!
//! The signature itself is ECDSA-P256-with-SHA-256 over those exact
//! bytes, ASN.1-DER encoded, base64'd, and stored as the
//! `dev.cosignproject.cosign/signature` annotation on the payload
//! layer of a referrer manifest attached to the package index (see
//! [`crate::referrer`]). Field order in the payload JSON matters —
//! cosign's Go serializer emits `critical` → `optional`, and within
//! `critical`: `identity` → `image` → `type`, compact, no spaces —
//! so [`SimpleSigning::payload_bytes`] reproduces those exact bytes.
//!
//! [signature spec]: https://github.com/sigstore/cosign/blob/main/specs/SIGNATURE_SPEC.md

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use oci_spec::image::{Descriptor, Digest};
use p256::ecdsa::{signature::Signer, signature::Verifier, Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::oci::layout::Layout;
use crate::referrer::{self, Referrer};

/// The cosign signature manifest's `artifactType`.
pub const ARTIFACT_TYPE: &str = "application/vnd.dev.cosign.artifact.sig.v1+json";
/// The signed payload's media type (the referrer's single layer).
pub const MEDIA_TYPE_PAYLOAD: &str = "application/vnd.dev.cosign.simplesigning.v1+json";
/// Layer annotation carrying the base64-encoded DER signature.
pub const SIGNATURE_ANNOTATION: &str = "dev.cosignproject.cosign/signature";
/// The `critical.type` value of a cosign simple-signing payload.
pub const SIGNATURE_TYPE: &str = "cosign container image signature";

/// Errors raised by signature handling.
#[derive(Debug, Error)]
pub enum SignatureError {
    /// A trusted key could not be loaded.
    #[error("trusted key {path}: {detail}")]
    Key { path: PathBuf, detail: String },

    /// Signature mode was requested without any trusted key.
    #[error(
        "signature verification needs at least one trusted key (--key / packages.trusted-keys)"
    )]
    NoTrustedKeys,

    /// The package carries no signature at all.
    #[error("package {index} is not signed (no cosign signature referrer found)")]
    NotSigned { index: String },

    /// Signatures exist but none verifies against the trusted keys.
    #[error("no signature of {index} verifies against any trusted key:\n{}", details.join("\n"))]
    SignatureRejected { index: String, details: Vec<String> },

    /// A signature payload did not have the cosign shape.
    #[error("malformed signature payload: {detail}")]
    MalformedPayload { detail: String },

    /// The payload does not attest the package being verified.
    #[error("signature payload mismatch: {detail}")]
    PayloadMismatch { detail: String },

    /// A referrer could not be read.
    #[error(transparent)]
    Referrer(#[from] referrer::ReferrerError),

    /// The backing layout failed.
    #[error(transparent)]
    Layout(#[from] crate::oci::layout::LayoutError),

    /// The blob store rejected a read.
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),

    /// Reading a blob failed.
    #[error("signature blob I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// The cosign simple-signing payload (the signed document itself).
///
/// Serialization order is load-bearing: it must match cosign's Go
/// `json.Marshal` byte for byte (see the module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleSigning {
    /// The signed identity, digest and type.
    pub critical: Critical,
    /// User-specified claims; `None` serializes as `null`, exactly
    /// like cosign without `--annotations`.
    pub optional: Option<BTreeMap<String, String>>,
}

/// The `critical` section of a simple-signing payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Critical {
    /// The image (here: package) identity.
    pub identity: Identity,
    /// The digest being attested.
    pub image: Image,
    /// Always `cosign container image signature` for cosign.
    #[serde(rename = "type")]
    pub kind: String,
}

/// The identity section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// The repository the signature was created for. Verification
    /// ignores it (packages move between registries); it is kept for
    /// cosign interoperability.
    #[serde(rename = "docker-reference")]
    pub docker_reference: String,
}

/// The image section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    /// The digest of the signed object — the package index.
    #[serde(rename = "docker-manifest-digest")]
    pub docker_manifest_digest: String,
}

impl SimpleSigning {
    /// Build the payload cosign would build for `index_digest` in
    /// `docker_reference`.
    pub fn new(index_digest: &str, docker_reference: &str) -> Self {
        SimpleSigning {
            critical: Critical {
                identity: Identity {
                    docker_reference: docker_reference.to_string(),
                },
                image: Image {
                    docker_manifest_digest: index_digest.to_string(),
                },
                kind: SIGNATURE_TYPE.to_string(),
            },
            optional: None,
        }
    }

    /// The exact bytes cosign signs: compact JSON, `critical` first,
    /// then `optional` (as `null` when unset).
    ///
    /// # Errors
    ///
    /// Serialization failure (never for the ASCII-only values the
    /// fields are constrained to).
    pub fn payload_bytes(&self) -> Result<Vec<u8>, SignatureError> {
        serde_json::to_vec(self).map_err(|e| SignatureError::MalformedPayload {
            detail: format!("serializing payload: {e}"),
        })
    }

    /// Parse a signed payload.
    ///
    /// # Errors
    ///
    /// Not a cosign simple-signing document.
    pub fn parse(bytes: &[u8]) -> Result<Self, SignatureError> {
        serde_json::from_slice(bytes).map_err(|e| SignatureError::MalformedPayload {
            detail: format!("{e}"),
        })
    }

    /// True when `critical.type` marks a cosign signature.
    pub fn is_cosign(&self) -> bool {
        self.critical.kind == SIGNATURE_TYPE
    }
}

/// One loaded cosign public key.
#[derive(Debug, Clone)]
pub struct TrustedKey {
    /// Where the key came from (CLI `--key` / `trusted-keys`).
    pub path: PathBuf,
    /// The P-256 verifying key.
    pub key: VerifyingKey,
}

impl TrustedKey {
    /// A short stable id for reports: the first 16 hex chars of the
    /// key's SEC1 point hash.
    pub fn fingerprint(&self) -> String {
        let point = self.key.to_sec1_bytes();
        let digest = Sha256::digest(point.as_ref());
        digest[..8].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parse a cosign public key: a PEM `PUBLIC KEY` block holding a
    /// DER SubjectPublicKeyInfo (what `cosign public-key` writes), or
    /// a `COSIGN PUBLIC KEY` block holding the raw SEC1 point.
    pub fn from_pem(path: &Path, text: &str) -> Result<Self, SignatureError> {
        let key = parse_public_key_pem(text).map_err(|detail| SignatureError::Key {
            path: path.to_path_buf(),
            detail,
        })?;
        Ok(TrustedKey {
            path: path.to_path_buf(),
            key,
        })
    }

    /// Load and parse a key file.
    ///
    /// # Errors
    ///
    /// Reading or parsing failed; the path is part of the error.
    pub fn load(path: &Path) -> Result<Self, SignatureError> {
        let text = std::fs::read_to_string(path).map_err(|e| SignatureError::Key {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        Self::from_pem(path, &text)
    }
}

/// The keys `verify = "signature"` trusts; any one of them may sign.
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys(Vec<TrustedKey>);

impl TrustedKeys {
    /// Load every key, failing on the first unreadable one so a typo
    /// in the config cannot silently reduce the trust set.
    ///
    /// # Errors
    ///
    /// One of the paths did not hold a cosign public key.
    pub fn load(paths: &[PathBuf]) -> Result<Self, SignatureError> {
        let mut keys = Vec::with_capacity(paths.len());
        for path in paths {
            keys.push(TrustedKey::load(path)?);
        }
        Ok(TrustedKeys(keys))
    }

    /// Number of loaded keys.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when no key loaded (signature verification cannot run).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The key files, for reports.
    pub fn paths(&self) -> Vec<&Path> {
        self.0.iter().map(|k| k.path.as_path()).collect()
    }
}

/// Parse a cosign public-key PEM. Two shapes exist in the wild:
/// `-----BEGIN PUBLIC KEY-----` (DER SubjectPublicKeyInfo — what
/// `cosign public-key` writes) and `-----BEGIN COSIGN PUBLIC KEY-----`
/// (raw uncompressed SEC1 point).
fn parse_public_key_pem(text: &str) -> Result<VerifyingKey, String> {
    use p256::pkcs8::DecodePublicKey;
    if let Ok(key) = VerifyingKey::from_public_key_pem(text.trim()) {
        return Ok(key);
    }
    // fall back to the raw-point form
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return Err("empty key file".to_string());
    };
    let first = first.trim();
    let (label, want_end) = match first {
        "-----BEGIN PUBLIC KEY-----" => ("PUBLIC KEY", "-----END PUBLIC KEY-----"),
        "-----BEGIN COSIGN PUBLIC KEY-----" => {
            ("COSIGN PUBLIC KEY", "-----END COSIGN PUBLIC KEY-----")
        }
        _ => {
            return Err(format!(
                "not a PEM public key (expected '-----BEGIN PUBLIC KEY-----', found {first:?})"
            ))
        }
    };
    let mut body = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == want_end {
            closed = true;
            break;
        }
        body.push_str(line.trim());
    }
    if !closed {
        return Err(format!("PEM block for {label} is not terminated"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|e| format!("PEM body is not valid base64: {e}"))?;
    VerifyingKey::from_sec1_bytes(&bytes).map_err(|_| {
        format!(
            "not a P-256 public key ({label} body is {} bytes)",
            bytes.len()
        )
    })
}

/// What [`verify`] established.
#[derive(Debug, Clone, Default)]
pub struct SignatureCheck {
    /// Signatures that verified against a trusted key.
    pub signatures: usize,
    /// The fingerprints of the keys that verified them.
    pub key_fingerprints: Vec<String>,
}

/// Sign the package at `subject` in `layout` with `key` (the same
/// bytes cosign would sign), attaching the signature as a referrer.
/// Signing itself is a CI/tooling concern (the release workflow uses
/// real cosign); this exists so the verification path can be tested
/// end to end and for local tooling.
///
/// # Errors
///
/// Referrer or serialization failure.
pub fn sign(
    layout: &Layout,
    subject: &Descriptor,
    key: &SigningKey,
    docker_reference: &str,
) -> Result<Descriptor, SignatureError> {
    let payload = SimpleSigning::new(subject.digest().as_ref(), docker_reference);
    let bytes = payload.payload_bytes()?;
    let signature: Signature = key.sign(&bytes);
    let encoded = base64::engine::general_purpose::STANDARD.encode(signature.to_der().as_ref());
    referrer::attach(
        layout,
        subject,
        ARTIFACT_TYPE,
        MEDIA_TYPE_PAYLOAD,
        &bytes,
        std::collections::HashMap::from([(SIGNATURE_ANNOTATION.to_string(), encoded)]),
    )
    .map_err(SignatureError::from)
}

/// Verify every cosign signature referrer of the package index named
/// by `subject` against `keys`: at least one signature must verify with
/// at least one trusted key, and its payload must attest exactly this
/// package (digest and cosign type). Payload and signature bytes are
/// read through the blob store, so both are digest-verified on the
/// way in.
///
/// # Errors
///
/// [`SignatureError::NotSigned`] when the package has no signature,
/// [`SignatureError::SignatureRejected`] when signatures exist but
/// none match a trusted key, and the parse/referrer errors for
/// malformed signatures — never a silent pass.
pub fn verify(
    layout: &Layout,
    subject: &Digest,
    keys: &TrustedKeys,
) -> Result<SignatureCheck, SignatureError> {
    if keys.is_empty() {
        return Err(SignatureError::NoTrustedKeys);
    }
    let mut check = SignatureCheck::default();
    let mut failures: Vec<String> = Vec::new();
    let mut saw_signature = false;
    for referrer in signature_referrers(layout, subject)? {
        saw_signature = true;
        match verify_one(layout, &referrer, subject, keys) {
            Ok(fingerprint) => {
                check.signatures += 1;
                if !check.key_fingerprints.contains(&fingerprint) {
                    check.key_fingerprints.push(fingerprint);
                }
            }
            Err(e) => failures.push(format!("signature {}: {e}", referrer.descriptor.digest())),
        }
    }
    if check.signatures > 0 {
        return Ok(check);
    }
    if saw_signature {
        Err(SignatureError::SignatureRejected {
            index: subject.to_string(),
            details: failures,
        })
    } else {
        Err(SignatureError::NotSigned {
            index: subject.to_string(),
        })
    }
}

/// The referrers of `subject` that carry a cosign signature payload.
fn signature_referrers(layout: &Layout, subject: &Digest) -> Result<Vec<Referrer>, SignatureError> {
    Ok(referrer::find(layout, subject)?
        .into_iter()
        .filter(|r| {
            r.artifact_type() == ARTIFACT_TYPE
                || r.payload()
                    .map(|p| p.media_type().as_ref() == MEDIA_TYPE_PAYLOAD)
                    .unwrap_or(false)
        })
        .collect())
}

/// Verify one signature referrer; on success return the verifying
/// key's fingerprint.
fn verify_one(
    layout: &Layout,
    referrer: &Referrer,
    subject: &Digest,
    keys: &TrustedKeys,
) -> Result<String, SignatureError> {
    let payload = referrer.payload()?;
    let mut bytes = Vec::new();
    layout
        .blobs()
        .open_blob(payload.digest())?
        .read_to_end(&mut bytes)?;
    let annotation = referrer
        .payload_annotations()
        .and_then(|a| a.get(SIGNATURE_ANNOTATION))
        .ok_or_else(|| SignatureError::MalformedPayload {
            detail: format!(
                "layer {} has no {SIGNATURE_ANNOTATION} annotation",
                payload.digest()
            ),
        })?;
    let document = SimpleSigning::parse(&bytes)?;
    if !document.is_cosign() {
        return Err(SignatureError::PayloadMismatch {
            detail: format!("critical.type is not {SIGNATURE_TYPE:?}"),
        });
    }
    if document.critical.image.docker_manifest_digest != subject.to_string() {
        return Err(SignatureError::PayloadMismatch {
            detail: format!(
                "payload attests {} but the package index is {subject}",
                document.critical.image.docker_manifest_digest
            ),
        });
    }
    let signature = decode_signature(annotation)?;
    for key in &keys.0 {
        if key.key.verify(&bytes, &signature).is_ok() {
            return Ok(key.fingerprint());
        }
    }
    Err(SignatureError::SignatureRejected {
        index: subject.to_string(),
        details: vec![format!(
            "signature over {} matches none of the {} trusted key(s)",
            payload.digest(),
            keys.len()
        )],
    })
}

/// Decode the annotation: base64 → DER `r,s` pair (what cosign
/// writes), or a fixed-width `r||s` pair (accepted for leniency).
fn decode_signature(encoded: &str) -> Result<Signature, SignatureError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| SignatureError::MalformedPayload {
            detail: format!("signature annotation is not valid base64: {e}"),
        })?;
    if let Ok(der) = p256::ecdsa::DerSignature::from_bytes(&bytes) {
        return Signature::try_from(der).map_err(|e| SignatureError::MalformedPayload {
            detail: format!("DER signature does not decode: {e}"),
        });
    }
    Signature::from_slice(&bytes).map_err(|e| SignatureError::MalformedPayload {
        detail: format!("signature is neither DER nor 64-byte fixed width: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_slice(&[seed; 32]).expect("a valid scalar")
    }

    fn test_subject() -> (oci_spec::image::Descriptor, Digest) {
        let digest = Digest::try_from(
            "sha256:1119abab63e605dcc281019bad0424744178b6f61ba57378701fe7391994c999",
        )
        .unwrap();
        let descriptor = oci_spec::image::DescriptorBuilder::default()
            .media_type(oci_spec::image::MediaType::ImageIndex)
            .digest(digest.clone())
            .size(4u64)
            .build()
            .unwrap();
        (descriptor, digest)
    }

    #[test]
    fn payload_bytes_match_cosign_exactly() {
        let payload = SimpleSigning::new(
            "sha256:1119abab63e605dcc281019bad0424744178b6f61ba57378701fe7391994c999",
            "ghcr.io/weironz/kurin/base",
        );
        let bytes = payload.payload_bytes().unwrap();
        // cosign's Go json.Marshal emits exactly this shape (compact,
        // critical first, optional: null).
        let expected = concat!(
            r#"{"critical":{"identity":{"docker-reference":"ghcr.io/weironz/kurin/base"},"#,
            r#""image":{"docker-manifest-digest":"sha256:1119abab63e605dcc281019bad0424744178b6f61ba57378701fe7391994c999"},"#,
            r#""type":"cosign container image signature"},"optional":null}"#
        );
        assert_eq!(String::from_utf8(bytes).unwrap(), expected);
        // and parses back
        let parsed = SimpleSigning::parse(expected.as_bytes()).unwrap();
        assert!(parsed.is_cosign());
        assert_eq!(parsed.payload_bytes().unwrap(), expected.as_bytes());
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::create(tmp.path()).unwrap();
        let (subject, index_digest) = test_subject();
        sign(
            &layout,
            &subject,
            &signing_key(7),
            "registry.local/kurin/base",
        )
        .unwrap();
        let keys = TrustedKeys(vec![TrustedKey {
            path: PathBuf::from("cosign.pub"),
            key: *signing_key(7).verifying_key(),
        }]);
        let check = verify(&layout, &index_digest, &keys).unwrap();
        assert_eq!(check.signatures, 1);

        // a different key does not verify
        let wrong = TrustedKeys(vec![TrustedKey {
            path: PathBuf::from("other.pub"),
            key: *signing_key(8).verifying_key(),
        }]);
        let err = verify(&layout, &index_digest, &wrong).unwrap_err();
        assert!(
            matches!(err, SignatureError::SignatureRejected { .. }),
            "{err}"
        );
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::create(tmp.path()).unwrap();
        let (subject, index_digest) = test_subject();
        sign(
            &layout,
            &subject,
            &signing_key(7),
            "registry.local/kurin/base",
        )
        .unwrap();
        // tamper: swap the payload layer's blob content for another
        // simple-signing document with a different digest
        let swapped = SimpleSigning::new(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "registry.local/kurin/base",
        )
        .payload_bytes()
        .unwrap();
        let found = referrer::find(&layout, &index_digest).unwrap();
        let payload_digest = found[0].payload().unwrap().digest().clone();
        // replace the payload bytes in place: the blob store now
        // serves content that no longer matches the layer's digest
        std::fs::write(layout.blob_path(&payload_digest), &swapped).unwrap();
        let keys = TrustedKeys(vec![TrustedKey {
            path: PathBuf::from("cosign.pub"),
            key: *signing_key(7).verifying_key(),
        }]);
        let err = verify(&layout, &index_digest, &keys).unwrap_err();
        // fails closed: the blob store rejects the swapped bytes, and
        // with no verifiable signature left the whole check is
        // rejected (the corruption is in the failure details)
        assert!(
            matches!(
                err,
                SignatureError::Store(_)
                    | SignatureError::PayloadMismatch { .. }
                    | SignatureError::SignatureRejected { .. }
            ) && err.to_string().contains("corrupt"),
            "{err}"
        );
    }

    #[test]
    fn keys_load_from_cosign_pem() {
        let key = signing_key(7);
        use p256::pkcs8::EncodePublicKey;
        let pem = key
            .verifying_key()
            .to_public_key_pem(p256::pkcs8::LineEnding::LF)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("cosign.pub");
        std::fs::write(&path, &pem).unwrap();
        let loaded = TrustedKeys::load(std::slice::from_ref(&path)).unwrap();
        assert_eq!(loaded.len(), 1);

        // raw-point form under the cosign label also parses
        let point = key.verifying_key().to_sec1_bytes();
        let body = base64::engine::general_purpose::STANDARD.encode(point.as_ref());
        let raw =
            format!("-----BEGIN COSIGN PUBLIC KEY-----\n{body}\n-----END COSIGN PUBLIC KEY-----\n");
        let raw_key = TrustedKey::from_pem(&path, &raw).unwrap();
        assert_eq!(raw_key.fingerprint(), loaded.0[0].fingerprint());

        // garbage is refused with the path in the error
        let err = TrustedKey::from_pem(&path, "not a pem").unwrap_err();
        assert!(err.to_string().contains("cosign.pub"), "{err}");
    }

    #[test]
    fn annotations_ride_the_layer() {
        let tmp = tempfile::tempdir().unwrap();
        let layout = Layout::create(tmp.path()).unwrap();
        let (subject, index_digest) = test_subject();
        sign(&layout, &subject, &signing_key(9), "").unwrap();
        let found = referrer::find(&layout, &index_digest).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].artifact_type(), ARTIFACT_TYPE);
        assert_eq!(
            found[0].payload().unwrap().media_type().as_ref(),
            MEDIA_TYPE_PAYLOAD
        );
        assert!(found[0]
            .payload_annotations()
            .unwrap()
            .contains_key(SIGNATURE_ANNOTATION));
    }
}
