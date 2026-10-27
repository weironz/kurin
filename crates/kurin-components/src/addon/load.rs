//! Loading addons (WP-40): from a directory, an OCI addon package, or the
//! embedded built-in catalog (03 §4.7).
//!
//! Loading parses and hands out file access; it does not judge the
//! content — that is [`crate::addon::validate`], so a caller can report
//! every rule violation at once instead of only the first.

use std::path::Path;

use rust_embed::RustEmbed;

use crate::addon::error::AddonError;
use crate::addon::source::{AddonFiles, BuiltinCatalog, DirFiles, EmbeddedFiles, PackageFiles};
use crate::addon::spec::{self, AddonManifest, ADDON_FILE};

/// One addon: its parsed `addon.yaml` plus access to its files.
pub struct Addon {
    manifest: AddonManifest,
    files: Box<dyn AddonFiles>,
}

impl std::fmt::Debug for Addon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Addon")
            .field("name", &self.manifest.metadata.name)
            .field("origin", &self.files.origin())
            .finish()
    }
}

impl Addon {
    /// The parsed `addon.yaml`.
    pub fn manifest(&self) -> &AddonManifest {
        &self.manifest
    }

    /// `metadata.name`.
    pub fn name(&self) -> &str {
        &self.manifest.metadata.name
    }

    /// Where this addon was loaded from (`dir …`, `package …`,
    /// `builtin …`).
    pub fn origin(&self) -> String {
        self.files.origin()
    }

    /// File access for the addon's payloads.
    pub fn files(&self) -> &dyn AddonFiles {
        self.files.as_ref()
    }

    /// Load one addon from a directory containing `addon.yaml`
    /// (03 §4.7, `source = "dir:///path/to/addon"`).
    ///
    /// # Errors
    ///
    /// [`AddonError::Io`] when the directory or `addon.yaml` cannot be
    /// read; [`AddonError::Parse`] when the document does not parse.
    pub fn from_dir(dir: &Path) -> Result<Self, AddonError> {
        if !dir.is_dir() {
            return Err(AddonError::io(dir.display().to_string(), "not a directory"));
        }
        let files = DirFiles::new(dir);
        Self::from_files(files)
    }

    /// Load one addon from an OCI addon package (04 §3.4, kind `addon`):
    /// `oci-layout://DIR`, `file://X.tar` or `file://X.tar.zst`.
    ///
    /// # Errors
    ///
    /// [`AddonError::Io`] when the package cannot be opened or read;
    /// [`AddonError::Invalid`] when it is not an addon package;
    /// [`AddonError::Parse`] when its `addon.yaml` does not parse.
    pub fn from_package(source: &str) -> Result<Self, AddonError> {
        let files = PackageFiles::open(source)?;
        Self::from_files(files)
    }

    /// Load from a source string: `dir:///path` (or a bare path) loads a
    /// directory, anything else an OCI package.
    ///
    /// # Errors
    ///
    /// As [`Addon::from_dir`] or [`Addon::from_package`].
    pub fn from_source(source: &str) -> Result<Self, AddonError> {
        let source = source.trim();
        if let Some(dir) = source.strip_prefix("dir://") {
            return Self::from_dir(Path::new(dir));
        }
        if source.starts_with("oci://") {
            return Err(AddonError::io(
                source,
                "registry sources need `kurin package pull` first (WP-33); \
                 use oci-layout:// or file://",
            ));
        }
        if source.starts_with("oci-layout://") || source.starts_with("file://") {
            return Self::from_package(source);
        }
        if source.contains("://") {
            return Err(AddonError::io(source, "unknown source scheme"));
        }
        // A bare path is a directory, unless it names a package file.
        if source.ends_with(".tar") || source.ends_with(".tar.zst") {
            return Self::from_package(source);
        }
        Self::from_dir(Path::new(source))
    }

    fn from_files(files: impl AddonFiles + 'static) -> Result<Self, AddonError> {
        let path = files.path_for(ADDON_FILE);
        let text =
            String::from_utf8(files.read(ADDON_FILE)?).map_err(|e| AddonError::io(&path, e))?;
        let manifest = spec::parse(&text).map_err(|e| AddonError::parse(&path, &text, e))?;
        Ok(Addon {
            manifest,
            files: Box::new(files),
        })
    }
}

/// The set of addons an install resolves against: the built-in catalog
/// plus everything configured from packages and local directories
/// (03 §4.5).
pub struct Catalog {
    addons: Vec<Addon>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.addons.iter().map(|a| a.name()))
            .finish()
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Catalog::new()
    }
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Catalog { addons: Vec::new() }
    }

    /// The built-in catalog embedded at build time (`addons/<name>/…`).
    /// Empty until WP-46 lands the first addons.
    ///
    /// # Errors
    ///
    /// [`AddonError`] when an embedded `addon.yaml` cannot be read or
    /// parsed — a defect of the kurin build, not of user input.
    pub fn embedded() -> Result<Self, AddonError> {
        embedded_from::<BuiltinCatalog>()
    }

    /// A catalog directory: every subdirectory is one addon
    /// (`addons/<name>/addon.yaml`); plain files are ignored.
    ///
    /// # Errors
    ///
    /// As [`Addon::from_dir`] per subdirectory; a subdirectory without
    /// `addon.yaml` is an error, not a skip.
    pub fn from_dir(root: &Path) -> Result<Self, AddonError> {
        let mut catalog = Catalog::new();
        let entries =
            std::fs::read_dir(root).map_err(|e| AddonError::io(root.display().to_string(), e))?;
        let mut dirs: Vec<_> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
        dirs.sort();
        for dir in dirs {
            catalog.push(Addon::from_dir(&dir)?);
        }
        Ok(catalog)
    }

    /// Add one addon (from a package or directory) to the set.
    pub fn push(&mut self, addon: Addon) {
        self.addons.push(addon);
    }

    /// Every addon, in insertion order.
    pub fn addons(&self) -> &[Addon] {
        &self.addons
    }

    /// Look up one addon by name.
    pub fn get(&self, name: &str) -> Option<&Addon> {
        self.addons.iter().find(|a| a.name() == name)
    }

    /// Every addon name, in insertion order.
    pub fn names(&self) -> Vec<&str> {
        self.addons.iter().map(|a| a.name()).collect()
    }
}

/// Scan an embed of `addons/<name>/…` subtrees into a catalog. Public so
/// alternative embeds (tests, curated sets) get the same scan rules.
///
/// # Errors
///
/// [`AddonError`] when an embedded `addon.yaml` cannot be read or
/// parsed.
pub fn embedded_from<E: RustEmbed + 'static>() -> Result<Catalog, AddonError> {
    let mut names: Vec<String> = E::iter()
        .filter_map(|path| {
            let (dir, file) = path.rsplit_once('/')?;
            // One level only: `addons/<name>/addon.yaml`.
            (file == ADDON_FILE && !dir.contains('/')).then(|| dir.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    let mut catalog = Catalog::new();
    for name in names {
        let files = EmbeddedFiles::<E>::new(&name);
        catalog.push(Addon::from_files(files)?);
    }
    Ok(catalog)
}
