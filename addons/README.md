# Built-in addon catalog

One subdirectory per addon (WP-46 will populate the first batch:
`metrics-server`, `local-path`, `nfs-subdir`, `ingress-nginx`,
`cert-manager`, `metallb`, `headlamp`, `kube-prometheus-stack`):

```text
addons/<name>/
  addon.yaml           # spec: 03-addons.md §4.1 (docs/schema/addon.v1alpha1.json)
  values.yaml.j2       # values template (embedded)
  values.schema.json   # user-parameter JSON Schema (embedded)
  README.md
```

The catalog is embedded at build time (only metadata and templates; chart
payloads and images travel in OCI addon packages or are pulled online —
see `crates/kurin-components/src/addon/source.rs`).
