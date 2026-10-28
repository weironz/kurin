# 命令 × 形态支持矩阵

每一列是一个配置维度。`✅` = 真机 e2e 验证过；`❌ 拒绝` = 配置/命令校验时明确报错（诚实失败，不静默腐蚀）。「create cluster」表由 `just docs-matrix` 从 `kurin capabilities --json`（组件注册表聚合）生成：支持/拒绝列为生成器所有，e2e 验证列手工维护。

## 操作系统 × 包家族（WP-74）

`discover` 读 `/etc/os-release`（`ID` + `VERSION_ID`，`ID_LIKE` 兜底）分类出
`OsFamily`（决定 os-baseline / kernel-deps / OS 包的包名与包管理器）和
`OsRelease`（支持矩阵条目）。未知发行版族为 `Other`：需要包管理器的任务
在校验期/执行期明确报错，不会静默猜测。

| OS | `ID` / `VERSION_ID` | 家族 | 包管理器 | os 包目标 | tier1 e2e |
|---|---|---|---|---|---|
| Ubuntu 24.04 LTS | `ubuntu` / `24.04` | deb | apt-get | `ubuntu-24.04` | ✅ `just tier1` |
| Ubuntu 22.04 LTS | `ubuntu` / `22.04` | deb | apt-get | `ubuntu-22.04` | 人工：`just tier1-ubuntu2204` |
| Debian 12 | `debian` / `12` | deb | apt-get | `debian-12` | ✅（含离线） |
| Rocky Linux 9 | `rocky` / `9.x` | rpm | dnf | `rocky-9` | ✅ `just tier1-rocky*` |
| RHEL 9 | `rhel` / `9.x` | rpm | dnf | `rhel-9`（`redhat/ubi9` 构建） | 人工 |
| AlmaLinux 9 | `almalinux` / `9.x` | rpm | dnf | `almalinux-9` | 人工：`just tier1-alma*` |
| openEuler 24.03 LTS | `openEuler` / `24.03` | euler | dnf | `openeuler-24.03` | 人工：`just tier1-openeuler*` |
| 麒麟 V10（高级服务器） | `kylin` / `V10` | kylin | dnf | `kylin-10`（见下） | 人工：`just tier1-kylin*` |

- 家族差异只有一处包名：RHEL 9（含 rocky/alma 重建版）把 `ebtables` 并进了
  `iptables-nft`，openEuler 与麒麟仍提供独立的 `ebtables` 包（`conntrack` →
  `conntrack-tools`、`nfs` → deb `nfs-common` / rpm `nfs-utils` 不变）。
- `kylin-10` 目标没有可验证的公共容器镜像（麒麟 ISO 走厂商门户）：
  `KYLIN_IMAGE_PLACEHOLDER` 是明确标注的占位，`package create os` 在本地
  导入镜像并 pin 之前直接拒绝构建（`UnverifiedTargetImage`），不会带一个
  无法核实的上游摘要发版。离线 e2e（`kylin-offline.sh`）同为手动。
- Kylin 的 `VERSION_ID="V10"` 带前缀 `V`：匹配与识别先经
  `normalize_version_id` 归一（`V10` → `10`），行标签是 `kylin-10`。
- precheck 的 `os` 检查按小写后的 `ID` 匹配（openEuler 写
  `ID="openEuler"`，POSIX `case` 区分大小写），未列出的发行报告 `warn`。

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
| `[kubernetes] profile` | ✅（仅 v1alpha2） | `"default"`（缺省，行为与历史逐字节一致）或 `"cis"`（WP-75：apiserver/kubelet 参数、审计策略、PSA `restricted`、0600 权限）；v1alpha1 报错提示先 `kurin config migrate`；验收用 `just cis-check`（kube-bench，人工） |

## registry（私有镜像仓库）

`[registry]` 在 v1alpha2 中的默认与可用形态（`role = "registry"` 节点只由
前三种 kind 使用；`external` 使用已有仓库，不需要 registry 节点）：

| kind | 安装位置 | v1alpha1 | v1alpha2 | e2e |
|---|---|---|---|---|
| `docker-registry` | registry 节点 | `kind` 必填 | 显式指定 | ✅ `e2e/tier3/registry.sh docker-registry` |
| `zot` | registry 节点 | `kind` 必填 | 未写 `kind` 时默认 | ✅ `e2e/tier3/registry.sh zot` |
| `harbor` | registry 节点（仅单节点） | `kind` 必填 | 显式指定 | ✅ `e2e/tier3/registry.sh harbor` |
| `seed` | 控制机（只读，`delivery = "seed"`） | ❌ 拒绝（无 `[packages]`） | 需 `packages.base` 或 `artifacts.offline-dir`；不能与 `[registry]` 并存 | 人工：见下 |

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

`seed` 不是 `[registry]` 的 kind，而是 `[packages] delivery = "seed"`
（v1alpha2）：控制机起只读 OCI registry（`registry/seed-serve@controller`），
每个节点经 SSH 远程转发看到 `127.0.0.1:<port>`，运行时 mirror 指向该端点
（`registry/seed-mirror@node`），镜像以 pull 语义到达；二进制仍走 SSH 流式
上传。详见 `docs/reference/configuration.md` §种子仓库。**需要人工 e2e**（无 registry
节点的离线安装，multipass 真机环境）：

```text
# 1. 控制机上准备离线包（或用已有的 file:// 包）
target/debug/kurin package create base -kubernetes <ver> -o file:///tmp/kurin-base.tar.zst
# 2. 无 [registry]、无 registry 节点的 v1alpha2 配置：
#    [packages] base = "file:///tmp/kurin-base.tar.zst", delivery = "seed"
# 3. 离线安装：registry 阶段应出现 seed-serve + 每节点 seed-mirror，
#    images 阶段 no-op（运行时经 mirror 拉取），bootstrap 正常完成
target/debug/kurin create cluster -f seed-cluster.toml
# 4. 验证：节点上 crictl images 的镜像来自 127.0.0.1 拉取；
#    操作结束后节点对 127.0.0.1:<port> 的连接应失效（转发已拆除）
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
| 常规升级 | ✅ | ✅（WP-72：二进制替换 + 滚动重启）|
| 跨 minor +1 | ✅ | ✅（k3s 按发布支持；目标 release 需制品清单 pin 或离线 bundle）|
| `upgrade --offline <dir>` | ✅（旧 bundle 目录） | ✅（bundle 携带目标 k3s 二进制）|

`upgrade` 走计划与执行器，按发行版分支：

- kubeadm：每个节点一条任务链（`version-check` → `cordon` → `drain`
  → `install-bins` → `kubeadm-upgrade` → `restart-kubelet` →
  `ready-gate` → `uncordon`），首个控制面单独执行，其余控制面逐个滚动，
  worker 按 `--batch` 分批。`--offline` 复用同一计划，只替换制品来源
  （接受旧 bundle 目录；包 URI 报"等 WP-35"）。
- k3s（WP-72）：`--with-kubernetes vX.Y.Z` 映射到 k3s release
  `vX.Y.Z+k3s1`（显式 `+k3sN` 后缀原样保留）。每个节点一条任务链
  （`version-check` → `stop-service` → `install-bins`（上传 →
  `cp -a` 保留旧二进制为 `/usr/local/bin/k3s.pre-upgrade-<ts>`（永不删除）
  → 原子 rename）→ `start-service` → `ready-gate`）；server 逐台滚动且
  `ready-gate` 同时等 `/readyz`，agent 按 `--batch` 分批、只等节点 Ready
  （agent 单元探测 `k3s-agent`（上游装法），kurin 自装则是 `k3s`）。
  不 cordon/drain：k3s 官方升级路径就是停机换二进制，排空 server 反而
  清空其内嵌控制面。

  制品来源：在线仅当清单 pin 的正是目标 release（清单不匹配时报错并
  指引 `--offline`，绝不静默下载未校验二进制）；`--offline` 要求 bundle
  按集群每个 arch 携带目标 k3s 二进制（checksum 已由 bundle 校验）。
  已在目标版本的节点为 `InSync`，不停服务/不换二进制。任一步失败中止该
  节点并阻塞后续节点与批次。

## 互斥与续跑

变更操作（`create cluster`、`add nodes`、`delete node`、`delete cluster`、
`upgrade`、`certs renew`、`certs schedule`）对同一集群互斥：锁文件位于
`$XDG_STATE_HOME/kurin/locks/<cluster>.lock`（回退 `~/.local/state`），
第二个进程立即失败并显示持有者的 pid、操作 id、命令行与开始时间。
只读命令（`status`、`precheck`、`certs check`、`logs`、`artifact`、
`config`、`import`）不加锁；`--dry-run` 同样不加锁。

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
- `import --from-cluster`：读取该记录反推配置（见下一节）。

## import --from-cluster（WP-73）

| | k8s（kubeadm） | k3s |
|---|---|---|
| 从 kurin-state 反推配置 | ✅（`kubectl --kubeconfig /etc/kubernetes/admin.conf`）| ✅（`k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml`）|
| worker 节点作 --host | ❌ 拒绝（无控制面 kubeconfig，报错提示换控制面）| ❌ 同左 |
| 记录缺失（WP-19 之前安装） | ❌ 拒绝（明确报错）| ❌ 同左 |

`kurin import --from-cluster --host <cp-ip> [--user root] [--port 22]
[--key PATH | --password-env NAME] [--known-hosts tofu|strict|accept-new]
[-o FILE]` 用单条 SSH 会话直连 `--host`（须为控制面节点；kubectl 形态
按节点实际持有的 kubeconfig 探测），读 `kube-system/kurin-state` 的
`spec.toml` 后回填凭据：

- **凭据**：记录是脱敏的（`******`）。`--password-env NAME` 把每个脱敏
  字段（`[ssh.auth]`、become 密码、`[registry]` 密码、`[addons]` values
  里的脱敏值、节点覆盖）写成 `{ env = "NAME" }` 引用并在 stderr 列清单
  告警；`--key PATH` 只重写 `[ssh.auth]` 为 `private_key`，密码形态字段
  明确报错列出字段清单（改用 `--password-env` 重跑）。两者互斥、必给
  其一；全程只写引用，秘密值不落任何输出（R8）。
- **保真**：`toml_edit` 逐字段改写（键序/注释/非秘密值不动）；v1alpha1
  记录先经 `config migrate` 的结构化迁移升为 v1alpha2；产物必须能被
  配置管道完整加载。
- **输出**：`-o FILE` 0600 落盘；缺省 stdout。`--json` 报告
  `{cluster, distribution, kubernetes_version, nodes, output}`（`nodes`
  仅名字/角色/地址）；`--json` 必须配 `-o`，缺 `-o` 在任何网络动作前
  拒绝。只读操作：不抢集群锁，无 JSONL 操作日志。

## certs check / renew / schedule

| | k8s | k3s |
|---|---|---|
| check | ✅（`/etc/kubernetes/pki` 逐证书表 + 下次续期时间，无 timer 时提示 `kurin certs schedule`）| ✅（`/var/lib/rancher/k3s/server/tls`，同左）|
| renew | ✅（`kubeadm certs renew` → 静态 pod 重启 → `/readyz` 门控，逐节点 `rolling(1)`）| ✅（`k3s certificate rotate` → restart → `/readyz` 门控，逐节点 `rolling(1)`）|
| schedule（WP-71）| ✅（每控制面装 `kurin-cert-renew.{service,timer}` + `/usr/local/bin/kurin-cert-renew.sh`：`kubeadm certs check-expiration` 最小剩余 < `--warn-days`（默认 30）才 `renew all` + 静态 pod 轮转 + `/readyz` 门控；`--every` 默认 24h；`logger -t kurin-cert-renew` 写 journald）| ✅（`k3s certificate check --output table` 门控 + `k3s certificate rotate` + `systemctl restart k3s`；旧版 k3s 无 rotate 时日志给升级指引）|

- `schedule` 抢集群锁，幂等：重跑重写同内容 unit/脚本（`WARN_DAYS`/`OnUnitActiveSec` 随参数更新）并重启 timer。
- 门控只看叶子证书表：kubeadm 的 CA 表不计入（`certs renew all` 续不了 CA，否则过期 CA 会让每个 tick 都重启控制面）；剩余时间解析同时兼容 kubeadm 的 `364d`/`9y` 与 k3s 的 `1 year`/`3 weeks` 写法，过期记 0 天。
- `status` / `certs check` 的下次续期读首个控制面 `systemctl show kurin-cert-renew.timer --property=NextElapseUSecRealtime`（`NextElapseUSecMonotonic` 回退）；未装 timer 时 `cert_next_renewal` 为 `null` / `none`。
- tier2 手动模拟（`--warn-days` 触发）：`kurin certs schedule -f cfg --every 1h --warn-days 3650` → 等 timer 到点（或 `systemctl start kurin-cert-renew.service`）→ `journalctl -t kurin-cert-renew` 依次出现 `minimum residual time …`、`renewing certificates`、`static control-plane pods cycled`、`apiserver /readyz ok after renewal` → `kurin status` 的 `cert_next_renewal` 前移。

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
