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
//! - [`Addon::render`] (WP-41) merges values, validates them against the
//!   addon's schema, resolves secret references and renders manifest
//!   templates.
//! - [`apply::apply_addon`] (WP-42) executes the rendered addon: Helm
//!   through the pinned binary on the anchor control plane (render,
//!   image check, `upgrade --install`), or server-side apply of the
//!   rendered manifests. Both report the object list the readiness
//!   tracker follows.
//! - [`engine`] (WP-43) is the `addon-engine` first-layer component:
//!   the selection/dependency validation, the `addons/<name>` install
//!   and remove tasks, and the addon operation plans.
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

pub mod apply;
pub mod engine;
pub mod error;
pub mod load;
pub mod render;
pub mod schema;
pub mod source;
pub mod spec;
pub mod validate;

pub use apply::{
    apply_addon, release_status, uninstall_addon, AddonApply, ApplyRequest, HelmPayload,
};
pub use engine::{
    build_addon_plan, resolve_selection, AddonEngine, AddonInstallTask, AddonRecord,
    AddonRemoveTask, RecordedObject, SelectedAddon,
};

pub use error::AddonError;
pub use load::{embedded_from, Addon, Catalog};
pub use render::{RenderRequest, RenderedAddon, RenderedManifest, RenderedValues};
pub use schema::{schema, schema_json};
pub use source::{AddonFiles, BuiltinCatalog, DirFiles, EmbeddedFiles, PackageFiles};
pub use spec::{
    AddonManifest, AddonVersion, ApiVersion, ApplyAction, Arch, Category, Compat, DeleteAction,
    HelmInstall, HookAction, Hooks, Install, Kind, LabelNodesAction, Metadata, ObjectRef,
    SemverRange, Spec, Timeout, Uninstall, WaitAction, ADDON_FILE,
};
