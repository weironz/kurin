# metallb

[MetalLB](https://metallb.io)：裸金属集群的 LoadBalancer 实现，为
`type: LoadBalancer` 的 Service 分配外部 IP。默认启用 L2 模式
（ARP/NDP），BGP 场景自带 frr-k8s 后端。

- 版本 / version: **0.16.1**（chart `metallb` 0.16.1，
  https://metallb.github.io/metallb）
- 安装 / install: `kurin addon install metallb -f cluster.toml`
- 命名空间 / namespace: `metallb-system`
- 镜像 / images: `quay.io/metallb/{controller,speaker,frr-k8s}`、
  `quay.io/frrouting/frr:10.4.3`（`zone = "cn"` 时经
  `quay.m.daocloud.io` 拉取）

## L2 配置 / L2 configuration

MetalLB 安装后不会自动分配任何地址——必须声明地址池。本 addon 把
L2 配置做成了参数：设置 `addresses` 后，安装的最后一步（内建
`apply` hook）会在 `metallb-system` 里创建：

- `IPAddressPool/kurin`：来自 `addresses` 的 CIDR 或区间列表；
- `L2Advertisement/kurin`：把该池通告到节点所在的二层网段。

```toml
[addons.metallb]
enabled = true
values = { addresses = ["192.168.10.100-192.168.10.120"] }
```

地址池必须与节点同网段（L2 模式靠 ARP 应答，IP 必须能被路由到节点
网卡）。多网卡/复杂拓扑或 BGP 场景：`addresses` 留空，自行创建
`IPAddressPool` + `BGPAdvertisement`/`L2Advertisement`（或
`bgpAdvertisement`）等 CR；重新运行 `kurin addon install metallb`
会幂等地收敛 hook 应用池。

## 常用参数 / common parameters

| 参数 | 默认 | 说明 |
|---|---|---|
| `addresses` | — | L2 地址池（CIDR 或区间）/ L2 address pool (CIDR or range) |

## 就绪 / readiness

`Deployment/metallb-controller` 与 `DaemonSet/metallb-speaker` 可用。
speaker 需要 `NET_RAW`/`NET_ADMIN`（chart 默认放开）。

## 依赖 / dependencies

无（`requires: []`）。提供 `provides: [load-balancer]`：
`ingress-nginx` 等的 `controller.service.type = "LoadBalancer"`
依赖这里分配的 IP 才会有外部地址。
