# Built-in addon catalog (WP-46)

One subdirectory per addon (03 §4.7):

```text
addons/<name>/
  addon.yaml           # spec: 03-addons.md §4.1 (docs/schema/addon.v1alpha1.json)
  values.yaml.j2       # values template (embedded) — helm addons only
  values.schema.json   # user-parameter JSON Schema (embedded)
  manifests/*.yaml.j2  # manifest templates (embedded) — manifests addons only
  README.md
```

The catalog is embedded at build time (only metadata and templates; chart
payloads and images travel in OCI addon packages or are pulled online —
see `crates/kurin-components/src/addon/source.rs`). Chart references are
upstream-absolute: an `oci://` reference where the project publishes its
chart to a registry, else the GitHub release URL of the chart archive
(zone = `cn` rewrites both through the mirror table). Chart versions and
image digests are pinned from upstream and re-verified by
`scripts/check-mirrors.sh`.

| addon | category | version | chart | install |
|---|---|---|---|---|
| `metrics-server` | monitoring | 0.9.0 | metrics-server 3.14.0 | helm |
| `local-path` | storage | 0.0.31 | — (upstream manifests) | manifests |
| `nfs-subdir` | storage | 4.0.2 | — (upstream manifests) | manifests |
| `ingress-nginx` | ingress | 1.15.1 | ingress-nginx 4.15.1 | helm |
| `cert-manager` | security | 1.21.3 | cert-manager v1.21.3 (OCI) | helm |
| `metallb` | networking | 0.16.1 | metallb 0.16.1 | helm |
| `headlamp` | dashboard | 0.46.0 | headlamp 0.46.0 | helm |
| `kube-prometheus-stack` | monitoring | 0.94.2 | kube-prometheus-stack 92.14.0 (OCI) | helm |

`local-path` and `nfs-subdir` replicate the resources of the first-layer
storage components (same StorageClasses `kurin-local-path` / `kurin-nfs`)
so WP-47's cutover does not recreate them; until then enabling those two
still routes to the first layer.

The `images` list of every addon carries **both** the upstream and the
`*.m.daocloud.io` form of each image with the same digest: the values
templates swap the image registries when `zone = "cn"`, and the
image-integrity gate (03 §4.2 rule 1) matches rendered references
exactly. Mirror parity is what `scripts/check-mirrors.sh` verifies.
