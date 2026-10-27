# 命令 × 形态支持矩阵

每一列是一个配置维度。`✅` = 真机 e2e 验证过；`❌ 拒绝` = 配置/命令校验时明确报错（诚实失败，不静默腐蚀）。「create cluster」表由 `just docs-matrix` 从 `kurin capabilities --json`（组件注册表聚合）生成：支持/拒绝列为生成器所有，e2e 验证列手工维护。

## create cluster
<!-- docs-matrix:begin -->
<!-- 由 `just docs-matrix` 生成：支持/拒绝列来自 kurin capabilities --json
     （单一事实来源）；e2e 验证列手工维护，生成器只透传。 -->
| 组件 | containerd | crio | k3s | 离线 | zone=cn | e2e 验证 |
|---|---|---|---|---|---|---|
| `ha/kube-vip` | ✅ | ✅ | ✅ | — | ✅ | ✅ |
| `ha/kurin-lb` | — | — | ✅ | — | ✅ | ✅ |
| `ha/external` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `cni/calico` | ✅ | ✅ | — | ✅ | ✅ | ✅ |
| `cni/flannel` | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `cni/cilium` | ✅ | ✅ | — | ✅ | ✅ | ✅ |
| `cni/kube-ovn` | ✅ | ✅ | — | ✅ | ✅ | ✅ |
| `cni/none` | ✅ | ✅ | ✅ | ✅ | ✅ | — |
<!-- docs-matrix:end -->

## create cluster / create config（快速参数）

`create cluster` 与 `create config` 共用同一组快速参数（`05-cli-quick-create.md`
§3），参数合并进 v1alpha2 文档后走与 `-f` 完全相同的加载、校验与执行路径。

| 能力 | 状态 | 说明 |
|---|---|---|
| 纯参数建集群（无配置文件） | ✅ | `--name` + 至少一个控制面节点；`e2e/tier1/quick.sh` |
| `-f` 基础上覆盖 | ✅ | 合并顺序：文件 → 节点追加 → 具名参数 → `--set` |
| v1alpha1 文件 + 快速参数 | ✅ | 先迁移为 v1alpha2，再合并（注释保留） |
| YAML 文件 + 快速参数 | ❌ 拒绝 | 合并用 `toml_edit`；先转成 TOML |
| IP 列表 / 范围 | ✅ | 末段范围、完整范围、逗号列表；上限 512；IPv6 拒绝 |
| `--node key=value` | ✅ | 名称、地址、角色、内网地址、账号、端口、密码 env/file、私钥、labels |
| `--node-name hostname` | ✅ | discover 后改名；非 DNS-1123 或重名时回退并告警 |
| 密码：`--ask-pass` / `-p` / env / file | ✅ | 明文只在本进程内存；落盘只写秘密引用 |
| `--ssh-key` | ✅ | 私钥认证；passphrase 用 `{ env = "…" }` |
| `--become` / sudo 密码 | ✅ | `--become-password-env`、`--ask-become-pass` |
| `--set PATH=VALUE` | ✅ | TOML 字面量解析，失败当字符串；未知字段在校验期拒绝 |
| `--addon` / `--addon-set` | ✅ | 内置 addon 目录；外部 addon 需 `source`（见 `[addons]`） |
| `--package` / `--offline` | ✅ | `/os/` 路径归入 `packages.os`，其余为 `packages.base` |
| 摘要 + 确认 | ✅ | discover 后打印；非 TTY 且无 `--yes` 报错 |
| `--save` / `--no-save` / `--force-save` | ✅ | 默认 `./<name>.kurin.toml`；内容不同默认拒绝覆盖 |
| `--setup-ssh-key` | ✅ | 密码会话幂等推送公钥（已存在跳过、重放不重复）；验证私钥登录；落盘改私钥认证；`e2e/tier1/ssh-key.sh` |

## registry（私有镜像仓库）

`[registry]` 在 v1alpha2 中的默认与可用形态（`role = "registry"` 节点只由
前三种 kind 使用；`external` 使用已有仓库，不需要 registry 节点）：

| kind | 安装位置 | v1alpha1 | v1alpha2 | e2e |
|---|---|---|---|---|
| `docker-registry` | registry 节点 | `kind` 必填 | 显式指定 | ✅ `e2e/tier3/registry.sh docker-registry` |
| `zot` | registry 节点 | `kind` 必填 | 未写 `kind` 时默认 | ✅ `e2e/tier3/registry.sh zot` |
| `harbor` | registry 节点（仅单节点） | `kind` 必填 | 显式指定 | ✅ `e2e/tier3/registry.sh harbor` |
| `external` | 无（使用已有仓库） | ❌ 拒绝 | `address` 必填；`ca-file`、`namespace` 可选 | 人工：见下 |

`external` 的校验与 WP-36 一致：`address` 必填，`namespace` 只能是斜杠分隔的
小写路径段（默认 `kurin`），`ca-file` 必须存在且只能与 `external` 一起使用；
`external` 指向 zot 时，e2e 把 `address` 写成该 zot 节点的
`https://<ip>:5000` 并复用同一套 `registry/trust` + `registry/push` 任务
（不安装任何东西）。人工命令：

```text
just tier3-registry zot
just tier3-registry docker-registry
KURIN_REGISTRY_EXTERNAL=1 just tier3-registry external
```

## add nodes / delete node / delete cluster

| | k8s（kubeadm） | k3s |
|---|---|---|
| add nodes | ✅（动态版本贯穿）| ✅ |
| delete node | ✅（计划：drain + delete-node-object + `kubeadm reset` + 清理）| ✅（计划：`k3s-uninstall` + 停服务 + `/var/lib/rancher`、`/etc/rancher`）|
| delete cluster | ✅（workers 先、control-plane 后）| ✅ |
| `delete --dry-run [--check]` | ✅（计划树 / 漂移）| ✅ |
| 单节点挂 node | ✅ | ✅ |

`add nodes -f cfg --worker IPS [--control-plane IPS] [--node KV]`（WP-27）
先用 `toml_edit` 把节点追加进配置文件（注释、空行与键顺序保留；自动名称
按现有最大序号顺延，如 `worker-2`），再执行与纯文件路径相同的 add nodes；
`--no-save` 只在内存中合并，不改写文件。`--set` 的未知路径在校验期以
`unknown field` 拒绝（诊断指向“来自命令行参数”的合并文档）。

删除的每一步都是计划中的任务，并带收敛判断（例如目录已不存在即
`unchanged`）。清理步骤是显式的 `BestEffort`：失败会发 `task-warn`
（终端 `warn:`）并在结尾按节点汇总，命令本身仍以 0 退出。锚点控制面
不可达时 API 侧步骤（drain / delete node）跳过并告警，节点本身照常清理。

## upgrade

| | k8s（kubeadm） | k3s |
|---|---|---|
| 常规升级 | ✅ | ❌ 拒绝（换二进制或按目标版本重装）|
| 跨 minor +1 | ✅ | ❌ 拒绝 |
| `upgrade --offline <dir>` | ✅（旧 bundle 目录） | ❌ 拒绝（包来源等 WP-35） |

`upgrade` 走计划与执行器：每个节点一条任务链（`version-check` → `cordon`
→ `drain` → `install-bins` → `kubeadm-upgrade` → `restart-kubelet` →
`ready-gate` → `uncordon`），首个控制面单独执行，其余控制面逐个滚动，
worker 按 `--batch` 分批。`--dry-run` 输出计划树（保留“当前 → 目标”版本
表头）；`--offline` 复用同一计划，只替换制品来源（接受旧 bundle 目录；
 包 URI 报“等 WP-35”）；已在目标版本的节点为
`InSync`，不会 cordon/drain/安装/重启。drain 失败会中止该节点并阻塞后续
节点与批次，且不会 uncordon。

## 互斥与续跑

变更操作（`create cluster`、`add nodes`、`delete node`、`delete cluster`、
`upgrade`、`certs renew`）对同一集群互斥：锁文件位于
`$XDG_STATE_HOME/kurin/locks/<cluster>.lock`（回退 `~/.local/state`），
第二个进程立即失败并显示持有者的 pid、操作 id、命令行与开始时间。
只读命令（`status`、`precheck`、`certs check`、`logs`、`artifact`、
`config`）不加锁；`--dry-run` 同样不加锁。

`create cluster --resume` 会校验计划指纹（任务 key + `describe()` 的
sha256）。计划变化时拒绝续跑并提示去掉 `--resume`；v1 状态文件没有
指纹，仍可续跑。

## status（首控制面不可达时依次尝试其余控制面）

| | k8s | k3s |
|---|---|---|
| node / pod 摘要 | ✅ | ✅（`k3s kubectl` + k3s kubeconfig）|
| etcd 健康 | ✅（pod 计数）| ✅（`/healthz/etcd`）|
| 证书剩余天数 | ✅（`/etc/kubernetes/pki`）| ✅（`/var/lib/rancher/k3s/server/tls`）|
| 期望 vs 观测版本 | ✅（`kube-system/kurin-state` 记录的版本 vs `/version`）| ✅ |

## 集群内状态记录 `kube-system/kurin-state`

每次变更操作（`create cluster`、`add nodes`）的 `post` 阶段服务端 apply
一次 ConfigMap（field manager `kurin`），字段：`kurin-version`、`op-id`、
`op-kind`、`finished-at`、`spec.toml`（规范化且**已脱敏**）、
`components.json`、`addons.json`、`history.json`（最近 20 次操作）。

- `add nodes`：优先从记录读取运行中的 Kubernetes 版本；记录缺失（WP-19
  之前安装的集群）时回退到 `/version` 探测。
- `status`：显示期望版本（记录）与观测版本（`/version`）的差异。
- `delete cluster` 不处理记录（集群已销毁）。

## certs check / renew

| | k8s | k3s |
|---|---|---|
| check | ✅ | ✅ |
| renew | ✅（`kubeadm certs renew` → 静态 pod 重启 → `/readyz` 门控，逐节点 `rolling(1)`）| ✅（`k3s certificate rotate` → restart → `/readyz` 门控，逐节点 `rolling(1)`）|

## etcd backup / restore / schedule（WP-70）

| | k8s | k3s |
|---|---|---|
| backup | ✅（`etcdctl` 经 `crictl exec` 进 etcd 静态 pod，回退 `kubectl exec`；节点留 `/var/lib/kurin/etcd-backups`，首个控制面字节拉回控制机 `-o`）| ✅（`k3s etcd-snapshot save`，节点留 db/snapshots）|
| restore | ✅（停静态 pod → 各成员 `etcdctl snapshot restore` 重建（单/多成员两路）→ 起 → `/healthz/etcd`+`/readyz` 门控；需控制机取上游 etcdctl tarball）| ✅（`systemctl stop k3s` → 锚点 `k3s server --cluster-reset` → 重启 → peer 停旧 db 重新 join）|
| schedule | ✅（`kurin-etcd-backup.timer` + 节点本地脚本，按 `kurin-` 前缀裁剪）| ✅（同上，脚本用 `k3s etcd-snapshot save`）|

- `backup` 只读不抢集群锁；`restore`/`schedule` 取锁。
- 恢复不删旧数据：etcd 数据目录移到 `…-pre-restore-<ts>`（kubeadm `/var/lib/etcd`，k3s `server/db`）。
- 快照文件未加密；多成员恢复要求配置与线上控制面拓扑一致（peer URL 由配置推导）。

## package / artifact（弃用别名）

`kurin package create base` 生成 04 §3 的 OCI 包（一个 image index：
`kurin-package.json` 元数据、每个架构一个文件集 manifest、全部镜像按上游
原 digest 保存）。`push` / `pull` 在任意两种形态间复制，只传目标端缺失的
blob（registry 目标先 HEAD，本地目标按 digest 判断）；`verify` 对 digest
闭包全量重哈希；`list` / `prune` 维护本机内容寻址缓存。包引用支持
`oci://REG/NS/NAME[:TAG]`、`oci-layout://DIR`、`file://X.tar`、
`file://X.tar.zst`（本地裸 tar 路径也接受）。SBOM 与 cosign 签名以
referrer 形式挂在包 index 上随包传输（WP-39）；`verify --key` 校验签名，

离线包需要覆盖全部涉及架构；缺省为控制机架构。`--cni all|none|calico,cilium`
裁剪 CNI 载荷；`--kubernetes vX.Y.Z` 走动态清单固定非内嵌版本；
`--distribution kubeadm|k3s` 选择发行版。registry 认证用 `--user` +
`--password-env`（或 `~/.docker/config.json`）；`oci://` 里回环地址默认
HTTP，远程仓库默认 HTTPS，`oci://http://…` 可显式指定。

`kurin package create addon -f addons/<name>/addon.yaml` 生成一个 addon 包
（04 §3.4）：addon 文件、`oci://` chart（拉进包并改写 `addon.yaml` 指向包内
路径）与 `spec.images` 的每个镜像（按 digest，含所选架构的子 manifest 与
blob）。`package create base --addons a,b` 把所选 addon 嵌进 base 包（文件在
`addons/<name>/`，镜像并入包的镜像集合），一个 base 包即可离线安装这些
addon；部署时 addon 镜像按 `[packages] delivery` 走 preload（逐节点导入）或
registry（`registry/push` 推一次 + 运行时 mirror）。

| | amd64 | arm64 |
|---|---|---|
| `package create base -o …` | ✅ | `--arch arm64` |
| `package create base --addons a,b` | ✅ | `--arch arm64` |
| `package create addon -f addon.yaml` | ✅ | `--arch arm64` |
| `package inspect <src> [--manifest] [--sbom]` | ✅ | ✅ |
| `package verify <src> [--key PUB]` | ✅ | ✅ |
| `package push <src> <dst>` / `package pull` | ✅ | ✅ |
| `package list` / `package prune` | ✅ | ✅ |
| `artifact --out <dir>`（弃用别名）| ✅ | `--arch arm64` |

## logs

每次操作（`create cluster`、`add nodes`、`delete node/cluster`、`upgrade`、
`certs check/renew`、`precheck`、`status`）都会把事件信封写入
`$XDG_STATE_HOME/kurin/logs/<cluster>/<op-id>.jsonl`（回退
`~/.local/state`），并在操作开始时把路径打印到 stderr 一次。

| 参数 | 行为 |
|---|---|
| `--cluster C` | 指定集群；只有一个集群有日志时可省略 |
| `--op ID` | 按操作 id（或唯一前缀）选择日志文件 |
| `--last` | 读取最新一次操作（缺省行为） |
| `--follow` | 持续输出新追加的信封 |
| `--node N` | 只显示属于该节点的行（含远端 stdout/stderr） |
| `--level L` | `trace`/`debug`/`info`/`warn`/`error`，缺省 `info`；`-vv` 等价于 `debug` |
| `--json` | 每行输出一个完整 v2 信封，便于管道处理 |

全局参数：`--timestamps`（终端日志加 UTC 时间戳）、`-v`（info 级进度 + 每条命令的远端输出摘要）、
`-vv`（全部远端 stdout/stderr）、`--json`/`--json=v1`（冻结的 v1 事件，
新增 `ts` 字段）、`--json=v2`（完整信封）、`--max-upload-parallel N`
（同一操作内并发上传的载荷数，缺省 4；上传共享控制机上行带宽，与任务并发分开限流）。

## addon（WP-44，03 §4.6）

`create cluster` 会一次装好配置里启用的全部 addon；之后用 `kurin addon`
单独收敛某一个 addon，不需要重跑整个安装计划。

| 命令 | 行为 |
|---|---|
| `addon list [-f cluster.toml]` | 目录中的 addon（内置目录 + 配置里 `source` 指向的目录/包）与集群 `kurin-state` 里已装的清单；`STATUS` 为 `installed`/`enabled`/`disabled`/`available`。给 `-f` 时连接集群读 `kurin-state`，读不到只告警并列出目录 |
| `addon info <name> [--schema]` | 元数据（版本、分类、命名空间、安装方式、镜像、依赖）；`--schema` 只输出 values JSON Schema（可直接管道） |
| `addon install <name> -f cfg [--version V] [--set k=v] [--values f.yaml] [--dry-run]` | 启用并收敛该 addon；`requires` 的条目一并启用 |
| `addon upgrade <name> -f cfg [--version V]` | 按目录提供的版本（或 `--version`）重新收敛，并刷新配置里的 `version` |
| `addon remove <name> -f cfg [--keep-crds]` | 卸载并把配置里该条目置为 `enabled = false`；`--keep-crds` 保留 CRD |

- 配置改写用 `toml_edit`：注释、空行与键顺序保留；`--no-save` 关闭改写。
  改写发生在操作**成功之后**——失败的安装不会把配置写成“已启用”。
- `--set` 的值按 TOML 字面量解析（失败当字符串），键用 `.` 嵌套；
  `--values` 的 YAML 逐叶子合并进 `[addons.<name>.values]`，未涉及的键与注释保持不变。
- 写入前按 addon 的 `values-schema` 校验 `values`，越界参数在校验期拒绝。
- `--dry-run` 只打印计划（`discover` → `addons/<name>` → `post`），不连接节点、不改配置。
- `--version` 必须是目录当前提供的版本；其它版本需要先为该版本提供 `source` 或拉取 addon 包。
- 每个操作只收敛目标 addon（及其 `requires`），不影响配置里其它 addon。

## 矩阵之外的恒真约束

- 所有 kubectl/k3s 命令经 session 下发到锚点节点执行；控制机（跑 kurin 的机器）无需安装任何集群组件。
- `❌ 拒绝` 是**校验期**的明确报错，不是运行到一半失败。
- kurin-lb / external-endpoint 与 distribution 正交；`role = "lb"` 节点只在这两种模式下出现。
- 离线安装对 Calico / Flannel / Cilium / Kube-OVN 完整（镜像都在 bundle 内，由组件注册表 `artifacts()` 聚合）。
