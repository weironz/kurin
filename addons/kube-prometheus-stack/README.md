# kube-prometheus-stack

prometheus-community 的 [kube-prometheus-stack](https://github.com/prometheus-community/helm-charts/tree/main/charts/kube-prometheus-stack)：
Prometheus Operator、Prometheus、Alertmanager、Grafana、
node-exporter、kube-state-metrics 与一组默认的告警/聚合规则。

- 版本 / version: **0.94.2**（operator；chart 92.14.0，以 OCI 形式发布：
  `oci://ghcr.io/prometheus-community/charts/kube-prometheus-stack:92.14.0`）
- 安装 / install: `kurin addon install kube-prometheus-stack -f cluster.toml`
  （镜像多、启动慢，单节点集群建议 ≥ 4 GB 内存）
- 命名空间 / namespace: `monitoring`
- 镜像 / images：prometheus、alertmanager、prometheus-operator、
  node-exporter、kube-state-metrics（registry.k8s.io）、grafana 与
  k8s-sidecar（docker.io/quay.io）等 9 个
  （`zone = "cn"` 时全部经 `*.m.daocloud.io` 拉取）

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `grafana.adminPassword` | `prom-operator`（chart 默认） | Grafana 管理员密码，支持秘密引用 / Grafana admin password; secret references work |

```toml
[addons.kube-prometheus-stack]
enabled = true
values = { grafana = { adminPassword = { env = "GRAFANA_PW" } } }
```

更多调优（保留期、存储、ingress、ServiceMonitor 白名单等）用未校验的
`raw-values` 透传。

## 就绪 / readiness

`Deployment/kube-prometheus-stack-operator`、`...-grafana`、
`...-kube-state-metrics` 与 `DaemonSet/...-prometheus-node-exporter`
可用。Prometheus 与 Alertmanager 的 StatefulSet 由 operator 在就绪后
异步拉起——本 addon 的就绪门槛是 operator 与采集链路就绪，两个
StatefulSet 随后自行收敛（e2e 里另行断言）。

## 依赖 / dependencies

无硬依赖（`requires: []`）。`metrics-server` 与本 addon 是互补关系
（前者是资源指标 API，后者是监控栈）。提供
`provides: [monitoring]`。

## 卸载 / uninstall

CRD 声明为 `uninstall.keep`（chart 自身也以 resource-policy 保留
Prometheus/Alertmanager 等 CRD），避免删除已有的
ServiceMonitor/PrometheusRule 数据。
