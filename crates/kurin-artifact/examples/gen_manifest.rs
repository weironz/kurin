//! Regenerate the embedded artifact manifest for a Kubernetes release.
//! Usage: cargo run -p kurin-artifact --example gen_manifest -- 1.37.2
use kurin_artifact::artifact::Arch;
use kurin_artifact::dynamic_manifest::k8s_manifest_for_arches;
use kurin_artifact::zone::Zone;
use semver::Version;

#[tokio::main]
async fn main() {
    let version = std::env::args()
        .nth(1)
        .expect("usage: gen_manifest <version>");
    let target = Version::parse(version.trim_start_matches('v')).expect("semver version");
    let manifest = k8s_manifest_for_arches(&target, &[Arch::Amd64, Arch::Arm64], Zone::Global)
        .await
        .expect("resolve manifest");
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).expect("encode")
    );
}
