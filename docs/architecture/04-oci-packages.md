# 04 · OCI 离线包与制品分发

> 状态：定稿 v1.0 · 2026-10-27
> 问题：离线包能不能统一封装成 OCI 格式，走 OCI 镜像仓库分发（参考 Zarf）？
> 结论：**能，而且应该这样做。** kurin 的镜像本来就以 OCI 形式存储、全部 digest 固定，包化是自然的延伸。但镜像改写**不照搬 Zarf 的 webhook**，而是在容器运行时的 mirror 配置层完成。

---

## 1. 现状

| 项 | 现状 | 位置 |
|---|---|---|
| 生成 | `kurin artifact --out <dir> [--arch] [--with-kubernetes]` | `main.rs:399-451` |
| 布局 | 目录，`<name>-<version>-<arch>/` 子目录 + sidecar 校验和 + `kurin-manifest.json` | `artifact_bundle.rs` |
| 镜像 | OCI index / manifest / blobs，digest 固定 | `tasks/images.rs` 头注释 |
| 使用 | `[artifacts] offline-dir` 或 `--offline <dir>`，路径在**控制机**上 | `model.rs` |
| 下发 | 控制机整文件读入内存后经 SSH 推到每个节点；镜像在节点上 `ctr import` | `tasks/install.rs:193-219` |
| OS 依赖 | 只有 deb 载荷；rpm 系离线无法安装内核依赖 | `tasks/kernel_deps.rs` |
| 缺口 | flannel/cilium/kube-ovn 的镜像不在包里，离线被拒；addon 无法离线；包无法版本化、签名或用仓库分发 | `docs/command-matrix.md` |

---

## 2. 与 Zarf 的对照

| Zarf 理念 | kurin 采用？ | 说明 |
|---|---|---|
| 包 = OCI 制品，可 publish/pull 到任意仓库 | **采用** | ADR-11 |
| 同一个包既能放仓库，也能存成单个压缩文件 | **采用** | 三种传输形态，见 §3.3 |
| 组件按 OS/arch 过滤 | **采用** | 包内按 `platform` 组织，按节点 facts 选择 |
| init 包：先在集群里部署 registry，再推镜像 | **部分采用** | kurin 在 bootstrap **之前**就能把 registry 装到 `role = "registry"` 节点上，不需要 Zarf 的注入器技巧；集群内 registry 作为远期 addon |
| mutating webhook 改写镜像地址 | **不采用** | kurin 掌控容器运行时配置，在 containerd/CRI-O/k3s 的 mirror 层透明改写，不需要集群内常驻组件，也覆盖 static pod（webhook 管不到 static pod） |
| SBOM 自动生成 | **采用** | ADR-14 |
| cosign 签名与校验 | **采用（校验内置，签名在 CI）** | ADR-14 |
| 变量与模板 | 已有 | MiniJinja + addon values（[03](03-addons.md)） |
| 组件 actions（shell） | 不采用（v1） | 见 [03](03-addons.md) §4.2 |

---

## 3. 包格式（ADR-11）

### 3.1 结构

一个 kurin 包是一个 **OCI image index**（OCI image-spec v1.1，与 ORAS 兼容）：

```text
index (mediaType: application/vnd.oci.image.index.v1+json
       artifactType: application/vnd.kurin.package.v1)
 ├─ manifest: 包元数据   artifactType = application/vnd.kurin.package.meta.v1+json
 │     config = application/vnd.oci.empty.v1+json
 │     layers = [ kurin-package.json ]
 ├─ manifest: 文件集（每个架构一个，带 platform）  artifactType = application/vnd.kurin.files.v1
 │     layers = [ kubeadm, kubelet, kubectl, containerd.tar.gz, runc, crictl, cni-plugins.tgz, helm, … ]
 │              每层的 annotation：org.opencontainers.image.title=<文件名>、dev.kurin.artifact.name=<清单名>
 ├─ 镜像：上游原始 index / manifest（按原 digest 保存），annotation dev.kurin.image.ref=<上游完整引用>
 │     例：registry.k8s.io/kube-apiserver:v1.33.1、quay.io/tigera/operator:v1.38.6 …
 ├─ Helm chart（addon）：标准 Helm OCI 制品（application/vnd.cncf.helm.config.v1+json）
 └─ OS 依赖（os 类包）：每个 OS/版本/架构一个 manifest，层为 deb/rpm 仓库 tar
referrers（subject = index）：SBOM（application/spdx+json）、签名（cosign）
```

关键点：**镜像保留上游原始 digest**。kurin 的清单按 index digest 固定镜像，节点上的 containerd 通过 mirror 按原引用解析，所以 digest 不能变。多架构镜像默认只携带所选架构的子 manifest 与 blob，index 本身原样保留。部分仓库会拒绝“子 manifest 不全”的 index，这一点在 WP-30 的 spike 中验证，并提供 `--full-multiarch` 选项作为兜底。

### 3.2 元数据 `kurin-package.json`

```json
{
  "schema": 1,
  "name": "base",
  "kind": "base",
  "version": "1.33.1",
  "kurin": ">=0.6.0, <0.8.0",
  "created": "2026-10-27T00:00:00Z",
  "distribution": "kubeadm",
  "arch": ["amd64", "arm64"],
  "components": [
    { "id": "distro/kubeadm", "version": "1.33.1" },
    { "id": "runtime/containerd", "version": "2.1.4" },
    { "id": "cni/calico", "version": "3.30.3" },
    { "id": "cni/cilium", "version": "1.18.2" }
  ],
  "artifacts": [
    { "name": "kubeadm", "kind": "binary", "version": "1.33.1", "arch": "amd64",
      "digest": "sha256:9a48…", "size": 72351928, "upstream": "https://dl.k8s.io/release/v1.33.1/bin/linux/amd64/kubeadm" }
  ],
  "images": [
    { "ref": "registry.k8s.io/kube-apiserver:v1.33.1", "digest": "sha256:…", "arch": ["amd64", "arm64"], "used-by": ["distro/kubeadm"] }
  ],
  "charts": [],
  "os": []
}
```

它取代现有的 `kurin-manifest.json`（字段是它的超集，可由旧格式转换得到）。上表中的版本号只是示例。

### 3.3 三种传输形态，同一格式

| 形态 | URI | 用途 |
|---|---|---|
| OCI 仓库 | `oci://harbor.local/kurin/base:v1.33.1-kubeadm` | 企业内网分发、版本管理、增量（只传缺失 blob） |
| OCI layout 目录 | `oci-layout:///srv/kurin/base` | 控制机本地，兼容 `oras`、`skopeo` 等工具 |
| 单文件 | `file:///media/usb/kurin-base-v1.33.1.tar.zst` | 跨网闸、U 盘摆渡（layout 目录打 tar，可选 zstd 压缩） |

`kurin package push <src> <dst>` 可以在任意两种形态之间复制，只传目标端缺失的 blob。

### 3.4 包的种类

| kind | 内容 | 命名示例 |
|---|---|---|
| `base` | 某个 K8s 版本 + 发行版的全部二进制与镜像：K8s 组件、运行时、全部（或所选）CNI、kube-vip、haproxy/keepalived 等节点组件、helm、registry 二进制 | `kurin/base:v1.33.1-kubeadm`、`kurin/base:v1.33.1-k3s` |
| `addon` | 一个 addon：addon.yaml + 模板 + chart + 镜像 | `kurin/addons/ingress-nginx:4.13.0` |
| `os` | 某个 OS 家族 / 版本 / 架构的依赖包仓库（conntrack、socat、ipset、ebtables、ipvsadm、haproxy、keepalived、nfs-utils…） | `kurin/os/ubuntu-24.04:2026.10`、`kurin/os/rocky-9:2026.10` |
| `bundle` | 把多个包合成一个 index，便于单文件摆渡 | `kurin-offline-v1.33.1.tar.zst` |
| `custom` | 用户通过 `kurin-package.yaml` 自定义 | — |

### 3.5 自定义包定义 `kurin-package.yaml`

```yaml
apiVersion: packages.kurin.dev/v1alpha1
kind: Package
metadata: { name: corp-addons, version: 2026.10.1 }
spec:
  arch: [amd64, arm64]
  include:
    - oci://ghcr.io/weironz/kurin/addons/ingress-nginx:4.13.0     # 复用已有包
  addons:
    - ./addons/corp-logging                                         # 本地 addon 目录
  images:
    - harbor.corp/tools/debug:1.2@sha256:…
  files:
    - { name: corp-agent, url: "https://dl.corp/agent-{arch}", sha256: { linux-amd64: "…", linux-arm64: "…" } }
```

---

## 4. 命令与配置

```bash
kurin package create base  [--kubernetes 1.33.1] [--distribution kubeadm|k3s] [--arch amd64,arm64]
                           [--cni all|calico,cilium] [--with-os ubuntu-24.04,rocky-9] [--addons ingress-nginx,…]
                           -o oci-layout://DIR | file://X.tar.zst
kurin package create -f kurin-package.yaml -o …
kurin package create bundle --from <src>… -o file://X.tar.zst
kurin package inspect <src> [--manifest] [--sbom]     # 摘要 / 完整 kurin-package.json / SPDX 文档
kurin package verify  <src> [--key cosign.pub]        # digest 闭包全量校验（--key 签名校验在 WP-39）
kurin package push    <src> <dst>                     # 任意两种形态间复制，只传缺失 blob
kurin package pull    <src> <dst>                      # push 的本地目标别名（oci-layout:// / file://）
kurin package mirror  <src> --registry REG [--ca FILE] [--user U --password-env V]   # 按 §5.2 的布局预先推入外部仓库
kurin package list [--format text|json] | prune [--dry-run]                            # 本机内容寻址缓存
```

`push` / `pull` / `verify` / registry 来源的 `inspect` 接受 `--user` +
`--password-env`（缺省读 `~/.docker/config.json`，再缺省匿名）。`oci://`
引用：远程仓库默认 HTTPS，回环地址默认 HTTP（同 Docker 的 insecure
localhost 规则），`oci://http://` / `oci://https://` 前缀可显式覆盖。
子命令里的 JSON 输出用 `--manifest` / `--format json` 而不是 `--json`：
全局 `--json` 归事件流所有，clap 要求同一命令下长选项唯一。

```toml
[packages]
base = "oci://harbor.local/kurin/base:v1.33.1-kubeadm"     # 或 file:// / oci-layout://；不写则在线使用内置清单
os = ["oci://harbor.local/kurin/os/rocky-9:2026.10"]       # 不写时按节点 facts 自动匹配（离线时必须能匹配上）
delivery = "auto"                                          # auto | preload | registry
verify = "digest"                                          # none | digest | signature
trusted-keys = ["/etc/kurin/cosign.pub"]                   # verify = "signature" 时必填
```

兼容：

- `kurin artifact --out DIR` 保留两个小版本，作为 `kurin package create base -o oci-layout://DIR` 的别名，并打印弃用提示。
- `[artifacts] offline-dir` 与 `--offline` 继续接受旧目录布局（兼容读取器），同时接受上述三种 URI。
- 已有离线环境升级 kurin 后不需要重新制作离线包。

---

## 5. 分发策略（ADR-12）

### 5.1 `preload`（无仓库时的默认）

- 控制机从本地内容寻址缓存（`~/.cache/kurin/blobs/sha256/…`）**流式**上传文件到节点（修复整文件读入内存的问题），按节点 `facts.arch` 只传对应架构。
- 镜像以单镜像 OCI layout tar 的形式流式上传，节点上 `ctr -n k8s.io images import`（k3s 放进 agent images 目录）。
- addon 镜像也预载到所有 k8s 节点。这样做简单可靠，但带宽开销是“节点数 × 包大小”。
- 优化：同一 phase 内节点并行上传，受控制机上行带宽和 `--max-upload-parallel` 限制。

### 5.2 `registry`（配置了仓库时的默认）

1. registry 节点（默认 zot）只用 preload 方式拿到自己的二进制并完成安装（在 `registry` 阶段，早于 bootstrap）；如果是 `external` 仓库，则跳过这一步。
2. 控制机把包内**所有镜像和文件只推一次**到仓库，使用确定性的布局：
   - 镜像：`<reg>/mirror/<上游主机>/<路径>:<tag>`，例如 `reg:5000/mirror/registry.k8s.io/kube-apiserver:v1.33.1`（digest 不变）；
   - 文件：`<reg>/kurin/files/<名称>:<版本>-<arch>`，单层制品。
3. 节点用 `curl`，带仓库 CA 和只读凭据，按 digest 从仓库的 blob API 下载文件并校验 sha256。节点之间并行，控制机不再是带宽瓶颈。
4. 镜像由运行时 mirror 配置透明改写（§5.4）。
5. 之后的 add nodes、addon 安装、升级也走同一个仓库，只推增量 blob。

### 5.3 `seed`（可选，WP-77 已实现）

控制机内置一个只读 OCI registry（直接读本地 blob 缓存），通过 SSH 远程端口转发暴露到每个节点的回环地址（实现用随机端口；文档示例 `127.0.0.1:5005`）。适用于"没有 registry 节点，但想用拉取语义"的场景，例如引导集群内 registry addon。安装结束后转发即失效，所以它**不能**作为集群长期的镜像来源。

实现（v1alpha2 `[packages] delivery = "seed"`，与任何 `[registry]` 配置互斥，需
离线载荷来源）：`registry/seed-serve@controller` 起服务（`127.0.0.1:0`，随机端口），
`registry/seed-mirror@node` 开远程转发并写回环 `http://` 运行时 mirror；镜像走
pull，二进制仍走 SSH 流式上传；操作收尾即拆除。安全边界：仅回环、无认证、
只读（上传/写入一律 405）。

### 5.4 镜像改写：运行时 mirror（ADR-13）

| 运行时 | 配置 |
|---|---|
| containerd | `/etc/containerd/certs.d/<上游主机>/hosts.toml`：`[host."https://reg:5000/v2/mirror/<上游主机>"]`，`capabilities = ["pull","resolve"]`，`override_path = true`，`ca = …`（`config_path` 已在 0.4.0 接好） |
| CRI-O | `/etc/containers/registries.conf.d/50-kurin.conf`：`[[registry]] prefix = "<上游主机>"`，`location = "reg:5000/mirror/<上游主机>"` |
| k3s | `/etc/rancher/k3s/registries.yaml`：`mirrors.<上游主机>.endpoint` + `rewrite` |

覆盖的上游主机由包内所有镜像引用自动汇总（`registry.k8s.io`、`docker.io`、`quay.io`、`ghcr.io`……），用户的工作负载使用这些公共镜像时同样会命中私有仓库。不改写 Pod spec，因此 static pod、DaemonSet、Helm 渲染结果都不需要特殊处理。

### 5.5 `auto` 的判定

```text
配置了 [registry]（role = "registry" 节点，或 kind = "external"） → registry
否则                                                             → preload
```

---

## 6. 信任链（ADR-14）

| 层 | 机制 | 默认 |
|---|---|---|
| 完整性 | 所有 blob 按 digest 寻址；控制机拉取时校验，节点落盘后再用 sha256 校验 | 始终开启 |
| 上游来源 | 制作包时，二进制按官方 `.sha256` 或清单中的固定值校验，镜像按 registry `docker-content-digest` 校验（沿用现有规则：sha256 必须真实来自上游） | 始终开启 |
| 签名 | 项目 CI 用 cosign 对发布的包签名；kurin 内置校验（优先用 sigstore-rs；不可行时回退到 cosign 公钥格式的 ECDSA P-256 校验，用 `p256` crate 实现） | `verify = "digest"`；`signature` 可选 |
| SBOM | 制作包时由元数据生成 SPDX 2.3 JSON（每个文件/镜像的名称、版本、purl、digest、下载地址），作为 referrer 附在 index 上；`kurin package inspect --sbom` 可导出 | 始终生成 |

---

## 7. 项目发布的官方包

- CI 为每个支持的 K8s 补丁版本 × 发行版构建 `base` 包，为每个内置 addon 构建 `addon` 包，为每个支持的 OS 构建 `os` 包（在对应发行版容器里下载依赖），推送到 `ghcr.io/weironz/kurin/…` 并签名。
- 国内访问：在 `zone = "cn"` 时把 `ghcr.io` 改写到 ghcr 的国内镜像。**WP-38 核实结论（2026-10-28）**：`ghcr.m.daocloud.io`（CN_MIRRORS 表中已有条目，digest 与上游一致）可用，是默认改写目标；备选 `ghcr.nju.edu.cn`（南京大学 e-Science 缓存代理，把 `ghcr.io` 域名替换即可、匿名拉取）经核实可解析、可拉取，作为 `scripts/check-mirrors.sh` 的可达性观察项。两者都失效时的回退：`kurin package pull` 经代理后以 `oci-layout://` / `file://` 形态使用——包的三种形态等价（§3.3）。
- 在线安装默认仍使用二进制内嵌的清单（行为不变）。`[packages] base = "oci://…"` 是可选项，让用户无需升级 kurin 就能使用新的 K8s 补丁版本。

---

## 8. 实现要点

| 模块 | 依赖 | 说明 |
|---|---|---|
| `kurin-artifact::oci::layout` | `oci-spec` 0.9.x | 自己实现 OCI layout 读写（结构简单，避免引入重依赖）。版本必须与 `oci-client` 依赖的 `oci-spec` 一致（0.16.1 → 0.9.0），否则依赖图出现两份类型不互通的 crate（WP-30 实测） |
| `kurin-artifact::oci::remote` | `oci-client`（ORAS 维护） | push/pull、分块上传、按 digest 跳过已存在的 blob。注意 0.16.1 的 `ImageIndexEntry` 没有 `artifactType` 字段：条目级 `artifactType` 必须用 `push_manifest_raw` 自行序列化（WP-30 实测） |
| `kurin-artifact::store` | — | 内容寻址 blob 缓存，原子写入（`.part` → rename），`prune` |
| `kurin-artifact::package` | `tar`、`zstd` | 包的创建、打开、校验、复制 |
| `kurin-artifact::preload` | — | WP-35：按 `ArtifactSources` 解析节点载荷（包 / 旧 bundle / 在线缓存），按 `facts.arch` 选 blob；镜像流式生成单镜像 OCI layout tar。包按 URI 进程内只打开一次（`oci://` 只拉一次） |
| 交叉编译 | — | **已验证**（WP-30）：`zstd-sys`（C+汇编）、bundled SQLite、rustls 的 `aws-lc-rs` 在 `cargo zigbuild` 下 x86_64-musl 与 aarch64-musl 均构建成功，产物为静态 ELF；`ruzstd` + gzip 仅作应急，不需要启用。原生 `cargo build --target x86_64-unknown-linux-musl` 需要 `musl-gcc`（本机缺失时用 zigbuild） |

---

## 9. 风险

| 风险 | 缓解 |
|---|---|
| 部分仓库拒绝“子 manifest 不全”的多架构 index | **WP-30 实测**：zot v2.1.22 接受残缺 index；distribution v3.1.2 与 harbor 2.13.0 返回 400 `MANIFEST_BLOB_UNKNOWN`，且均无放宽配置。因此推送前必须补齐 index 引用的全部子 manifest；`--full-multiarch` 的语义是“补齐所有架构”，不是“允许残缺” |
| 包体积大（多 CNI × 多架构） | `--cni`、`--arch` 裁剪；仓库形态天然去重；`bundle` 默认使用 zstd |
| 节点没有 `curl` | facts 检测；没有时回退到该节点的 preload |
| 仓库自签 CA | kurin 签发的 CA 已经分发到节点（0.4.0 已实现）；外部仓库通过 `[registry] ca-file` 提供 |
| OS 依赖包与节点上现有包冲突 | 只装缺失的包；版本冲突时报错，并给出“手动安装后重试”的提示 |
