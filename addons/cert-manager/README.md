# cert-manager

[cert-manager](https://cert-manager.io)：X.509 证书的签发与自动续期，
支持 ACME（Let's Encrypt 等）、CA、自签等签发者。

- 版本 / version: **1.21.3**（chart 同版本，以 OCI 形式发布：
  `oci://quay.io/jetstack/charts/cert-manager:v1.21.3`）
- 安装 / install: `kurin addon install cert-manager -f cluster.toml`
- 命名空间 / namespace: `cert-manager`
- CRD：随 release 安装（`crds.enabled: true`，上游推荐），卸载时因
  `crds.keep` 资源策略保留，避免删掉用户的 Issuer/Certificate 数据
- 镜像 / images: `quay.io/jetstack/cert-manager-{controller,webhook,cainjector,startupapicheck}:v1.21.3`
  （`zone = "cn"` 时经 `quay.m.daocloud.io` 拉取）

## 常用参数 / common parameters

无（安装即用）。确需调优（如 `replicas`、资源限制、service monitor）
使用未校验的 `raw-values` 透传。

## 依赖关系 / dependencies

- `requires: []`：cert-manager 是叶子组件，不依赖任何 addon。
- `provides: [certificates]`：供其他 addon 声明依赖。首批目录中**没有**
  addon 硬依赖它——`ingress-nginx` 与它是**组合关系**而非依赖关系：
  Ingress 的 `tls` 段引用 cert-manager 签发的 secret 即可获得自动化
  TLS，两者都可以独立安装、独立卸载。
- 卸载顺序：依赖它签发证书的 workload 不受影响（secret 仍在），但证书
  不再自动续期——先移除依赖方再卸载 cert-manager 是稳妥顺序。

## 就绪 / readiness

`Deployment/cert-manager`、`Deployment/cert-manager-webhook`、
`Deployment/cert-manager-cainjector` 全部可用；chart 自带的
`startupapicheck` Job（helm hook）在安装期验证 webhook 可达。

## 验证 / verifying

```bash
kubectl apply -f - <<'EOF'
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: selfsigned-kurin
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: kurin-e2e
  namespace: default
spec:
  secretName: kurin-e2e-tls
  duration: 24h
  dnsNames: [kurin-e2e.local]
  issuerRef:
    name: selfsigned-kurin
    kind: ClusterIssuer
EOF
kubectl wait --for=condition=Ready certificate/kurin-e2e -n default --timeout=120s
```
