# ingress-nginx

Kubernetes 官方维护的 [NGINX Ingress 控制器](https://github.com/kubernetes/ingress-nginx)：
把集群内的服务经 Ingress 规则暴露为七层入口。

- 版本 / version: **1.15.1**（chart `ingress-nginx` 4.15.1，
  https://kubernetes.github.io/ingress-nginx/）
- 安装 / install: `kurin addon install ingress-nginx -f cluster.toml`
- 命名空间 / namespace: `ingress-nginx`
- 镜像 / images: `registry.k8s.io/ingress-nginx/controller:v1.15.1`、
  `registry.k8s.io/ingress-nginx/kube-webhook-certgen:v1.6.9`
  （`zone = "cn"` 时经 `k8s.m.daocloud.io` 拉取；chart 自带镜像 digest，
  镜像端与上游一致）

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `controller.replicas` | `1` | 控制器副本数 / controller replicas |
| `controller.service.type` | `ClusterIP` | `NodePort` / `LoadBalancer`（配合 metallb）|

```toml
[addons.ingress-nginx]
enabled = true
values = { controller = { replicas = 2, service = { type = "NodePort" } } }
```

## 就绪 / readiness

`Deployment/ingress-nginx-controller` 可用；admission webhook 的
patch Job 由 chart 的 helm hook 完成。

## 依赖 / dependencies

无硬依赖（`requires: []`）。与 `cert-manager` 组合可实现 Ingress 的
自动化 TLS 证书（`ingressClassName: nginx` + cert-manager 签发的
secret），两者互不依赖、可单独安装。提供 `provides: [ingress]`。
