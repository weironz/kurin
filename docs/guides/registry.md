# 私有 registry

kurin 可以在 `role = "registry"` 节点上安装私有仓库，供集群节点作为镜像源；
也可以接入运维自管的仓库（`external`）。

## 配置

```toml
[registry]
kind = "zot"                  # docker-registry | zot | harbor | external
port = 5000
username = "kurin"
password = "<htpasswd-password>"
data-dir = "/var/lib/kurin/registry"

[nodes.reg1]
address = "192.168.10.30"
role = "registry"
```

v1alpha2 中声明了 registry 节点但没写 `kind` 时默认 `zot`；v1alpha1 保持
`kind` 必填。

## 四种形态

| kind | 实现 | 说明 |
|---|---|---|
| `docker-registry` | CNCF distribution v3 | 单二进制 + systemd，最简 |
| `zot` | project-zot | OCI 原生，开 `docker2s2` 兼容 `ctr` 推送；v1alpha2 默认 |
| `harbor` | Harbor v2 离线安装器 | 自带 UI；仅单 registry 节点；username 必须 `admin` |
| `external` | 运维自管仓库 | 不安装任何东西；需要 `address`，可选 `ca-file`、`namespace` |

`external` 示例：

```toml
[registry]
kind = "external"
address = "https://registry.example.com:5000"
ca-file = "/etc/kurin/registry-ca.pem"   # 可选；不写用系统信任库
namespace = "kurin"                      # 默认 kurin
```

## 校验

- 存在 `role = "registry"` 节点 ⇔ 必须有 `[registry]`（`external` 不需要
  registry 节点）。
- registry 节点不能同时是 control-plane/worker/lb。
- `harbor` 仅支持单个 registry 节点。
- `username` / `password` 必须成对；harbor 的 username 只能是 `admin`。
- `external` 的 `namespace` 只能是斜杠分隔的小写路径段；`ca-file` 必须存在
  且只能与 `external` 一起使用。

## TLS 与信任

- kurin 在首个 registry 节点生成自签 CA（`/var/lib/kurin/pki/ca.crt`），为
  每个 registry 节点签发服务端证书（SAN 含节点 IP + 集群名）。
- `registry/trust` 把 CA 与认证写进每个节点的
  `/etc/containerd/certs.d/<host>:<port>/hosts.toml`；CRI-O 写
  `registries.conf.d/50-kurin.conf`，k3s 写 `registries.yaml`。
- `external` 用 `ca-file` 指定的 PEM；不写则用系统信任库。

## 阶段与任务

`Stage::Registry` 位于 prepare 之后、images 之前：

| 任务 | 节点 | 作用 |
|---|---|---|
| `registry/install@regN` | 每个 registry 节点 | 下载校验制品、CA+证书、htpasswd、config、systemd（或 harbor installer），等 `/v2/` 200 |
| `registry/trust@k8sN` | 每个 k8s 节点 | 装 CA + hosts.toml / registries.conf |
| `registry/push@controller` | 控制机 | 把包内镜像与文件推入仓库（只推缺失 blob） |
| `registry/seed@cp1` | 首控制面 | 把 bundle 选中镜像 `ctr images push` 进仓库 |

`delivery = "auto"`（默认）在配置了 `[registry]` 时走 registry，否则走
preload。

## harbor 限制

harbor 的仓库是 `project/repo` 形状，无法服务单段路径的上游 mirror，因此
harbor 模式不做上游 mirror：镜像统一 seed 到内置 `library` 项目，kubeadm
的 `imageRepository` 指向 `<reg>:<port>/library`。CNI 等非控制面镜像的离线
供给仍建议用 zot/docker-registry。

## 相关

- [Registry 契约](../reference/registry-contracts.md)
- [离线安装](offline.md)
- [seed 仓库](seed.md)
- [CLI 参考 · package](../reference/cli.md#package)