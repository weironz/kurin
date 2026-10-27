//! OCI package support: image layout reading/writing and archive
//! transport.

pub mod layout;
pub mod remote;

pub use layout::{Layout, LayoutError, LAYOUT_VERSION, REF_NAME_ANNOTATION};
pub use remote::{
    is_manifest_media_type, resolve_registry_auth, RegistryRef, MANIFEST_MEDIA_TYPES,
};
