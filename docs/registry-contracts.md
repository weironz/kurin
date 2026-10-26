# Registry 契约（冻结）

kurin 内置私有镜像仓库，装到 `role = "registry"` 节点上，供集群节点作为
镜像源（离线/弱网场景不必逐节点导入 tar，也不用外网 registry）。
三种实现：`docker-registry`（distribution v3）、`zot`、`harbor`。

## 配置（v1alpha1 新增，所有字段 `#[serde(default)]` 向后兼容）

```toml
[registry]
kind = "docker-registry"      # | "zot" | "harbor"（有 registry 节点时必填）
port = 5000                   # harbor 默认 8443（https）
username = "kurin"
password = "<htpasswd basic auth>"   # harbor: admin 初始密码
data-dir = "/var/lib/kurin/registry"  # harbor 固定用其安装目录

[nodes.reg1]
address = "192.168.10.30"
role = "registry"
```

校验规则（kurin-config/model.rs，owner: 集成负责人）：
1. 存在 `role = "registry"` 节点 ⇔ 必须有 `[registry].kind`（镜像 lb 规则）。
2. `harbor` 仅支持**单个** registry 节点（多实例无共享存储会分叉）。
3. registry 节点不能同时是 control-plane/worker/lb。
4. k3s 分布同样适用（registry 在 join 前可用即可）。
## 模式：seed（v0.4 唯一模式）

kurin 把 bundle 中选中的镜像按**原始 repo 路径**（去掉 registry 主机部分）
逐一 `ctr images push` 进内部 registry（`ctr` 已有 `pull`+`push` 路径，复用
现有 `images` 任务的 bundle 内容）。节点上 containerd 的
`/etc/containerd/certs.d/<upstream>/hosts.toml` 把已知上游
（registry.k8s.io、k8s.gcr.io、quay.io、ghcr.io、docker.io）的 endpoint
指到内部 registry 第一顺位：命中则为本地解析，未命中自动回退外网（在线
集群行为不变）。**不做 pull-through 代理**（distribution v3 单实例只能代理
一个上游，zot/harbor 才能多上游——该复杂度留给 v0.5 决策）。

## TLS 与信任

- kurin 在**首个 registry 节点**生成自签 CA（`/var/lib/kurin/pki/registry-ca.crt`），
  为每个 registry 节点签服务端证书（SAN 含节点内网 IP + 集群名）。
- k8s 节点通过 `registry/trust` 任务把 CA 装到
  `/etc/containerd/certs.d/<host>:<port>/hosts.toml`：
  ```toml
  server = "https://<host>:<port>"
  [host."https://<host>:<port>"]
    ca = "/etc/containerd/certs.d/<host>:<port>/ca.crt"
  ```
  zone=cn 时 registry 引用**不**改写（本地地址）。
- 认证：docker-registry/zot 用 htpasswd（bcrypt）；harbor 用其 admin 密码。
  `registry/trust` 同时把 auth 写进 hosts.toml（kurin 的节点 repo 推拉都认证）。

`optional: true` = `kurin artifact` 默认仍打包（registry 用户需要），但
`images/import` 不消费它——它只被 registry/install 任务读取。

## TLS 与信任

- kurin 在**首个 registry 节点**生成自签 CA（`/var/lib/kurin/pki/registry-ca.crt`），
  为每个 registry 节点签服务端证书（SAN 含节点内网 IP + 集群名）。
- k8s 节点通过 `registry/trust` 任务把 CA 装到
  `/etc/containerd/certs.d/<host>:<port>/hosts.toml`：
  ```toml
  server = "https://<host>:<port>"
  [host."https://<host>:<port>"]
    ca = "/etc/containerd/certs.d/<host>:<port>/ca.crt"
  ```
  zone=cn 时 registry 引用**不**改写（本地地址）。
- 认证：docker-registry/zot 用 htpasswd（bcrypt）；harbor 用其 admin 密码。

## 阶段与任务（Stage::Registry，位于 Prepare 之后、Images 之前）

| 任务 | 节点 | 作用 |
|---|---|---|
| `registry/install@regN` | 每个 registry 节点 | 制品下载校验、CA+证书、htpasswd、config、systemd 单元（zot/registry）或 harbor `install.sh`，等 `/v2/` 200 |
| `registry/trust@k8sN` | 每个 k8s 节点 | 装 CA + hosts.toml |
| `registry/seed@cp1` | 首控制面 | 把 bundle 选中镜像 `ctr images push` 进 registry |

harbor 特例：registry 节点由 kurin 安装 docker（registry 节点**不是** k8s
节点，不违反"无 docker 运行时"承诺——那是对 kubelet 的）。

## 不做

- registry 多实例负载均衡（单实例 + 备份由用户自理）
- harbor ChartMuseum/Trivy 组件（`install.sh` 默认集）
- 镜像 GC 策略管理
