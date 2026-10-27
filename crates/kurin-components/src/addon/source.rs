//! File access for one addon, wherever it was loaded from (03 §4.7).
//!
//! Validation (and later rendering, WP-41) resolves the package-relative
//! paths in `addon.yaml` — `values.schema.json`, `values.yaml.j2`,
//! `charts/…tgz` — through [`AddonFiles`], so the same rules apply to a
//! local directory, an OCI addon package and the embedded built-in
//! catalog. The one intentional difference: the embedded catalog ships
//! metadata and templates only, so chart payloads are not expected to be
//! there (they come from a package or online, [`AddonFiles::carries_charts`]).

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use kurin_artifact::package::{PackageKind, PackageSource, MEDIA_TYPE_FILES};
use oci_spec::image::{Digest, ImageManifest};
use rust_embed::RustEmbed;

use crate::addon::error::AddonError;

/// The layer annotation carrying a file's package-relative path.
const TITLE_ANNOTATION: &str = "org.opencontainers.image.title";

/// Read access to one addon's files. Paths are package-relative
/// (`values.yaml.j2`, `charts/…`).
pub trait AddonFiles: std::fmt::Debug + Send + Sync {
    /// Human-readable origin for listings and diagnostics
    /// (`dir addons/metrics-server`).
    fn origin(&self) -> String;

    /// The diagnostics path for one file.
    fn path_for(&self, rel: &str) -> String;

    /// Whether chart payloads are carried. Directories and packages do;
    /// the embedded catalog ships metadata and templates only, its
    /// charts come from a package or online.
    fn carries_charts(&self) -> bool;

    /// Whether `rel` exists.
    fn exists(&self, rel: &str) -> bool;

    /// Every file in the addon, package-relative and sorted.
    fn list(&self) -> Vec<String>;

    /// Read one file.
    ///
    /// # Errors
    ///
    /// [`AddonError::Io`] when the file is missing or unreadable.
    fn read(&self, rel: &str) -> Result<Vec<u8>, AddonError>;
}

// ---------------------------------------------------------------------------
// directory source (03 §4.7: `source = "dir:///path/to/addon"`)
// ---------------------------------------------------------------------------

/// An addon loaded from a directory containing `addon.yaml`.
#[derive(Debug, Clone)]
pub struct DirFiles {
    root: PathBuf,
}

impl DirFiles {
    /// Wrap a directory; it must contain `addon.yaml` (checked by the
    /// loader, not here).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DirFiles { root: root.into() }
    }

    fn full(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

impl AddonFiles for DirFiles {
    fn origin(&self) -> String {
        format!("dir {}", self.root.display())
    }

    fn path_for(&self, rel: &str) -> String {
        self.full(rel).display().to_string()
    }

    fn carries_charts(&self) -> bool {
        true
    }

    fn exists(&self, rel: &str) -> bool {
        self.full(rel).is_file()
    }

    fn list(&self) -> Vec<String> {
        let mut out = Vec::new();
        walk(&self.root, "", &mut out);
        out.sort();
        out
    }

    fn read(&self, rel: &str) -> Result<Vec<u8>, AddonError> {
        fs::read(self.full(rel)).map_err(|e| AddonError::io(self.path_for(rel), e))
    }
}

/// Collect every file below `root`, as `/`-separated relative paths.
fn walk(root: &Path, rel: &str, out: &mut Vec<String>) {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk(root, &child, out);
        } else {
            out.push(child);
        }
    }
}

// ---------------------------------------------------------------------------
// package source (04 §3.4: kind `addon`)
// ---------------------------------------------------------------------------

/// An addon loaded from an OCI addon package: the file-set manifests'
/// `org.opencontainers.image.title`-annotated layers are the addon's
/// file tree, with every blob digest-verified on read.
pub struct PackageFiles {
    source: String,
    package: PackageSource,
    files: BTreeMap<String, Digest>,
}

impl std::fmt::Debug for PackageFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackageFiles")
            .field("source", &self.source)
            .field("files", &self.files.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl PackageFiles {
    /// Open `oci-layout://DIR`, `file://X.tar` or `file://X.tar.zst` as
    /// an addon package.
    ///
    /// # Errors
    ///
    /// [`AddonError::Io`] when the source cannot be opened or is not a
    /// package; [`AddonError::Invalid`] when the package is not an
    /// addon package or carries one file twice.
    pub fn open(source: &str) -> Result<Self, AddonError> {
        let package = PackageSource::open(source).map_err(|e| AddonError::io(source, e))?;
        let meta = package.manifest().map_err(|e| AddonError::io(source, e))?;
        if meta.kind != PackageKind::Addon {
            return Err(AddonError::invalid(
                source,
                format!("package kind is `{}`, expected `addon`", meta.kind.as_str()),
            ));
        }
        let files = read_file_map(&package, source)?;
        Ok(PackageFiles {
            source: source.to_string(),
            package,
            files,
        })
    }

    /// The file map: package-relative path → blob digest.
    pub fn files(&self) -> &BTreeMap<String, Digest> {
        &self.files
    }
}

/// Read every file-set manifest of the package into a path → digest map.
///
/// `index.json` points at the package index, whose own `manifests`
/// carry the metadata and one file-set manifest per architecture
/// (04 §3.1); the file-set layers are annotated with their
/// package-relative path.
fn read_file_map(
    package: &PackageSource,
    source: &str,
) -> Result<BTreeMap<String, Digest>, AddonError> {
    let root = package
        .layout()
        .read_index()
        .map_err(|e| AddonError::io(source, e))?;
    let Some(package_desc) = root.manifests().first() else {
        return Ok(BTreeMap::new());
    };
    let package_index =
        read_blob_json::<oci_spec::image::ImageIndex>(package, package_desc.digest(), source)?;
    let mut files = BTreeMap::new();
    for desc in package_index.manifests() {
        if desc.artifact_type().as_ref().map(|t| t.as_ref()) != Some(MEDIA_TYPE_FILES) {
            continue;
        }
        let manifest = read_blob_json::<ImageManifest>(package, desc.digest(), source)?;
        for layer in manifest.layers() {
            let Some(title) = layer
                .annotations()
                .as_ref()
                .and_then(|a| a.get(TITLE_ANNOTATION))
            else {
                continue;
            };
            if files
                .insert(title.clone(), layer.digest().clone())
                .is_some()
            {
                return Err(AddonError::invalid(
                    source,
                    format!("package carries file `{title}` twice"),
                ));
            }
        }
    }
    Ok(files)
}

/// Read one blob of the package as JSON.
fn read_blob_json<T: serde::de::DeserializeOwned>(
    package: &PackageSource,
    digest: &Digest,
    source: &str,
) -> Result<T, AddonError> {
    let mut bytes = Vec::new();
    package
        .layout()
        .blobs()
        .open_blob(digest)
        .map_err(|e| AddonError::io(source, e))?
        .read_to_end(&mut bytes)
        .map_err(|e| AddonError::io(source, e))?;
    serde_json::from_slice(&bytes).map_err(|e| AddonError::io(source, e))
}

impl AddonFiles for PackageFiles {
    fn origin(&self) -> String {
        format!("package {source}", source = self.source)
    }

    fn path_for(&self, rel: &str) -> String {
        format!("{source}!{rel}", source = self.source)
    }

    fn carries_charts(&self) -> bool {
        true
    }

    fn exists(&self, rel: &str) -> bool {
        self.files.contains_key(rel)
    }

    fn list(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    fn read(&self, rel: &str) -> Result<Vec<u8>, AddonError> {
        let digest = self
            .files
            .get(rel)
            .ok_or_else(|| AddonError::io(self.path_for(rel), "file not found in package"))?;
        let mut bytes = Vec::new();
        self.package
            .layout()
            .blobs()
            .open_blob(digest)
            .map_err(|e| AddonError::io(self.path_for(rel), e))?
            .read_to_end(&mut bytes)
            .map_err(|e| AddonError::io(self.path_for(rel), e))?;
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// embedded source (03 §4.7: the built-in catalog, filled by WP-46)
// ---------------------------------------------------------------------------

/// The built-in addon catalog at the repository root (`addons/<name>/…`),
/// embedded at build time. Only metadata and templates are embedded;
/// charts and images travel in packages or are pulled online.
#[derive(RustEmbed)]
#[folder = "../../addons"]
pub struct BuiltinCatalog;

/// File access for one addon of an embedded catalog.
pub struct EmbeddedFiles<E: RustEmbed> {
    name: String,
    // A fn pointer keeps `E` without inheriting its auto traits: the
    // embed is a stateless unit type, but the compiler cannot know.
    _embed: PhantomData<fn() -> E>,
}

impl<E: RustEmbed> std::fmt::Debug for EmbeddedFiles<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedFiles")
            .field("name", &self.name)
            .finish()
    }
}

impl<E: RustEmbed> EmbeddedFiles<E> {
    /// Wrap the `addons/<name>/` subtree of an embed.
    pub fn new(name: impl Into<String>) -> Self {
        EmbeddedFiles {
            name: name.into(),
            _embed: PhantomData,
        }
    }

    fn key(&self, rel: &str) -> String {
        format!("{name}/{rel}", name = self.name)
    }
}

impl<E: RustEmbed> AddonFiles for EmbeddedFiles<E> {
    fn origin(&self) -> String {
        format!("builtin {name}", name = self.name)
    }

    fn path_for(&self, rel: &str) -> String {
        format!("addons/{name}/{rel}", name = self.name)
    }

    fn carries_charts(&self) -> bool {
        false
    }

    fn exists(&self, rel: &str) -> bool {
        E::get(&self.key(rel)).is_some()
    }

    fn list(&self) -> Vec<String> {
        let prefix = format!("{name}/", name = self.name);
        E::iter()
            .filter(|p| p.starts_with(&prefix))
            .map(|p| p[prefix.len()..].to_string())
            .collect()
    }

    fn read(&self, rel: &str) -> Result<Vec<u8>, AddonError> {
        E::get(&self.key(rel))
            .map(|f| f.data.into_owned())
            .ok_or_else(|| AddonError::io(self.path_for(rel), "file not found in catalog"))
    }
}

// ---------------------------------------------------------------------------
// glob matching (for spec.install.manifests)
// ---------------------------------------------------------------------------

/// Match a package-relative path against a glob: `*` matches any run of
/// non-`/` characters, `?` one non-`/` character, `**` any run at all.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_impl(&pattern, &text)
}

fn glob_impl(pattern: &[char], text: &[char]) -> bool {
    match (pattern.first(), text.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some('*'), _) if pattern.get(1) == Some(&'*') => {
            // `**` crosses segments: skip it, or consume one character.
            glob_impl(&pattern[2..], text)
                || text.first().is_some_and(|_| glob_impl(pattern, &text[1..]))
        }
        (Some('*'), _) => {
            glob_impl(&pattern[1..], text)
                || text
                    .first()
                    .is_some_and(|c| *c != '/' && glob_impl(pattern, &text[1..]))
        }
        (Some('?'), Some(c)) => *c != '/' && glob_impl(&pattern[1..], &text[1..]),
        (Some(p), Some(c)) => *p == *c && glob_impl(&pattern[1..], &text[1..]),
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_within_one_segment() {
        assert!(glob_match("manifests/*.yaml.j2", "manifests/svc.yaml.j2"));
        assert!(!glob_match("manifests/*.yaml.j2", "manifests/a/b.yaml.j2"));
        assert!(!glob_match("*.yaml", "a/b.yaml"));
        assert!(glob_match("*.yaml", "b.yaml"));
        assert!(glob_match("manifests/?.yaml", "manifests/a.yaml"));
        assert!(!glob_match("manifests/?.yaml", "manifests/ab.yaml"));
    }

    #[test]
    fn double_star_crosses_segments() {
        assert!(glob_match("manifests/**", "manifests/anything/at/all"));
        assert!(glob_match("manifests/**.yaml", "manifests/a/b.yaml"));
        assert!(!glob_match("charts/*.tgz", "charts/sub/chart.tgz"));
    }

    #[test]
    fn dir_files_lists_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("demo");
        fs::create_dir_all(root.join("manifests")).unwrap();
        fs::write(
            root.join("addon.yaml"),
            "apiVersion: addons.kurin.dev/v1alpha1",
        )
        .unwrap();
        fs::write(root.join("manifests/svc.yaml.j2"), "kind: Service").unwrap();

        let files = DirFiles::new(&root);
        assert_eq!(
            files.list(),
            vec![
                "addon.yaml".to_string(),
                "manifests/svc.yaml.j2".to_string()
            ]
        );
        assert!(files.exists("manifests/svc.yaml.j2"));
        assert!(!files.exists("values.yaml.j2"));
        assert_eq!(
            files.read("addon.yaml").unwrap(),
            b"apiVersion: addons.kurin.dev/v1alpha1"
        );
        assert_eq!(files.origin(), format!("dir {}", root.display()));
    }
}
