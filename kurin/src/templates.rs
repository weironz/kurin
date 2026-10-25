//! In-memory template rendering. Templates live in `kurin/templates/` and are
//! embedded at compile time; no YAML/shell is ever assembled with `format!`.
//!
//! MiniJinja syntax keeps Kubernetes version drift in template files.

use std::sync::LazyLock;

use minijinja::{context, Environment};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "templates"]
struct Templates;

static ENV: LazyLock<Environment<'static>> = LazyLock::new(|| {
    let mut env = Environment::new();
    for name in Templates::iter() {
        let name = name.to_string();
        if let Some(file) = Templates::get(&name) {
            let src = String::from_utf8_lossy(&file.data).into_owned();
            env.add_template_owned(name, src)
                .expect("embedded template parses");
        }
    }
    env
});

/// Render a template by relative path (e.g. `containerd/config.toml.j2`).
pub fn render(path: &str, ctx: minijinja::Value) -> Result<String, minijinja::Error> {
    ENV.get_template(path)?.render(ctx)
}

/// Convenience for the common cluster-level context.
pub fn render_cluster(
    path: &str,
    cluster: &kurin_config::Cluster,
) -> Result<String, minijinja::Error> {
    render(
        path,
        context! {
            cluster => cluster.name,
            kubernetes_version => cluster.kubernetes_version.to_string(),
            pod_cidr => cluster.pod_cidr.to_string(),
            service_cidr => cluster.service_cidr.to_string(),
            dns_domain => cluster.dns_domain,
            vip => cluster.vip.map(|v| v.to_string()),
        },
    )
}

pub fn names() -> Vec<String> {
    Templates::iter().map(|s| s.to_string()).collect()
}
