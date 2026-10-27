//! Fail early when the `ui` build has no frontend to embed (WP-55, UI §4.2).
//!
//! `rust-embed` would otherwise fail with an opaque "folder does not exist"
//! error from the derive macro. `kurin`'s `ui` feature enables
//! `kurin-server/embed-ui`; plain `cargo build`/`cargo test` leave it off and
//! never need Node or `web/dist`.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../web/dist");

    if std::env::var_os("CARGO_FEATURE_EMBED_UI").is_none() {
        return;
    }

    let dist = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"))
        .join("../../web/dist");
    let entry = dist.join("index.html.gz");
    if !entry.is_file() {
        panic!(
            "\n\nweb/dist/index.html.gz 不存在：`--features ui` 需要先构建前端。\n\
             请运行 `just web-build`（或 `pnpm --dir web install && pnpm --dir web build`），\n\
             然后重新构建。预期产物目录：{}\n\n",
            dist.display()
        );
    }
}
