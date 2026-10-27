# headlamp

Kubernetes SIG 官方孵化的 [Headlamp](https://headlamp.dev)：Web 控制台，
可只读浏览集群状态、日志与事件。

- 版本 / version: **0.46.0**（chart `headlamp` 0.46.0，
  https://kubernetes-sigs.github.io/headlamp/）
- 安装 / install: `kurin addon install headlamp -f cluster.toml`
- 命名空间 / namespace: `headlamp`
- 镜像 / image: `ghcr.io/headlamp-k8s/headlamp:v0.46.0`
  （`zone = "cn"` 时经 `ghcr.m.daocloud.io` 拉取）

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `replicaCount` | `1` | 副本数 / number of replicas |
| `service.type` | `ClusterIP` | `NodePort` 可从集群外访问 / NodePort for off-cluster access |

```toml
[addons.headlamp]
enabled = true
values = { service = { type = "NodePort" } }
```

访问 / access：`kubectl -n headlamp port-forward svc/headlamp 8080:80`
（或 NodePort），页面创建 token 后登录。

## 就绪 / readiness

`Deployment/headlamp` 可用。

## 依赖 / dependencies

无（`requires: []`）。提供 `provides: [dashboard]`。
