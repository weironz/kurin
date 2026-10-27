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
| `cni/kube-ovn` | ✅ | ✅ | — | ❌ 拒绝 | ✅ | ✅ |
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

## package / artifact（弃用别名）

`kurin package create base` 生成 04 §3 的 OCI 包（一个 image index：
`kurin-package.json` 元数据、每个架构一个文件集 manifest、全部镜像按上游
原 digest 保存）。`push` / `pull` 在任意两种形态间复制，只传目标端缺失的
blob（registry 目标先 HEAD，本地目标按 digest 判断）；`verify` 对 digest
闭包全量重哈希；`list` / `prune` 维护本机内容寻址缓存。包引用支持
`oci://REG/NS/NAME[:TAG]`、`oci-layout://DIR`、`file://X.tar`、
`file://X.tar.zst`（本地裸 tar 路径也接受）。

`--arch` 接受逗号分隔列表（如 `--arch amd64,arm64`）：混合架构集群的
离线包需要覆盖全部涉及架构；缺省为控制机架构。`--cni all|none|calico,cilium`
裁剪 CNI 载荷；`--kubernetes vX.Y.Z` 走动态清单固定非内嵌版本；
`--distribution kubeadm|k3s` 选择发行版。registry 认证用 `--user` +
`--password-env`（或 `~/.docker/config.json`）；`oci://` 里回环地址默认
HTTP，远程仓库默认 HTTPS，`oci://http://…` 可显式指定。

| | amd64 | arm64 |
|---|---|---|
| `package create base -o …` | ✅ | `--arch arm64` |
| `package inspect <src> [--manifest] [--sbom]` | ✅ | ✅ |
| `package verify <src>` | ✅ | ✅ |
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

## 矩阵之外的恒真约束

- 所有 kubectl/k3s 命令经 session 下发到锚点节点执行；控制机（跑 kurin 的机器）无需安装任何集群组件。
- `❌ 拒绝` 是**校验期**的明确报错，不是运行到一半失败。
- kurin-lb / external-endpoint 与 distribution 正交；`role = "lb"` 节点只在这两种模式下出现。
- 离线安装对 Calico / Flannel / Cilium 完整（镜像都在 bundle 内，由组件注册表 `artifacts()` 聚合）；kube-ovn 的 chart 渲染镜像不在 bundle 内，校验期拒绝。
