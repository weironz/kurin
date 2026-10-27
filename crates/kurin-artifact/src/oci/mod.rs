//! OCI package support: image layout reading/writing and archive
//! transport.

pub mod layout;

pub use layout::{Layout, LayoutError, LAYOUT_VERSION, REF_NAME_ANNOTATION};
