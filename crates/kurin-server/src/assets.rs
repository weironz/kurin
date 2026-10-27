//! Embedded SPA assets (WP-55, UI §4.2/§9).
//!
//! Only the `.gz` files produced by `just web-build` are embedded: the
//! response carries `Content-Encoding: gzip`, so the server never compresses
//! at request time (UI §9). The SPA fallback serves `index.html.gz` for
//! client-side routes; unknown `/api` paths keep the JSON 404 envelope, so a
//! typo in an API call never returns HTML.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method, Uri};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;
use crate::state::AppState;

/// The embedded (or, in tests, in-memory) asset set keyed by the path
/// relative to `web/dist` — including the `.gz` suffix.
#[derive(Clone, Default)]
pub struct StaticAssets {
    files: Arc<HashMap<String, Cow<'static, [u8]>>>,
}

impl StaticAssets {
    /// An asset set from `(path, bytes)` pairs. Used by tests and by the
    /// non-`embed-ui` build, which serves no frontend at all.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<Vec<u8>>,
    {
        let files = pairs
            .into_iter()
            .map(|(path, bytes)| (path.into(), Cow::Owned(bytes.into())))
            .collect();
        Self {
            files: Arc::new(files),
        }
    }

    /// All embedded assets, when this build has `--features ui`.
    pub fn embedded() -> Self {
        #[cfg(feature = "embed-ui")]
        {
            let mut files = HashMap::new();
            for path in WebDist::iter() {
                if let Some(file) = WebDist::get(&path) {
                    files.insert(path.into_owned(), file.data);
                }
            }
            Self {
                files: Arc::new(files),
            }
        }
        #[cfg(not(feature = "embed-ui"))]
        {
            Self::default()
        }
    }

    /// Look one asset up by its `web/dist` relative path.
    pub fn get(&self, path: &str) -> Option<&Cow<'static, [u8]>> {
        self.files.get(path)
    }

    /// Number of embedded files (used by the startup log and tests).
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// True when no frontend was embedded.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// The Vite output. `#[include]` keeps only the gzip copies; the build
/// script fails early when `web/dist` is missing (UI §4.2).
#[cfg(feature = "embed-ui")]
#[derive(rust_embed::Embed)]
#[folder = "../../web/dist/"]
#[include = "**/*.gz"]
struct WebDist;

/// Router fallback: API paths get the JSON envelope, everything else is a
/// static file or the SPA entry point.
pub async fn fallback(State(state): State<AppState>, method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") {
        return ApiError::NotFound.into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return ApiError::NotFound.into_response();
    }
    serve(&state.assets(), path)
}

/// Resolve `path` against `assets` and build the response.
///
/// Resolution order: exact file, then `index.html.gz` for extension-less
/// paths (client-side routes). A missing file with an extension is a 404,
/// never the SPA entry point.
pub fn serve(assets: &StaticAssets, path: &str) -> Response {
    let relative = path.trim_start_matches('/');
    if relative.split('/').any(|segment| segment == "..") {
        return ApiError::NotFound.into_response();
    }
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let key = format!("{relative}.gz");
    if let Some(data) = assets.get(&key) {
        return asset_response(relative, data.as_ref());
    }
    let has_extension = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains('.'));
    if !has_extension {
        if let Some(data) = assets.get("index.html.gz") {
            return asset_response("index.html", data.as_ref());
        }
    }
    ApiError::NotFound.into_response()
}

/// A gzip asset response with its original media type and a cache policy:
/// hashed Vite bundles are immutable, the entry point is revalidated.
fn asset_response(original: &str, data: &[u8]) -> Response {
    let mime = mime_guess::from_path(original).first_or_octet_stream();
    let cache = if original == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    let mut response = Response::new(Body::from(data.to_vec()));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref()).expect("mime types are valid header values"),
    );
    headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    fn assets() -> StaticAssets {
        StaticAssets::from_pairs([
            ("index.html.gz", b"<html>spa</html>".to_vec()),
            ("assets/app-abc.js.gz", b"console.log(1)".to_vec()),
            ("assets/app-abc.css.gz", b"body{}".to_vec()),
        ])
    }

    async fn body(response: Response) -> Vec<u8> {
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("in-memory body")
            .to_vec()
    }

    #[tokio::test]
    async fn serves_gzip_assets_with_their_original_media_type() {
        let response = serve(&assets(), "/assets/app-abc.js");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/javascript");
        assert_eq!(body(response).await, b"console.log(1)");

        let response = serve(&assets(), "/assets/app-abc.css");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/css");
    }

    #[tokio::test]
    async fn the_root_and_client_routes_serve_the_spa_entry_point() {
        for path in ["/", "/clusters", "/clusters/demo/operations"] {
            let response = serve(&assets(), path);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
            assert_eq!(body(response).await, b"<html>spa</html>", "{path}");
        }
    }

    #[tokio::test]
    async fn a_missing_asset_is_not_masked_by_the_spa_fallback() {
        let response = serve(&assets(), "/assets/missing.js");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn path_traversal_is_rejected() {
        let response = serve(&assets(), "/../secret");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
