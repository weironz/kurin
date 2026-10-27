# metrics-server

资源指标 API（`metrics.k8s.io`）：`kubectl top` 与 HPA 的依赖，
Kubernetes 默认不部署它。

- 版本 / version: **0.9.0**（chart `metrics-server` 3.14.0，
  https://kubernetes-sigs.github.io/metrics-server/）
- 安装 / install: `kurin addon install metrics-server -f cluster.toml`
- 命名空间 / namespace: `kube-system`
- 镜像 / image: `registry.k8s.io/metrics-server/metrics-server:v0.9.0`
  （`zone = "cn"` 时经 `k8s.m.daocloud.io` 拉取）

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `replicas` | `1` | 副本数 / number of replicas |

```toml
[addons.metrics-server]
enabled = true
values = { replicas = 2 }
```

## 就绪 / readiness

`Deployment/metrics-server` 可用，且 hook 等待
`APIService/v1beta1.metrics.k8s.io` 变为 `Available`——安装完成即代表
`kubectl top nodes` 可用。

## 依赖 / dependencies

无（`requires: []`）。提供 `provides: [metrics-api]` 供后续 addon
声明依赖（例如依赖指标 API 的自动扩缩组件）。
