//! TypeScript binding generation and its drift check (WP-55).
//!
//! `just ts-bindings` runs this test with `KURIN_TS_BINDINGS_WRITE=1` to
//! rewrite `web/src/api/generated/`. A plain `cargo test` exports into a
//! temporary directory and compares it with the committed files, so drift
//! fails the build instead of silently shipping stale types.

use std::fs;
use std::path::{Path, PathBuf};

use ts_rs::TS;

use crate::api::clusters::{
    ClusterDetail, ClusterList, ClusterView, ConfigView, CreateCluster, Imported, RefreshResponse,
    SaveConfig, Saved, SnapshotView,
};
use crate::api::operations::{
    EventsQuery, ListQuery, LogPageView, LogsQuery, OperationList, OperationView, SubmitBody,
};
use crate::api::wp54::{
    AddonOpBody, CatalogAddon, CatalogResponse, ClusterAddonsView, CreateBaseBody,
    ImportPackageBody, MetaResponse, NodeBody, OperationSubmitted, PackageList, PackageView,
    ParseNodesBody, ParsedNode, ParsedNodes, PlanQuery, PlanSubmitted, ProbeBody, ProbeFacts,
    ProbeResponse, PushBody, RenderBody, RenderedConfig, SshBody, ValidResponse, ValidateBody,
};
use crate::error::{Diagnostic, ErrorEnvelope, Label};
use crate::routes::{Healthz, LoginRequest, SessionInfo};
use crate::store::RevisionSummary;

/// Where the committed bindings live.
pub(crate) fn bindings_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/src/api/generated")
}

/// Export every DTO (and its dependencies) into `dir`.
fn export_all(dir: &Path) {
    fs::create_dir_all(dir).expect("create the bindings directory");
    macro_rules! export {
        ($($ty:ty),* $(,)?) => {
            $(
                <$ty as TS>::export_all_to(dir)
                    .unwrap_or_else(|error| panic!("export {}: {error}", stringify!($ty)));
            )*
        };
    }
    export!(
        // clusters
        CreateCluster,
        ClusterView,
        Imported,
        ClusterList,
        ClusterDetail,
        SnapshotView,
        RefreshResponse,
        ConfigView,
        SaveConfig,
        Saved,
        // operations
        SubmitBody,
        OperationView,
        ListQuery,
        OperationList,
        EventsQuery,
        LogsQuery,
        LogPageView,
        // wp54
        MetaResponse,
        RenderBody,
        SshBody,
        NodeBody,
        RenderedConfig,
        ValidateBody,
        ValidResponse,
        ParseNodesBody,
        ParsedNode,
        ParsedNodes,
        ProbeBody,
        ProbeFacts,
        ProbeResponse,
        CatalogAddon,
        CatalogResponse,
        ClusterAddonsView,
        AddonOpBody,
        PlanQuery,
        PlanSubmitted,
        PackageView,
        PackageList,
        ImportPackageBody,
        PushBody,
        CreateBaseBody,
        OperationSubmitted,
        // error envelope
        Label,
        Diagnostic,
        ErrorEnvelope,
        // session / health
        Healthz,
        LoginRequest,
        SessionInfo,
        // store views
        RevisionSummary,
        // event envelope (kurin-core)
        kurin_core::events::Envelope,
    );
}

/// Every `*.ts` file below `dir`, as `(relative path, contents)`.
fn read_tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|ext| ext == "ts") {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked path is below the root")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((relative, fs::read(&path).expect("read a generated binding")));
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_bindings() {
        let committed = bindings_dir();
        if std::env::var_os("KURIN_TS_BINDINGS_WRITE").is_some() {
            if committed.exists() {
                fs::remove_dir_all(&committed).expect("clear the old bindings");
            }
            export_all(&committed);
            return;
        }

        let scratch = tempfile::tempdir().expect("temporary directory");
        export_all(scratch.path());
        let expected = read_tree(scratch.path());
        let actual = read_tree(&committed);

        assert!(!expected.is_empty(), "no bindings were generated");
        if expected != actual {
            let expected_paths: Vec<&str> = expected.iter().map(|(p, _)| p.as_str()).collect();
            let actual_paths: Vec<&str> = actual.iter().map(|(p, _)| p.as_str()).collect();
            let changed: Vec<&str> = expected
                .iter()
                .zip(actual.iter())
                .filter(|(a, b)| a != b)
                .map(|(a, _)| a.0.as_str())
                .collect();
            panic!(
                "web/src/api/generated 与 DTO 不一致（漂移）。\n\
                 运行 `just ts-bindings` 重新生成。\n\
                 期望文件：{expected_paths:?}\n\
                 实际文件：{actual_paths:?}\n\
                 内容不同的文件：{changed:?}"
            );
        }
    }
}
