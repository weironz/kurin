# 高可用控制面

三种入口形态，按运维条件选一个。共同要求：至少 2 个（推荐 3 个）
`role = "control-plane"` 节点；`vip` 不能与任何节点地址冲突。

## kube-vip（默认）

kurin 在每个控制面写 kube-vip 静态 pod，由它持有 VIP。不需要额外机器。

```toml
[ha]
vip = "192.168.10.100"
mode = "kube-vip"          # 默认，可省略

[nodes.master01]
address = "192.168.10.11"
role = "control-plane"

[nodes.master02]
address = "192.168.10.12"
role = "control-plane"

[nodes.master03]
address = "192.168.10.13"
role = "control-plane"
```

验证：

```bash
kubectl --server=https://192.168.10.100:6443 get --raw /readyz
kubectl -n kube-system get pods -l component=etcd   # 3 个投票成员
```

k3s 只支持 kube-vip 模式；前 3 个控制面自动 `cluster-init` 形成嵌入 etcd。

## external-endpoint（自管 LB）

`vip` 是你的负载均衡器地址；kurin 不安装任何 LB 组件，只做校验。

```toml
[ha]
vip = "192.168.10.100"
mode = "external-endpoint"
```

要求：≥2 个控制面，`vip` 必填。LB 后端指向每个控制面的 `:6443`。

## kurin-lb（haproxy + keepalived）

kurin 在 `role = "lb"` 节点上安装 haproxy + keepalived。LB 节点不参与
Kubernetes（不跑 kubelet）。

```toml
[ha]
vip = "192.168.10.100"
mode = "kurin-lb"

[nodes.lb1]
address = "192.168.10.31"
role = "lb"

[nodes.lb2]
address = "192.168.10.32"
role = "lb"                # 第二个 LB 自动成为 keepalived BACKUP
```

要求：≥2 个控制面、≥1 个 lb 节点、`vip` 必填。haproxy 对每个后端做
HTTPS `/readyz` 健康检查，统计页在 `http://<lb>:8404/haproxy-stats`。

验证：

```bash
kubectl --server=https://192.168.10.100:6443 get --raw /readyz
# VIP 当前持有者
ip -4 addr show | grep 192.168.10.100
```

## 单控制面

`kube-vip` 模式下单控制面 + `vip` 会报错（没有可漂移的目标）。单节点实验
环境不要写 `[ha]`。

## 相关

- [配置参考 §ha](../reference/configuration.md#ha-三模式)
- [升级](upgrade.md)：控制面逐个滚动
- [证书](certificates.md)：每个控制面独立续期