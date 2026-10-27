# 01 · 同类项目调研与差异分析

> 状态：定稿 v1.0 · 2026-10-27 · 代码基线 v0.4.0
> 目的：把 kurin 和成熟的集群部署/交付项目放在一起比较，找出**设计上的重大差异**和**值得借鉴的点**。每条借鉴都对应到后续文档里的决策编号（`ADR-xx`，见 [README](README.md)）和开发计划工作包（`WP-xx`，见 [06-dev-plan](06-dev-plan.md)）。
>
> 资料来源：各项目 GitHub README 与官方文档（2026-10 抓取），见文末链接。标注 *（待核实）* 的条目来自二手资料或记忆，实施相关工作包前要先复核。

---

## 1. 调研对象一览

| 项目 | 定位 | 语言 / 执行引擎 | 状态（2026-10） |
|---|---|---|---|
| **KubeKey**（kurin 的重写对象） | 集群生命周期 + KubeSphere 安装 | Go；v4 改为**仿 Ansible 的通用任务框架**（Project/Playbook/Role/Task，连接器 ssh/local/kubernetes） | 活跃，v4.0.5（2026-04）；v4 带 `kk web` 安装 UI |
| **Kubespray**（kubernetes-sigs） | 生产级集群安装，组合矩阵最广 | Ansible playbook/roles | 活跃；生态有 Kubean（CNCF Sandbox，把 kubespray 包成 Operator） |
| **kubeasz**（easzlab） | 二进制方式部署，中文社区常用 | Ansible + `ezctl`/`ezdown` 脚本 | 活跃，3.7.0 推荐 K8s v1.33–v1.36 |
| **RKE1 / RKE2 / K3s**（Rancher/SUSE） | 发行版 + 安装器 | Go；RKE2/K3s 为单二进制发行版，内置 helm-controller | RKE1 已于 **2025-07-31 EOL**；RKE2 为继任者 |
| **KubeOperator**（FIT2CLOUD） | Web 化 K8s 发行版：规划 → 部署 → 运维 | Go 后端 + Web；Terraform 建机 + Ansible 部署 | **2024-07-09 归档，只读** |
| **PangeeCluster**（opencmit / 中国移动） | 图形化离线安装、维护工具 | Go 后端（`go.work`）+ `web/`；v1 基于 kubespray，v2 改为基于独立“资源包”（含 ansible roles 的二进制安装脚本） | 活跃，约 2k star；Docker 镜像或单二进制，Web 端口 9080 |
| **werf**（Flant） | **不是集群安装器**，而是构建 + 部署到 K8s 的 CI/CD CLI（CNCF Sandbox）；部署引擎是 **nelm**（Helm 4 替代品） | Go | 活跃 |
| **Zarf**（zarf-dev） | 离线（air-gap）原生的 K8s **包管理器** | Go；注入器用 Rust | 活跃，OpenSSF Scorecard |
| *附：Sealos*（labring） | “集群镜像”理念：把 K8s 本身打成 OCI 镜像，`sealos run` 一条命令带 IP 与密码直接建集群 | Go | CLI 仍可用，主线已转向云 OS *（待核实）* |

> 关于 werf：用户列出了 werf.io，但它不负责建集群。它和 kurin 的交集在**“部署过程的可观测性”**与**“制品走容器仓库发布”**两点，这正是本次需求里“友好详细的日志界面”和“OCI 分发”要解决的问题，所以按“借鉴部署引擎”的角度纳入调研。

---

## 2. 维度对比

### 2.1 执行模型

| | 方式 | 特点 |
|---|---|---|
| kurin | 编译期类型化 Rust `Task`：`check_state → apply → verify`；`Unknown ≠ failed`；计划/执行分离，`--dry-run --check` 可在线做漂移比对 | **幂等性和失败语义是同类中最严格的**；扩展要写 Rust |
| KubeKey v4 | 通用 playbook 引擎（YAML），内置模块 command/copy/template | 扩展门槛低，但正确性靠 playbook 作者自己保证；它从“专用安装器”走向“通用任务框架” |
| Kubespray / kubeasz / PangeeCluster | Ansible | 生态成熟、可改参数极多（kubeasz 原话：“几乎可以设置任何组件的任何参数”）；依赖控制机上的 Python/Ansible；幂等程度取决于每个 role |
| RKE2 / K3s | 节点上跑发行版二进制，组件以 Helm Chart 形式由内置 **helm-controller** 在集群内调和 | 核心组件生命周期交给集群内控制器，不依赖外部工具持续运行 |
| KubeOperator | Terraform（IaaS 建机）+ Ansible | Day0 规划能力强（含建机）；项目已归档 |

**差异判断**：kurin 不应该走 KubeKey v4 那种通用 playbook 路线（这和 `docs/plugins.md` 的既定原则一致）。真正的短板是**生态组件扩展必须改 Rust 代码、动 6～7 处文件**。结论是“分层”：核心路径保持类型化 Rust，生态组件改成声明式数据（见 [03-addons](03-addons.md)，ADR-07/ADR-08）。

### 2.2 配置与快速上手

| | 配置入口 | 快速创建 |
|---|---|---|
| kurin | 严格的 TOML/YAML（`deny_unknown_fields`、带 span 的诊断） | 必须先 `create config` 生成文件再编辑 |
| KubeKey | `config-sample.yaml`；`kk create cluster --with-kubernetes vX`（all-in-one 无需配置文件） | 单机一条命令 |
| kubeasz | `ezctl new <cluster>` 生成 `clusters/<name>/{hosts,config.yml}`；AllinOne 一键脚本 | 交互少，但要改 hosts 文件 |
| Sealos | `sealos run <镜像> --masters ip1,ip2 --nodes ip3 -p <密码>` | **命令行直接传 IP、角色、密码**，体验最好 |
| PangeeCluster / KubeOperator | Web 表单 | 表单向导 |

**差异判断**：kurin 缺一条“命令行直接带节点 IP、角色、认证信息”的路径。决策见 [05-cli-quick-create](05-cli-quick-create.md)（ADR-10）：flags 与配置文件进入**同一个 `ClusterConfig` 类型、同一套校验**，可以 `--save` 成配置文件；密码默认不进 argv。

### 2.3 离线与制品分发

| | 制品形态 | 节点如何拿到 | 镜像改写 |
|---|---|---|---|
| kurin | `kurin artifact --out <dir>`：目录里放 `name-version-arch` 与 `kurin-manifest.json`，镜像为 OCI layout；sha256 全部固定 | 控制机**整文件读入内存**后经 SSH 逐节点上传（`tasks/install.rs:193-219`），镜像 `ctr import` | `KURIN_ZONE=cn` 改写上游地址；containerd `certs.d` 已接好 |
| Kubespray | `generate_list.sh` 生成 files.list/images.list；`manage-offline-*` 脚本起 nginx 和本地 registry | `download_run_once`：下载一次后由委托节点分发；或每节点各自下载 | 用变量把各组件仓库地址指向私有仓库 |
| kubeasz | `ezdown` 下载二进制、镜像，并起本地 registry | 控制机分发 + 本地 registry | 配置项 |
| KubeOperator | 离线包 + 内置仓库（Nexus 承载 yum/apt/镜像/文件）*（待核实）* | 节点从仓库拉 | 指向内置仓库 |
| PangeeCluster | **资源包**：独立仓库按 K8s 版本分支发布，含 roles/脚本/依赖；Web 端导入 | 由资源包内脚本分发 | — |
| RKE2 / K3s | airgap 镜像 tar 放入 agent images 目录启动时导入；`registries.yaml` 配镜像源 | 节点本地导入或私有仓库 | 运行时 mirror |
| **Zarf** | **包 = OCI 制品**，可 `publish/pull` 到任意 OCI 仓库，也可存成单个压缩文件；组件按 OS/arch 过滤；自动 SBOM；cosign 签名 | `zarf init` 先部署集群内 registry（用 Rust 注入器从 ConfigMap 拼出种子镜像自举），再推镜像 | **变更 webhook** 改写 Pod 镜像地址并注入 pull secret |
| Sealos | 集群镜像（OCI），整套 K8s 当镜像拉取 | — | — |

**差异判断**：kurin 的 OCI 基础其实已经有了（镜像本来就按 OCI index/manifest/blob 存储、全部 digest 固定），差的是：① 包本身不是 OCI 制品，不能用仓库分发、版本化或签名；② 控制机单点推送，内存占用随制品大小线性增长；③ 只有基础组件进了离线包，flannel/cilium/kube-ovn 离线被拒；④ rpm 系没有离线 OS 依赖包。决策见 [04-oci-packages](04-oci-packages.md)（ADR-11～ADR-14）。镜像改写**不采用 Zarf 的 webhook**：kurin 自己管理容器运行时配置，在 containerd/CRI-O/k3s 的 mirror 配置层改写更简单，也不需要集群内常驻组件。

### 2.4 插件 / 生态组件

| | 机制 |
|---|---|
| kurin | 编译期 enum 变体：CNI、HA、发行版、制品（`docs/plugins.md`）；存储与 registry 字段直接平铺在核心模型 `Cluster` 上（`registry_*`、`storage_*`） |
| Kubespray | `roles/kubernetes-apps/*` + `addons.yml` 开关，组件很全（ingress、cert-manager、metallb、各类 CSI…） |
| kubeasz | 内置 addon：DNS、headlamp、metrics-server、prometheus；生态工具：harbor、helm、argocd |
| RKE2 / K3s | **manifests 目录自动部署**（文件即 AddOn CR）+ **HelmChart / HelmChartConfig CRD**（用户覆盖 values，不改原包） |
| KubeKey v4 | playbook 即插件；web-installer 用 **schema** 驱动表单 |
| Zarf | 组件 = chart + manifests + 镜像 + 文件 + 生命周期 actions；变量模板 |
| werf / nelm | 部署 DAG：`werf.io/weight`、`deploy-dependency`；**状态表 + 容器日志的实时跟踪**；`plan` 先看 diff；CRD 每次部署都更新 |

**差异判断**：kurin 需要一个**声明式 addon 格式**（Helm chart 或 manifest + 镜像清单 + values schema + 依赖 + 就绪判定），用 values schema 同时驱动 CLI 校验和 Web 表单（借鉴 KubeKey v4 的 schema 表单、RKE2 的 HelmChartConfig 覆盖思路、nelm 的权重与依赖、Zarf 的组件打包）。

### 2.5 可观测性与日志

| | 部署过程输出 |
|---|---|
| kurin | 结构化 `Event`（无时间戳）+ 各处 `eprintln!`；upgrade/teardown/certs 不走事件流；远端命令输出只在失败时出现 |
| Ansible 系 | 每个 task 的 ok/changed/failed 文本；`-vvv` 才有细节 |
| **nelm/werf** | 每隔几秒打印资源状态表，周期性输出容器日志，可选显示事件，按 annotation 过滤 |
| PangeeCluster / KubeOperator | Web 里看执行日志 |

**差异判断**：kurin 的事件结构是同类里最好的基础，但缺**时间戳、运行时日志行和 K8s 资源就绪跟踪**。决策：引入 `tracing` 与带时间戳的事件 v2；SSH 支持流式 exec，逐行输出；用 kube 客户端做 nelm 式的资源就绪跟踪（ADR-04/ADR-05/ADR-06）。

### 2.6 多集群与 Web

| | 多集群 | Web |
|---|---|---|
| kurin | 每次 `-f` 指定一个文件；本地状态按集群名分目录 | 设计中（[docs/ui](../ui/ui-architecture-final.md)） |
| kubeasz | `clusters/<name>/` 目录区分多个集群 | 无 |
| KubeOperator | 平台内统一纳管，监控、日志、告警一体 | 有（已归档） |
| PangeeCluster | 平台内多集群，资源包管理页 | 有 |
| KubeKey v4 | — | `kk web`（schema 驱动的安装 UI） |

### 2.7 Day-2 能力

| 能力 | kurin | 同类 |
|---|---|---|
| 扩缩节点 / 删除集群 | ✅ | 全部具备 |
| 升级 | kubeadm ✅；k3s ❌ | Kubespray `upgrade-cluster.yml`；RKE2/K3s 有 system-upgrade-controller *（待核实）* |
| 证书 | check/renew ✅ | 普遍具备 |
| **etcd 备份 / 恢复** | ❌ | Kubespray、kubeasz（`ezctl backup/restore`）、RKE（etcd snapshot，可定时） |
| 集群状态记录 | 只在控制机本地缓存 | **RKE1 把完整集群状态存进集群内 ConfigMap**；kubeadm 有 `kubeadm-config` |
| CIS 加固 | ❌ | RKE2 内置 CIS profile |
| CNCF 一致性认证 | ❌ | KubeKey、kubeasz、KubeOperator 均声称通过 |
| OS 覆盖 | Ubuntu 24.04+ / Debian 12 / Rocky 9 | kubeasz 还覆盖 openEuler、RHEL 8/9、Alma、openSUSE 等 |

---

## 3. kurin 的独特优势（要保住）

1. **单静态二进制、零控制机依赖**（musl，无 Python/Ansible/Docker）。同类里只有 RKE2/K3s（节点侧）和 Sealos 接近。
2. **计划与执行分离 + 在线漂移探测**（`--dry-run --check`）。Ansible 的 `--check` 远没有这么可靠。
3. **`Unknown ≠ failed` 的执行语义**：网络中断后先重新探测，不盲目重试。
4. **全制品 sha256 / digest 固定**，国内镜像源有 CI 做 digest 一致性比对（`scripts/check-mirrors.sh`）。
5. **诚实失败**：不支持的组合在校验期就拒绝（`docs/command-matrix.md`）。
6. **严格的配置 schema**：未知字段报错，诊断带源码位置。

后续的重构与扩展都不能破坏这六点，这也是所有工作包验收的隐含前提。

---

## 4. 重大差异（kurin 的缺口）汇总

| # | 缺口 | 参照 | 决策 / 工作包 |
|---|---|---|---|
| G1 | 生态组件扩展要改 Rust 核心代码，没有声明式 addon | RKE2 HelmChart、Kubespray addons、Zarf components、KubeKey schema | ADR-07/08，WP-40～49 |
| G2 | 离线包不是 OCI 制品，不能用仓库分发、签名、增量更新；部分 CNI 和 OS 依赖不能离线 | Zarf、Sealos、PangeeCluster 资源包 | ADR-11～14，WP-30～39 |
| G3 | 没有命令行快速建集群（IP/角色/密码直接传） | Sealos、KubeKey all-in-one | ADR-10，WP-25～28 |
| G4 | 日志没有时间戳，没有远端命令实时输出，没有资源就绪跟踪 | nelm 状态表与日志 | ADR-04～06，WP-05/06/18 |
| G5 | 混合架构不支持：节点制品架构取的是**控制机**架构 | Kubespray 按节点 facts 选架构 | ADR-03，WP-04 |
| G6 | 非 root 用户无法部署：precheck 认可免密 sudo，但实际命令从不提权 | Ansible `become` | ADR-03，WP-03 |
| G7 | 没有 etcd 备份/恢复 | kubeasz、RKE、Kubespray | WP-70 |
| G8 | 集群状态只存在控制机本地，换一台控制机就“失忆” | RKE1 集群内状态 ConfigMap、kubeadm-config | ADR-09，WP-19 |
| G9 | upgrade/teardown/certs 是命令式代码，不走计划/执行器（没有 dry-run、续跑、统一事件） | Kubespray 每种操作都是 playbook | ADR-02，WP-12～14 |
| G10 | k3s 不能升级 | RKE2/K3s 升级路径 | WP-72 |
| G11 | OS 覆盖较窄（无 openEuler/麒麟） | kubeasz、KubeOperator | WP-74 |

---

## 5. 借鉴清单（按优先级）

| 优先级 | 借鉴点 | 来源 | 落地方式 |
|---|---|---|---|
| P0 | 包即 OCI 制品：registry / OCI layout 目录 / 单文件三种传输形态 | Zarf、Sealos | [04](04-oci-packages.md) |
| P0 | 命令行直接传节点与认证信息 | Sealos、KubeKey | [05](05-cli-quick-create.md) |
| P0 | 按节点收集 facts（arch/OS/内核/资源），制品按节点架构选择 | Ansible gather_facts / Kubespray | [02](02-core-architecture.md) §3.4 |
| P0 | 提权（become/sudo） | Ansible | [02](02-core-architecture.md) §3.5 |
| P1 | 声明式 addon：chart/manifest + 镜像 + values schema + 依赖 + 就绪判定 | RKE2 HelmChart、Zarf 组件、Kubespray addons | [03](03-addons.md) |
| P1 | 用户覆盖 values 不改原包（HelmChartConfig 思路） | RKE2 | [03](03-addons.md) §4.3 |
| P1 | 部署 DAG 权重与依赖、资源就绪跟踪、状态表 + 日志输出 | nelm | [02](02-core-architecture.md) §3.7、[03](03-addons.md) §5 |
| P1 | schema 驱动的 Web 表单 | KubeKey v4 web-installer | [docs/ui](../ui/ui-architecture-final.md) §8 |
| P1 | 集群内状态记录 | RKE1、kubeadm-config | [02](02-core-architecture.md) §3.9 |
| P1 | 下载一次再分发，避免每节点各自外网下载或控制机单点推送 | Kubespray `download_run_once`、Zarf 集群内 registry | [04](04-oci-packages.md) §5 |
| P1 | SBOM 与签名校验 | Zarf | [04](04-oci-packages.md) §6 |
| P2 | etcd 快照（含定时）与恢复 | kubeasz、RKE | WP-70 |
| P2 | CIS 加固档位 | RKE2 | WP-75 |
| P2 | 先出 plan 和 diff，再执行；可保存计划后执行 | nelm `--save-plan/--use-plan` | WP-11（计划指纹） |
| P3 | 跑 CNCF conformance（hydrophone/sonobuoy）作为发布门禁 | KubeKey、kubeasz | WP-76 |

### 明确不借鉴

| 不借鉴 | 理由 |
|---|---|
| 通用 playbook DSL（KubeKey v4、Ansible） | 会放弃 kurin 最核心的类型化幂等保证；生态组件用声明式 addon 已经够用 |
| Zarf 的镜像改写 webhook | kurin 掌控运行时配置，mirror 层改写更简单，也不需要集群内常驻组件 |
| Terraform 建机（KubeOperator） | 超出“安装器”边界，交给用户现有的 IaC |
| 平台内置监控、日志、告警（KubeOperator） | 用 addon 装 prometheus 栈即可，平台不重复造 |
| “整个 K8s 打成一个可运行镜像”（Sealos） | 与“二进制优先、按节点 OS 收敛”的路线冲突；只借鉴“命令行体验”和“OCI 分发” |
| Python/Ansible 运行时依赖 | 违背单二进制原则 |

---

## 参考链接

- KubeKey：<https://github.com/kubesphere/kubekey>
- Kubespray：<https://github.com/kubernetes-sigs/kubespray>，离线：`docs/offline-environment.md`、`contrib/offline/`
- Kubean：<https://github.com/kubean-io/kubean>
- kubeasz：<https://github.com/easzlab/kubeasz>
- RKE2 Helm / 自动部署 manifests：<https://docs.rke2.io/add-ons/helm>
- RKE1 → RKE2 差异与 EOL：<https://documentation.suse.com/cloudnative/rancher-manager/v2.11/en/cluster-deployment/rke1-vs-rke2.html>
- KubeOperator（已归档）：<https://github.com/KubeOperator/KubeOperator>
- PangeeCluster：<https://github.com/opencmit/pangee-cluster>，资源包：<https://github.com/opencmit/pangee-cluster-resource-package>
- werf：<https://github.com/werf/werf>；nelm：<https://github.com/werf/nelm>
- Zarf：<https://github.com/zarf-dev/zarf>；init 包：<https://docs.zarf.dev/ref/init-package/>；webhook ADR：`adr/0005-mutating-webhook.md`
- rust-oci-client（ORAS）：<https://github.com/oras-project/rust-oci-client>
