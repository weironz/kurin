# 03 · 扩展性与插件体系

> 状态：定稿 v1.0 · 2026-10-27
> 问题：K8s 生态插件很多（网络、存储、ingress、监控、证书、镜像仓库……），kurin 现在的架构能不能“插拔”？以后集成更多工具时，会不会要大改代码？

---

## 1. 现状评估

### 1.1 现在加一个组件要改哪些地方

以“新增一个存储插件，比如 Longhorn”为例，按现有方式（`docs/development/plugins.md` 的集成点规则 + 存储的实际实现）：

| # | 改动位置 | 内容 |
|---|---|---|
| 1 | `kurin-config/src/config.rs` | `StorageKind` 增加变体，`StorageConfig` 增加专属字段 |
| 2 | `kurin-config/src/model.rs` | `Cluster` 增加平铺字段，`from_config` 映射，`validate` 增加规则 |
| 3 | `kurin/src/tasks/storage.rs` | 新任务实现（这个文件已有 501 行） |
| 4 | `kurin/src/plan.rs` | 注册到对应阶段（魔法下标） |
| 5 | `kurin/src/artifacts/manifest.json` | 镜像与 manifest 条目、sha256 |
| 6 | `kurin/templates/` | 嵌入 manifest 或模板 |
| 7 | `zone.rs` 与 `scripts/check-mirrors.sh` | 国内镜像源 |
| 8 | 离线：`images.rs` 的镜像集合 | 不加就会被离线校验拒绝 |
| 9 | 文档、`command-matrix.md`、e2e | — |

**结论：现有架构没有插拔能力**。它是“有界的编译期变体集合”，每加一个组件都要动 7～9 处、横跨两个 crate，而且全部需要发版。这个模式用在 CNI、发行版、HA 这类“深度嵌入安装流程”的组件上是合理的；但用在 metrics-server、ingress、cert-manager、监控这类“集群起来之后 apply 一下就行”的组件上，代价过高。

### 1.2 同类项目的做法（详见 [01](01-landscape.md) §2.4）

- RKE2/K3s：manifests 目录 + HelmChart CRD，加组件就是加数据。
- Kubespray：每个 addon 一个 role + 开关，加组件要写 Ansible。
- Zarf：组件 = chart/manifests/镜像/文件/actions，纯数据。
- KubeKey v4：playbook + schema。
- nelm：Helm chart + 权重/依赖注解 + 就绪跟踪。

---

## 2. 方案比较

| 方案 | 说明 | 优点 | 缺点 | 结论 |
|---|---|---|---|---|
| A. 维持编译期 enum | 现状 | 类型安全 | 扩展成本高，见 §1.1 | 否 |
| B. 全部 Rust trait 化 | 每个组件实现 `Component` trait，集中注册 | 一处注册；校验/能力单一来源 | 生态组件仍要写 Rust 并发版 | **用于核心组件** |
| C. 声明式 addon（数据） | YAML 描述 chart/manifest + 镜像 + values schema + 依赖 + 就绪规则 | 加组件不写代码；可单独打包成 OCI 分发；Web 表单可由 schema 生成 | 表达不了“改节点 OS / 参与 bootstrap”的组件 | **用于生态组件** |
| D. 动态插件（WASM / 外部进程） | 运行期加载任意代码 | 能力最强 | ABI 稳定性、安全审查、调试成本都高；违背“诚实、可审计”的原则 | 否（列为远期触发项） |
| E. 通用 playbook DSL | KubeKey v4 路线 | 灵活 | 放弃类型化幂等，`plugins.md` 已明确拒绝 | 否 |

---

## 3. 决策：两层组件模型（ADR-07）

```text
┌──────────────── 第 1 层：内置组件（Rust，Component trait）────────────────┐
│ 参与节点 OS 收敛或 bootstrap 的东西：                                       │
│  distro/kubeadm  distro/k3s   runtime/containerd  runtime/crio             │
│  ha/kube-vip  ha/kurin-lb  ha/external   os/baseline  os/sysctl  os/kernel │
│  cni/calico  cni/flannel  cni/cilium  cni/kube-ovn  cni/none               │
│  registry/zot  registry/docker-registry  registry/harbor（节点上的仓库）    │
│  addon-engine（第 2 层的执行器）                                           │
└─────────────────────────────────────────────────────────────────────────────┘
┌──────────────── 第 2 层：声明式 addon（数据，addon.yaml）──────────────────┐
│ 集群就绪之后安装的东西：                                                    │
│  metrics-server  local-path  nfs-subdir  ingress-nginx  cert-manager        │
│  metallb  kube-prometheus-stack  headlamp  longhorn  openebs  velero ……    │
│ 来源：内置目录（随二进制嵌入） / OCI addon 包 / 本地目录                    │
└─────────────────────────────────────────────────────────────────────────────┘
```

划分规则：**需要在节点上执行宿主机命令，或者必须在 Node Ready 之前存在的，进第 1 层；只需要对 API server 做 apply 的，进第 2 层。**

- CNI 留在第 1 层，因为节点在 CNI 就绪前不会 Ready，而且离线镜像必须预载。但 CNI 的**实现**复用第 2 层的 chart/manifest 应用器和就绪跟踪器（cilium 从 CLI 驱动改为 chart 渲染应用，kube-ovn 不再需要嵌入预渲染的 YAML）。
- 现有 `[storage] local-path / nfs` 迁到第 2 层（配置自动迁移，见 02 §3.12）。

### 3.1 第 1 层改造后，加一个内置组件的成本

| 位置 | 内容 |
|---|---|
| `crates/kurin-components/src/<kind>/<name>/` | 实现 `Component`（配置类型、validate、capabilities、artifacts、plan）+ 任务 |
| `kurin-components::registry()` | 一行注册 |
| `kurin-config` 中该 kind 的配置枚举 | 一个变体（承载组件自己的配置类型） |
| e2e 脚本 | 一个 |

能力表、命令矩阵文档、Web 选项、离线镜像集合、国内镜像源比对清单，全部从 `capabilities()` / `artifacts()` 自动导出，不再手工维护。

---

## 4. 第 2 层：addon 规范（ADR-08）

### 4.1 `addon.yaml`

```yaml
apiVersion: addons.kurin.dev/v1alpha1
kind: Addon
metadata:
  name: metrics-server
  version: 0.8.0                # addon 自身的版本（通常等于上游 app 版本）
  title: Metrics Server
  description: 资源指标 API，kubectl top 与 HPA 依赖它
  category: monitoring          # networking | storage | ingress | security | monitoring | registry | dashboard | other
  icon: icon.svg                # 可选，Web 展示用
spec:
  compat:
    kubernetes: ">=1.30.0"
    distributions: [kubeadm, k3s]
    arch: [amd64, arm64]
  requires: []                  # 依赖的 addon，如 ingress-nginx 依赖无；kube-prometheus-stack 可声明 requires: [local-path] 或可选依赖
  conflicts: []                 # 例如两个默认 StorageClass 的提供者
  provides: [metrics-api]       # 能力标签，供 requires-any 使用
  namespace: kube-system
  install:
    helm:
      chart: charts/metrics-server-3.13.0.tgz     # 包内相对路径，或 oci://… 引用（打包时会拉进包里）
      release: metrics-server
      values: values.yaml.j2                      # MiniJinja 模板，渲染上下文见 §4.4
    # 或者：
    # manifests: [manifests/*.yaml.j2]            # 服务端 apply
  images:                                          # 离线与预载的唯一依据，必须带 digest
    - registry.k8s.io/metrics-server/metrics-server:v0.8.0@sha256:…
  values-schema: values.schema.json                # 用户可调参数（JSON Schema）：CLI 校验与 Web 表单共用
  defaults:                                        # 用户未设置时的值，需满足 schema
    replicas: 1
  readiness:                                       # 可选；不写时由就绪跟踪器从渲染出的对象自动推导
    - { kind: Deployment, name: metrics-server }
  timeout: 5m
  hooks:                                           # 受限动作，不允许任意 shell
    post-install:
      - wait: { kind: APIService, name: v1beta1.metrics.k8s.io, condition: Available }
  uninstall:
    keep: []                                       # 卸载时保留的对象（如 CRD、PVC）
```

### 4.2 设计约束

1. **镜像必须完整列出且带 digest**。打包工具会校验：渲染后对象里出现的每个镜像都必须在 `images` 里，否则拒绝（保持“离线不会半装”的诚实原则）。
2. **hooks 只允许内建动作**：`wait`、`apply`、`delete`、`label-nodes`、`annotate`。v1 不支持任意 shell，需要宿主机操作的组件应该写成第 1 层组件。
3. **values-schema 是用户参数的唯一合法入口**。没有出现在 schema 里的 key 一律报错（延续 `deny_unknown_fields`）。高级用户可以用 `raw-values` 透传 Helm values，但会被明确标记为“未校验”。
4. **版本与兼容性由 addon 自己声明**，kurin 在校验期拒绝不兼容的组合（例如 K8s 版本、发行版或架构不匹配）。

### 4.3 用户配置

```toml
[addons.metrics-server]
enabled = true                 # 未写 version 时取所选包或内置目录里的默认版本

[addons.ingress-nginx]
version = "4.13.0"
values = { controller = { replicas = 2, service = { type = "NodePort" } } }   # 受 values-schema 校验

[addons.local-path]
default-class = true           # 由旧 [storage] 自动迁移而来

[addons.kube-prometheus-stack]
source = "oci://harbor.local/kurin/addons/kube-prometheus-stack:65.1.0"        # 外部 addon 包
raw-values = { grafana = { adminPassword = { env = "GRAFANA_PW" } } }          # 未校验的透传，秘密引用同样适用
```

- 合并顺序：`defaults` → 内置推荐值 → 用户 `values` → `raw-values`。这借鉴了 RKE2 HelmChartConfig“覆盖但不改原包”的思路。
- 秘密引用（`{ env }`/`{ file }`）在 values 中同样可用，渲染后只存在于内存和远端，事件与日志里会被脱敏。

### 4.4 渲染上下文

values 模板与 manifest 模板可以使用：

| 变量 | 内容 |
|---|---|
| `cluster.name`、`cluster.domain` | 集群名、DNS 域 |
| `kubernetes.version`、`kubernetes.distribution` | — |
| `network.pod_cidr`、`network.service_cidr`、`network.cni` | — |
| `ha.vip`、`ha.mode` | — |
| `registry.mirror` | 已配置私有仓库时的镜像前缀（离线时为必填） |
| `nodes`（name、role、address、arch、labels） | 选择器、副本数推导 |
| `addon.values` | 合并后的 values |
| `zone` | `global` / `cn` |

### 4.5 执行（第 1 层 `addon-engine` 组件）

1. **规划**：解析启用的 addon 和它的 `requires`，构造依赖 DAG；`conflicts` 冲突和缺少依赖都在校验期报错。每个 addon 生成一组任务，放在 `addons` 阶段，并按依赖关系连上 `after` 边。
2. **check**：读取 `kurin-state` 记录和 Helm release（或按 field manager 查出的对象），比较版本与 values 摘要；一致即 `InSync`。
3. **apply**：
   - **Helm**：把固定版本的 `helm` 二进制（以制品形式进入 base 包）和 chart 上传到锚点控制面，执行 `helm upgrade --install --namespace … -f values.yaml --wait=false`。也就是说，Helm 只负责渲染和记录 release；
   - **manifests**：在控制机渲染，经 `ClusterApi.apply` 做服务端 apply；
   - 然后由**就绪跟踪器**跟踪渲染出的对象（02 §3.11），持续发出 `Resource` 状态事件；超时会自动附带未就绪 Pod 的事件和日志尾部。
4. **verify**：执行 `readiness` 与 hooks 中的 `wait`。
5. **记录**：在 `kurin-state` 的 `addons.json` 中写入版本和 values 摘要。

不采用“在集群里部署 helm-controller，交给它调和”的 RKE2 方案，原因是：这样 kurin 得到的反馈就从“同步可见”变成了“异步调和”，Web 端的详细进度和失败原因都会变弱，而且还多一个常驻组件。如果以后有 GitOps 需求，可以单独提供一个 `flux` 或 `argo-cd` addon。

### 4.6 生命周期命令

```bash
kurin addon list   [-f cluster.toml]                  # 目录中的可用 addon + 集群中已装的（读 kurin-state）
kurin addon info   ingress-nginx                       # 元数据、版本、values schema（可 --schema 输出 JSON Schema）
kurin addon install ingress-nginx -f cluster.toml [--version V] [--set k=v] [--values f.yaml] [--dry-run]
kurin addon upgrade ingress-nginx -f cluster.toml [--version V]
kurin addon remove  ingress-nginx -f cluster.toml [--keep-crds]
```

- `install/remove` 默认会把配置文件里对应的 `[addons.x]` 段改写进去（toml_edit，保留注释），保持“配置即真相”；`--no-save` 关闭此行为。
- `create cluster` 会一次装好配置中启用的全部 addon；之后可以单独执行 addon 操作，不需要重跑整个安装计划。

### 4.7 来源与分发

| 来源 | 说明 |
|---|---|
| 内置目录 | 仓库 `addons/<name>/` 目录，构建时嵌入二进制（只嵌元数据与模板，chart 与镜像在包里或在线拉取） |
| OCI addon 包 | `kurin package create addon -f addons/<name>/addon.yaml` 生成，可推到任意 OCI 仓库（见 [04](04-oci-packages.md)） |
| 本地目录 | `source = "dir:///path/to/addon"`，便于开发自定义 addon |

首批内置目录（WP-46）：`metrics-server`、`local-path`、`nfs-subdir`、`ingress-nginx`、`cert-manager`、`metallb`、`headlamp`、`kube-prometheus-stack`。之后按需增加 `longhorn`、`openebs`、`velero`、`kubernetes-dashboard`、`argo-cd`。

---

## 5. 镜像仓库（registry）的定位（ADR-17）

镜像仓库在 OCI 包体系里是**分发枢纽**（见 [04](04-oci-packages.md) §5），所以它保留在第 1 层（需要装到节点上，并在 bootstrap 前可用）：

| kind | 定位 | 默认 |
|---|---|---|
| `zot` | 单静态二进制、OCI 原生（支持 OCI artifact 与 referrers），最适合承载 kurin 包与签名 | **默认** |
| `docker-registry` | distribution v3，兼容性最好 | 保留 |
| `harbor` | 企业功能全（多租户、扫描、复制），依赖 docker compose | 保留，标为“重量级” |
| `external` | 只配置地址与凭据，使用用户现有的仓库 | 新增 |

`[registry]` 配置不变；新增 `kind = "external"`，以及 `role = "registry"` 节点上的 `zot` 默认值。v1alpha1 中未写 kind 的配置保持原默认值，不悄悄改变行为。

---

## 6. 前后对比

| 场景 | 现在 | 改造后 |
|---|---|---|
| 新增 ingress-nginx | 改 7～9 处 Rust/JSON，并发版 | 新增 `addons/ingress-nginx/` 目录（addon.yaml + values 模板 + schema），不改 Rust |
| 用户自带内部组件 | 不支持 | 写 addon.yaml，`kurin package create addon` 打包推到内部仓库，在配置里引用 |
| 新增一个 CNI | 改 config 枚举、plan.rs、cni.rs 分派、manifest、zone、离线镜像集合 | 实现一个 `Component`（多数情况下只是 chart + 镜像 + 就绪规则 + 少量节点准备），注册一行 |
| Web 表单 | 每个组件手写表单 | 第 2 层由 values-schema 自动生成；第 1 层的核心字段手工精排 |
| 离线 | 每个组件手工加入镜像集合，漏掉就会被拒 | `artifacts()` / `images` 自动汇总进包 |

---

## 7. 远期触发项

| 能力 | 触发条件 |
|---|---|
| addon 中的受控 shell 动作 | 出现无法用内建动作表达、又不值得写第 1 层组件的需求，且 addon 来自已签名的包 |
| WASM 插件（第 1 层的外部实现） | 第三方需要在不 fork kurin 的情况下提供节点级组件 |
| GitOps 模式（集群内调和） | 用户希望 addon 的期望状态由集群自己维持 |
