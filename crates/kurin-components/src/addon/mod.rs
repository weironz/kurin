//! Declarative addons (WP-40, 03 §4): the `addon.yaml` types, loading
//! from a directory / OCI package / the embedded built-in catalog, and
//! the static validation rules every addon passes before it can be
//! planned, packaged or offered.
//!
//! What lives here and what does not:
//!
//! - Parsing and file access are pure: no cluster, no network.
//! - [`Addon::validate`] checks every rule of 03 §4.2 that one addon can
//!   violate alone; [`Catalog::validate`] adds the cross-addon rules.
//! - Rendering (WP-41), helm execution and applying (WP-42), the
//!   `addon-engine` component and install-time compat matching against a
//!   real cluster (WP-43) are deliberately out of scope.
//!
//! ```no_run
//! use kurin_components::addon::{Addon, Catalog};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let mut catalog = Catalog::embedded()?;      // built-ins (WP-46)
//! catalog.push(Addon::from_source("dir:///srv/addons/corp-logging")?);
//! catalog.push(Addon::from_source("oci-layout:///srv/packages/corp-logging")?);
//! let diagnostics = catalog.validate();        // empty = every addon is valid
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod load;
pub mod schema;
pub mod source;
pub mod spec;
pub mod validate;

pub use error::AddonError;
pub use load::{embedded_from, Addon, Catalog};
pub use schema::{schema, schema_json};
pub use source::{AddonFiles, BuiltinCatalog, DirFiles, EmbeddedFiles, PackageFiles};
pub use spec::{
    AddonManifest, AddonVersion, ApiVersion, ApplyAction, Arch, Category, Compat, DeleteAction,
    HelmInstall, HookAction, Hooks, Install, Kind, LabelNodesAction, Metadata, ObjectRef,
    SemverRange, Spec, Timeout, Uninstall, WaitAction, ADDON_FILE,
};
