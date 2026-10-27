## WP-78：英文文案与文档站（2026-10-28）

- **`web/src/i18n/en.ts`**：593 个键全量英文翻译，`Dictionary = Record<MessageKey, string>` 由 zh 字典派生，缺键/多键编译期报错；占位符逐键校验。
- **运行时切换**：`i18n/index.ts` 提供 `t/setLocale/useLocale`，默认 zh-CN，`localStorage['kurin.locale']` 持久化；`App.tsx` 的 antd `ConfigProvider` 与 dayjs locale 跟随，`AppShell` 头部「中文 / English」切换无需刷新；页面标题随 locale 更新。
- **测试**：189 Vitest（键集/占位符/回退/持久化/真实 App 切换）+ 8 Playwright（zh→en→reload 持久化→切回）。
- **文档站**：`docs/site/` mdBook 脚手架（`book.toml`、`SUMMARY.md`、`sync.sh` 从 `docs/` 同步 23 章，`src/`/`book/` gitignore）；本机未装 mdBook，构建命令见 `docs/site/README.md`。

# 进度日志

## WP-71：证书自动续期（2026-10-28）

- **`kurin certs schedule -f <cfg> [--warn-days 30] [--every 24h]`**：每个控制面装 `kurin-cert-renew.{service,timer}`（oneshot + `OnBootSec`/`OnUnitActiveSec`，`WantedBy=timers.target`）与 `/usr/local/bin/kurin-cert-renew.sh`（0755）。脚本门控在 `kubeadm certs check-expiration`（k3s：`k3s certificate check --output table`）的最小剩余天数——小于 `WARN_DAYS` 才动手：kubeadm `certs renew all` → 静态 pod manifests 轮转（15s）→ `/readyz` 门控（120s，有 kubectl 时 best-effort）；k3s `certificate rotate`（旧版 k3s 无该子命令时日志给升级指引）→ `systemctl restart k3s` → `/readyz`（180s）。全程 `logger -t kurin-cert-renew` 写 journald；kubeadm 的 CA 表不计入门控（`renew all` 续不了 CA）。重跑幂等（重写同内容文件并重启 timer，参数变化随之生效）；`--every` 复用 WP-70 的解析器；`schedule` 抢集群锁。
- **`status` / `certs check` 显示下次续期**：`StatusReport` 新增 `cert_next_renewal`——首个控制面 `systemctl show kurin-cert-renew.timer --property=NextElapseUSecRealtime`（`NextElapseUSecMonotonic` 回退），未装 timer 时 JSON 为 `null`、人读输出提示 `none`；`certs check` 表格后追加同一行。既有字段不变。
- **测试**：`kurin-components` certs 单测 11 个（脚本/单元渲染 kubeadm+k3s、门控 awk 对 kubeadm/k3s 报告样例的解析——`364d`/`9y`/`1y57d`/`1 year`/`3 weeks`/`<invalid>` 全覆盖、timer/service 断言、next-renewal 解析）；`kurin/tests/certs_schedule.rs` 5 个集成测试（mock Session：安装文件内容、enable+restart 命令、幂等重装、k3s 流程、`systemctl show` 探测与缺失回退）；`kurin/tests/status.rs` 增 1 个（timer 探测进报告、既有字段不变）。
- **e2e（tier2，待人工）**：`--warn-days` 模拟触发——`kurin certs schedule --every 1h --warn-days 3650` 后 `journalctl -t kurin-cert-renew` 应依次出现 `minimum residual time …`、`renewing certificates`、`static control-plane pods cycled`、`apiserver /readyz ok after renewal`，`kurin status` 的 `cert_next_renewal` 前移（完整步骤见 command-matrix「certs check / renew / schedule」）。节点本地脚本已用 stub 命令做过全流程验证（门控、续期、pod 轮转、readyz、journald 标签）。

## WP-70：etcd 备份/恢复/定时（2026-10-28）

- **`kurin etcd backup [-o FILE]`**：每个控制面各拍一份快照（kubeadm 经 `crictl exec` 进 etcd 静态 pod、回退 `kubectl exec`；k3s 用 `k3s etcd-snapshot save`），`etcdctl snapshot status` 校验 revision/size 后在节点保留于 `/var/lib/kurin/etcd-backups`（k3s 在其 db/snapshots），首个控制面的字节经分块 `dd | base64 -w 4096`（6 MiB/块）拉回控制机，远端 sha256 全量校验后写入 `-o`（默认 `./etcd-<cluster>-<ts>.db`）。
- **`kurin etcd restore --from FILE`**：kubeadm——先在集群仍在跑时把上游 etcdctl v3.5.21 tarball（控制机缓存、sha256 实抓）与快照上传到各节点，再移走 `/etc/kubernetes/manifests` 停止 etcd+apiserver 静态 pod，各成员 `etcdctl snapshot restore` 重建 etcd（单成员无 flags；多成员用 `--name/--initial-cluster/--initial-advertise-peer-urls/--initial-cluster-token` 重建 quorum），移回 manifests，`/healthz/etcd` + `/readyz` 门控；k3s——全部 `systemctl stop k3s`，锚点 `k3s server --cluster-reset --cluster-reset-restore-path` 重置为单成员，重启并通过健康门控后，其余 server 停用旧 db（移到 `…-pre-restore-<ts>`）重新 join。旧数据一律保留不删。
- **`kurin etcd schedule --every 6h --keep 7`**：每个控制面装 `kurin-etcd-backup.{service,timer}`（`OnBootSec`/`OnUnitActiveSec`）与 `/usr/local/bin/kurin-etcd-backup.sh`（节点本地快照 + 只按 `kurin-` 前缀裁剪到 keep 份；脚本幂等，重跑仅重写同内容文件并重启 timer）。
- **测试**：`kurin-components` 22 个单测（`--every` 解析、kubeadm/k3s 命令构造、systemd 单元渲染、DR flags、分块传输、etcdctl pin）；`kurin/tests/etcd.rs` 8 个集成测试用 mock Session 断言命令序列与顺序（stop→park→restore→start→health gate、kubectl 回退、多成员 flags、k3s 锚点/peer 流程、schedule 幂等）。
- **e2e（tier1，已执行）**：`just tier1-etcd` **PASS**——备份 6.6MB 快照 → 建 `kurin-etcd-e2e` namespace → 恢复 → namespace 消失、`/readyz` 与 `/healthz/etcd` 恢复。k3s 路径仍需人工在 `e2e/tier1/k3s.sh` 建的集群上验证。

## WP-66：Web 真实环境 e2e（tier1，2026-10-28）
- **`web/e2e/lifecycle.spec.ts` PASS（3.9m）**：单节点 tier1（multipass Ubuntu 24.04）上通过页面完成创建 → 状态（运行中）→ 加 worker → 删 worker → 销毁；服务端操作全部 `succeeded`。
- **`web/e2e/resilience.spec.ts`「创建中关闭浏览器」PASS（2.4m）**：关闭浏览器后 create 在服务端继续，重开页面可见操作详情并最终成功。
- **真实 e2e 暴露并修复的集成问题**：
  - `GET /clusters/{name}` 缺 `desired_nodes`/`observed_nodes`/`config_summary`，节点 Tab 在真实数据上崩溃（`s is not iterable`）；前端新增 `api/clusters.ts` 适配 snake_case。
  - `OperationView` 未序列化 `result`，向导 precheck/plan 结果永远为空。
  - create/add-nodes/delete-cluster 从不更新 lifecycle（永远 draft）；collector 只采集 active，形成死锁。
  - `ContainerdConfig` 先于 `ContainerdUnit` 执行，新主机上 `systemctl restart containerd` 失败；加 DAG 边。
  - SPA 回退把带点的 operation id（`…255Z-…`）当资产返回 404，deep link 打不开。
  - 向导发送 `artifacts.zone = "global"`（只接受 `cn`），render 报错被空文档掩盖成 `missing version`。
  - precheck 在去抖 render 产出 TOML 前可点，空文档登记失败。
- **待人工**：`resilience.spec.ts` 的服务重启 retry、CLI 抢锁、密码明文扫描（需 `KURIN_E2E_RESTART_CMD` / `KURIN_E2E_CLI` / `KURIN_E2E_SECRET`）；tier2 升级场景。

## WP-66：Web 真实环境 e2e 套件（2026-10-28）

- **新增 `web/e2e/`**（独立 Playwright 配置，`pnpm test:e2e:real`）：创建→状态→加 worker→删 worker→销毁、创建中关闭浏览器、重启 serve 后 interrupted→retry、CLI 抢锁、密码明文扫描（DB/事件/日志/API）。未设置 `KURIN_E2E_URL` 时全部 skip；环境变量见 `web/e2e/README.md`。
- **加节点 UI**：集群详情「节点」Tab 的「添加节点」批量粘贴 → `/nodes/parse` → `POST /clusters/{name}/nodes`（服务端 toml_edit 追加修订 + `add-nodes` 操作），补上 UI 文档 M2 的 `add nodes` 入口。
- **服务端补齐**：`POST /clusters/{name}/nodes`（WP-54 遗漏的结构化加节点端点）。
- **待人工**：在 tier1/tier2 执行 `pnpm test:e2e:real`，把通过/失败项记录到本文件；本环境无 multipass，未执行。

# 进度日志

## rpm 系 OS 支持（gap #4，2026-10-27）

- **prepare/os-baseline**（新任务，所有 k8s/registry 节点）：swapoff + fstab
  注释、SELinux enforcing→permissive（写 `/etc/selinux/config` 持久）、firewalld
  stop+disable（kubekey 语义）；apply 后重探测，未收敛则 `Unknown`（执行器会再
  check 并 reconciled/fail）。precheck 会先报告这些项，操作者有预期。
- **模块持久化**并入 Sysctl：`/etc/modules-load.d/kurin.conf`（overlay、
  br_netfilter）+ modprobe 先于 `sysctl --system`（bridge sysctl 依赖模块）。
- **kernel_deps 包名按族映射**：rpm 下 conntrack→conntrack-tools、
  ebtables→iptables-ebtables；dnf/yum 双支持。external_lb 保持 apt/dnf。
- 侦察结论（scout）：仓库里真正的 Ubuntu 假设只有 4 处（kernel_deps、
  external_lb、preflight、无 firewalld 处理）；multipass 无 rpm 镜像，
  e2e 走 Rocky GenericCloud qcow2 URL 直启（cloud-init 注入 root key）。
- e2e：`e2e/tier1/rocky.sh` **PASS**（Rocky 9.8 单节点 kubeadm：cluster-ready
  零失败、os-baseline 执行、pod 全健康、selinux=Permissive / swap=off /
  firewalld=inactive）。镜像下载：官方 CDN ~1MB/s，新加坡 mirror 23MB/s。

## WP-16：Component trait（二）与能力表（2026-10-28）

- **全部内置组件迁移完成**：ha/kube-vip、ha/kurin-lb、ha/external、cni/*（calico、
  flannel、cilium、kube-ovn、none）、registry/*（zot、docker-registry、harbor）、
  storage/*（local-path、nfs；WP-47 再迁 addon 层）、post/*（kubeconfig、summary、
  labels）共 16 个新组件注册进 `kurin_components::registry()`（连同 WP-15 的 7 个共
  23 个）。`plan.rs` 的 `plan_<area>` 函数全部删除，构建只剩"遍历 enabled 组件调
  plan()"。
- **能力表成为单一事实来源**：新增隐藏子命令 `kurin capabilities --json`（输出
  组件 id、kind、distributions、archs、offline、zones、ops，快照测试锁定）；
  `just docs-matrix` 由 `scripts/docs-matrix.py` 从它生成 command-matrix.md 的
  create cluster 表（支持/拒绝列归生成器，e2e 列手工维护）。
- **制品聚合**：`artifacts(spec, arch)` 逐组件声明；`kurin artifact` 改为迭代
  `bundle_artifacts()`（注册表聚合），离线镜像集合不再手工维护；
  `images/import` 与 registry seed 的镜像集合也改由 `bootstrap_images()` 聚合
  （runtime + distro + 所选 CNI），对照测试锁定与原手写清单完全一致。
- **校验迁移**：kube-vip/kurin-lb/external 的 HA 规则、registry 的节点/凭据/harbor
  规则、storage 的 nfs 规则、kube-ovn 的离线拒绝规则全部搬进各组件 `validate()`
  （错误文本逐字保持，kurin-config 只留纯结构校验 + "registry 节点无 kind"这一
  条没有启用组件可归属的结构规则）。
- 快照零变化；`just check` 通过。需人工 e2e：`just tier1-offline`、`just
  tier3-external-lb`。

## 对标 kubekey：precheck / registry / storage（2026-10-27）

- **`kurin precheck`**（gap #1）：只读主机体检，单次 SSH 往返，per-check
  ok/warn/FAIL；角色相关端口（CP 6443/2379/2380/10257/10259、LB 6443/8404、
  worker 10250/30000）；swap、内核模块（可加载即可）、sysctl（模块提供的键
  在模块可加载时视为 ok）、内存/CPU/磁盘、NTP、SELinux、运行时 socket、
  hostname 解析；`--json` 逐条事件 + precheck-done 汇总。e2e：干净 VM 全绿、
  开 swap 报 FAIL、swapoff 恢复。
- **私有 registry 三种实现**（gap #2，`role = "registry"` 节点，
  `Stage::Registry`）：`docker-registry`（distribution 3.1.2 二进制）、
  `zot`（2.1.22 二进制，开 `docker2s2` 兼容）、`harbor`（2.14.6 离线安装器 +
  其自带 docker/compose）。kurin 生成一次 CA，给每个 registry 节点签 leaf；
  trust 写 containerd `certs.d/<upstream>/hosts.toml`（mirror + CA + basic auth）
  与 `certs.d/<reg>:port/`；seed 把 bundle 选中镜像按原始 repo 路径推进 registry。
  **三种实现 tier-3 e2e 全 GREEN**（含断上游后经 mirror 拉取、二次运行全 skip）。
  关键坑：containerd 必须设置 `registry.config_path`（否则 certs.d 不生效）、
  zot 415（schema2 兼容）、distribution `auth` 顶层键、harbor 命名空间限制
  （seed 到 `library` + kubeadm `imageRepository`）、CA 曾每 task 重铸、
  harbor.yml 需从模板 sed、base64 表的 `UVWYYZ` 错字。
- **存储插件**（gap #3）：`[storage] kind = "local-path" | "nfs"`，
  local-path（rancher v0.0.31，含 busybox helper）与 NFS subdir provisioner
  （v4.0.2）内嵌 manifest，StorageClass 命名 `kurin-local-path`/`kurin-nfs`，
  `default = true` 加默认注解；nfs 校验要求 server+path。

## 离线 CNI 矩阵 + k3s×external-endpoint + 发布工程（2026-10-27）

- **离线 CNI 补齐**：flannel（2 镜像）+ cilium（3 镜像，版本取自 cilium-cli
  `--dry-run` 渲染的实际引用）加入 bundle manifest（digest 均实抓 HEAD 解析）。
  `images/import` 改为与所选 CNI 匹配（不再一律全量导入 15 个镜像）。
  校验放开了 flannel/cilium × offline（kube-ovn 仍拒绝）。
  **tier-1 断网 e2e PASS（flannel、cilium）**：egress iptables 全断后镜像来自
  bundle（ghcr.io/quay.io 引用在 containerd 里可查）且集群收敛。
- **GHCR 匿名 token 流**：ghcr.io v2 匿名 401 → 需走
  `ghcr.io/token?scope=repository:<repo>:pull` 获取 token；extract 时
  `format!("{}", json["token"])` 会把 JSON 字符串的引号带进 Authorization 头 →
  403（修复如 HEAD 解析）→ 200。
- **k3s × external-endpoint 放开**：校验中可捆绑，并不在配置；K3sJoin/
  K3sApiReady 默认终端的 VIP 已通用的；post/k3s-kubeconfig 复制 kubeconfig 时
  将 server 从 127.0.0.1 改写为 VIP（与 kubeadm track 齐平）。tier-3 e2e
  （3 server + 1 手工 haproxy）新增。
- **`operation timed out after 300s`**（russh WRITE_TIMEOUT）：离线 cilium 镜像
  tar 走 SSH stdin、单包 >300s；memcpy 发导引文件时 WRITE_TIMEOUT 300s 少于
  EXEC_TIMEOUT 900s → 涨到 1800s。
- **发布工程**：`just release <ver>` = release-musl + tag + 从 CHANGELOG.md 提取
  该版本段落做 notes + gh release create 四资产。CHANGELOG.md 从 v0.1.0 补齐。


## 命令 × 发行版矩阵审计（2026-10-26）✅

修掉四个被主线验收绕开的矩阵缺口（命令 × k3s/crio/offline），新增 `docs/command-matrix.md`：

- **delete 对 k3s**：teardown 重写为发行版感知——k3s 停服务后清 `/etc/rancher`、`/var/lib/rancher`、k3s 二进制，**不跑 kubeadm reset**；crio 运行时补 `/etc/crio`、crio/pinns 二进制等残留路径；kubectl helper 按 kubeadm/k3s 分派（`k3s kubectl --kubeconfig /etc/rancher/k3s/k3s.yaml`）。
- **status 对 k3s**：`kubectl_base` 按发行版分派（k3s kubeconfig + HA 本机 127.0.0.1）；etcd 健康 k3s 走 `/healthz/etcd`（内嵌 etcd 不在 pod 里，pod 计数会误报）；cert_min_days 委托 certs::check（单一事实源）。
- **certs 对 k3s**：探测根目录 kubeadm `/etc/kubernetes/pki` / k3s `/var/lib/rancher/k3s/server/tls`；`renew` k3s = `k3s certificate rotate` + restart + `/readyz` 门控（180s），旧版 k3s 无 rotate 时给明确升级指引。
- **upgrade 对 k3s**：校验期明确拒绝（kubeadm 流程会损坏 k3s 节点；带离线换二进制/重装指引）。
- **离线一致性**：flannel/cilium 与 kube-ovn 同语义——离线 bundle 只有 calico，其余 CNI 配置校验期拒绝（原来会一路跑到 CNI 阶段 Pull 失败）。
- e2e：`e2e/tier1/k3s-ops.sh`（status + certs check/renew + delete 全链路真机断言）、`just tier1-k3s-ops`。

## Kube-OVN + haproxy 细化 + CI 手动化（2026-10-26）✅

- **Kube-OVN CNI**（`network.cni.kind = "kube-ovn"`）：上游只发 helm chart，故把 v1.16.15 的
  `helm template` 输出固化进 `kurin/templates/cni/kube-ovn.yaml`（39KB），运行时按集群
  CIDR/注册表改写。**上游 install.sh 的隐式步骤**：给控制面打 `kube-ovn/role=master`
  标签（否则 ovn-central 永久 Pending、控制器连不上 NB）——已内置。
  tier-1 e2e PASS（controller/ovs-ovn/ovn-default subnet 全部收敛、pinger 网关可达、二遍全 skip）。
  明确限制：kube-ovn 镜像未入离线 bundle，离线安装校验拒绝（诚实失败）。
- **haproxy 细化**：后端健康检查从裸 TCP 改为 HTTPS `GET /readyz`（`http-check expect
  status 200` + `check-ssl verify none`，数据面保持 TCP 透传）；`:8404/haproxy-stats`
  统计页；e2e 用 `;csv=true` 断言全部后端 UP。
- **实机 triage 三连**：minijinja 默认丢弃模板末尾换行 → haproxy 2.8 视为截断拒绝启动
  （`set_keep_trailing_newline(true)` + 回归测试）；`default-server ssl` 会把客户端 TLS
  二次包一层（已移除，仅 check-ssl）；e2e stats 断言中的 awk 字段错位。
- **CI 手动化**：check/mirrors 改为仅 `workflow_dispatch`（push 触发的 CI 与本机 e2e
  抢内存导致连续失败堆积）；release 保留 tag 触发并加手动入口。

## Tails 收尾（2026-10-26）✅ 全部完成

四轨并行 + 集成，全部真机验证：

| 尾项 | 结果 |
|---|---|
| CRI-O 运行时（`kubernetes.runtime = "crio"`） | tier-1 PASS：node runtime `cri-o://1.33.4`；containerd 制品自动跳过；criSocket 切换；启动竞态修复（同 stage 前置等待） |
| k3s 离线（airgap tar） | tier-1 断网 PASS：airgap tar 落 `/var/lib/rancher/k3s/agent/images/`，二遍全 skip |
| k3s × kurin-lb | tier-3 PASS：2 CP + 1 LB，keepalived 持 VIP、无 kube-vip pod、VIP /readyz、全 pod 健康 |
| 离线升级（`artifact --with-kubernetes` + `upgrade --offline`） | tier-2 断网升级 1.33.13 → 1.34.12 PASS，二遍 up-to-date |
| k3s arm64 资产名 | `{asset_arch}` 模板变量；arm64 bundle 实测（k3s-arm64 二进制下载校验通过） |
| 安装报告 | `post/summary` 哈希门控（报告变化才重报，不破幂等）；tier-2 二遍全 skip 已回归 |

集成修复（e2e 暴露）：plan 的 runtime gate 两次静默未生效（crio 集群仍装 containerd）；
k3s 分支 ExternalLb 重复规划（stage_tasks 已含）；Summary "always drift" 破坏二遍幂等；
crio unit 与安装/配置并行启动竞态。

## 文档补齐（2026-10-26）
## M2 步骤 1 — upgrade ✅（2026-10-26）

（M2 后续全部任务清单见 `docs/roadmap.md`：批次 1 证书轮转/CP 追加/status；批次 2 国内镜像、CNI 矩阵、K3s、外部 LB；批次 3 跨 minor 升级、离线增强、夜间 e2e 备选方案。约定：e2e 全手工，主线功能开发。）

`kurin upgrade -f cluster.toml --with-kubernetes vX.Y.Z [--dry-run] [--batch N]`

- **动态版本表**：kubeadm/kubelet/kubectl 的 sha256 从 `dl.k8s.io` 官方 `.sha256` 端点实时获取并校验下载（内容寻址），目标版本无需重编译。
- **顺序**：init 节点 `kubeadm upgrade plan/apply` → 其余控制面串行 `kubeadm upgrade node` + drain/uncordon → worker 按 batch 推进；每节点按 kubelet 版本幂等跳过。
- 实机验证（tier-2）：1.33.1 → **1.33.13** 成功，双节点 Ready；重跑全部 `up-to-date`。
- **重大 bug（e2e 挖出）**：直接 `cat > /usr/local/bin/kubelet` 会因该文件正被 kubelet 进程持有而 `ETXTBSY`，远端 cat 秒退后 russh 通道挂在已关闭窗口上，整个上传静默卡 300s。修复：所有二进制替换改为「临时名写入 + `mv -f` 原子改名」（install 与 upgrade 两条路径都改），并加 `/usr/local/bin/*.kurin-new` 清理。探针二分证明与大小无关、只与目标 inode 是否被占用有关。

## W2 — 发布工程与 CI（2026-10-26）

- **just 取代 Makefile**：`just -l` 列出全部 recipe；`check`（fmt + clippy -D warnings + 单测）、tier0-3、`tier1-offline`、`release-musl`、`ci-gate`。
- **musl 静态双架构**：x86_64 走 musl-gcc，aarch64 走 `cargo zigbuild`（zig 装在 /usr/local/bin）；`just release-musl` 产出 tarball + 裸文件名 sha256 sidecar。
- **install.sh**：`KURIN_BASE_URL` 可覆盖；本地 http 端到端实测（下载 → 校验 → 安装 → 可跑）。
- **CI（.github/workflows/）**：push/PR 跑 check + tier0，tag v* 跑 musl release（self-hosted runner；依赖 rustup+just+zig+docker）。
- **clippy 清零**：修 8 处真 lint，`result_large_err` 项目级 allow（理由注释在 kurin/src/lib.rs）。
- 国内镜像源维持留口不实现（2026-10-26 决策）。

## W0 — 契约冻结（2026-10-25）✅

- **kurin-config**：`v1alpha1` schema（TOML/YAML、deny_unknown_fields、tagged enum、强类型）、四段流水线、miette 行列诊断。
- **kurin**：`Task`（check/apply + `Verified/Unknown`）、`Session`、petgraph 线性阶段图、dry-run 树渲染、rust-embed 模板、编译期版本表、CLI。

## W1 — 七轨并行（2026-10-26）✅

| 轨 | 交付 | commit |
|---|---|---|
| A kurin-config | DNS-label/地址唯一/CIDR 范围/vip 冲突校验、YAML 示例等价测试、11 快照 | `26ec316` |
| B templates | systemd unit、join 配置、calico CR、kube-vip 模板；消灭 format! YAML | `a85d2a2` |
| C artifact | 制品清单（9 项双架构 sha256 上游实抓）、缓存/下载/校验、离线 from_dir、11 单测 | `ca2672b` |
| D ssh | russh SshSession/Pool（复用/限额/退避/超时/TOFU）、docker 实测 3 用例过 | `8d0a9a4` |
| E engine | executor 重写（实时事件流、fail-fast、resume 状态文件、probe）、14 单测 | `b6421d3` |
| F e2e | tier-0 Makefile + 3 systemd 容器 + 断言脚本（降级策略） | `49d60f9` |
| G 版本表 | 全组件 sha256 收集脚本 + `ARTIFACT_SHA256` 表（版本核实无需改号） | `9daed33` |

## 集成（W1 → M1 步骤 1+2）✅

- tokio features 补 `process/fs/signal`；lib.rs 声明 `artifact`/`ssh`/`state`。
- plan.rs 接线 TrackB 新任务（ContainerdUnit/KubeletUnit/KubeVip）。
- **新增 `artifacts` 阶段**（preflight 与 prepare 之间）：`ArtifactsInstall` 任务经 ArtifactStore 取缓存/下载校验后，把 kubeadm/kubelet/kubectl/containerd/runc/crictl/cni-plugins 装到 `/usr/local`、`/opt/cni/bin`，marker 记录 sha256 供 check。并行下载全局串行化避免缓存竞态。
- main.rs 接真实执行路径：`SshSessionPool::from_cluster` → `probe`/`run_with`，`--json` 逐行事件、`--resume` 状态文件。
- 全 workspace 测试 72+ 全绿。
- **tier-0 全量 PASS（干净环境）**：3 systemd 容器，dry-run 确定性 + 阶段序列 + 首遍 18/18 settle（17 applied，1 因镜像自带状态 skip）+ 二遍 18/18 全 skip（幂等）。
- e2e 暴露并修复的 4 个真缺陷：containerd tgz `--strip-components` 剥错目录 → unit 指向消失的二进制；marker 不含安装配方版本 → 配方修复无法生效（marker 现为 `sha@r<N>`）；sysctl check 用 `stdout.trim()` 比带尾换行的 expected → 永久漂移；任务 id 前缀与 stage 不一致（`prepare/artifacts@` → `artifacts/install@`）。

## M1 步骤 4 — 多节点 ✅（2026-10-26）

- **join 真正接线**：bootstrap 在 init 后收割 `kubeadm token create --print-join-command` + `upload-certs` key，写入本地 `~/.cache/kurin/clusters/<name>/join.env`（密钥不进模板、不持久化到节点）；join 任务读取后以 flags 形式（`--node-name`/`--apiserver-advertise-address`/CP 的 `--control-plane --certificate-key`）执行——kubeadm 禁止 `--config` 与 discovery flags 混用。
- **host key 语义**：TOFU 记录失配时不再盲目重试 3 次，改为 fail-fast 并给出「VM 重建需删除 ~/.cache/kurin/known_hosts 对应条目」指引；harness 在自己管理的 IP 上主动清理旧条目。
- **CNI 竞态**：Calico ≥3.26 的 CRD 由 operator 运行时创建，`kubectl wait` 会撞上 nil conditions；两个 readiness wait 改为带超时的容错轮询。
- **tier-2 干净循环 PASS**（新 VM×2 → 安装 → 断言 → 幂等，exit 0，296s）：worker join 事件出现、双节点 Ready、worker podCIDR、全 pod Running、二遍 0 重跑。

## M1 步骤 5 — HA 控制面 ✅（2026-10-26）

新增 `vip` 与 `api-ready` 阶段：kube-vip 静态 pod 必须在 init 前落盘，join 前必须确认 VIP 后端真的在服务。

kubeadm 1.33 + VIP 的三个真实坑（都是 tier-3 暴露的）：

1. **admin RBAC 自举死锁**：1.33 把 admin.conf 移出 `system:masters`（组 `kubeadm:cluster-admins`），补建 CRB 的调用走 controlPlaneEndpoint=VIP，而 VIP 需要 kube-vip 的 lease、lease 又要这个 CRB。解法：用 kubeadm 自带的 `super-admin.conf`（system:masters）经 `127.0.0.1` 补建（apiserver certSANs 增加 127.0.0.1）。
2. **post-init phases 静默缺失**：`upload-config`（kubeadm-config CM + token RBAC）、`bootstrap-token`（cluster-info CM + 匿名 RBAC）、`addon`（kube-proxy + CoreDNS）都经 VIP 访问 API，init 时不可达 → 全部缺失 → join 在 discovery/preflight 阶段饿死、CNI 无法调度。解法：init 后按序等待 API ready 并重放这三个幂等 phase。
3. **kube-vip address 语义**：v0.8 把 `address + vip_subnet` 拼成 CIDR，`vip_subnet` 必须带斜杠（`/32`）。

tier-3 干净循环 PASS（新 3 VM → 安装 → 断言 → 幂等，exit 0，~13 分钟）：3 节点 Ready、2 个 CP join、VIP `/readyz`、全 pod Running、二遍全 skip。

## M1 步骤 6 — 增删节点与销毁 ✅（2026-10-26）

实机验证（tier-2 集群）：

- `kurin add nodes -f`：新 worker 2 秒 join，既有节点/阶段全部 skip（同一条命令兼作 reconcile）。
- `kurin delete node <name> -f`：drain → kubectl delete node → kubeadm reset → 服务停用 → 制品清除；验证节点从集群消失且 `/etc/kubernetes`、kubelet 二进制被清空。
- `kurin delete cluster -f`：workers 先、控制面后逐个擦除 —— 两节点均确认 `/etc/kubernetes` 不存在、containerd/kubelet 停止、`/usr/local/bin` kubernetes 制品清空（含 containerd-stress），本地 join 材料与 run-state 一并删除。
- 修复：drain 在 NotReady 节点上会被优雅驱逐卡死，改用 `--disable-eviction --grace-period=0 --timeout=90s`。

## 下一步（M1 实施顺序）

1. ✅ Planner + dry-run + Task 引擎骨架（tier-0 PASS）
2. ✅ 制品模型 + 分发（tier-0 PASS）
3. ✅ 单节点 all-in-one 真实闭环（multipass 已装：snap + `sudo` 组 socket 认证；tier-1 PASS）

   - 修复并验证：cni kubectl 显式 `--kubeconfig`；CRD 由 operator 自建需轮询（Calico ≥3.26 operator manifest 不含 CRD）；exec 超时 120s→900s（kubeadm init ~55s + 镜像拉取）；kernel-deps 任务（apt/dnf 兜底 conntrack/socat/ebtables/ipset/ipvsadm）；kubeadm 节点名取自配置（`nodeRegistration.name`，缺省会用 hostname）。
   - tier-1 断言：cluster-ready 事件零 task-failed、node01 Ready（轮询）、podCIDR 为集群 /18 的 /24 切片、全部 pod Running（含 calico-system）、第二遍全 skip 幂等。
   - **干净环境全流程证明**（新 VM→create cluster→断言→销毁，exit 0，225s）：`make tier1-down && make tier1 && make tier1-test`。
4. ✅ 多节点（tier-2 PASS）；5. ✅ HA/kube-vip（tier-3 PASS）；6. ✅ add/delete nodes；7. ✅ 离线验收（tier-1-offline PASS）

## M1 步骤 7 — 离线部署 ✅（2026-10-26）

- **制品清单扩容到 23 项**：二进制 7 + calico-operator manifest + 镜像 15（kube-apiserver/controller-manager/scheduler/proxy、etcd、coredns、pause、calico 全家桶、tigera operator）。镜像以 OCI index digest 锁定，下载走 registry v2 API（quay.io 匿名可拉，Docker Hub 需 token 故统一 quay；Installation CR 设 `registry: quay.io` 使引用一致）。
- **`kurin artifact --out <dir>`**：经 sha256 校验抓取全部制品为离线 bundle（目录布局与在线缓存一致）。
- **`create cluster --offline <dir>` / `[artifacts] offline-dir`**：安装全程零下载——制品读 bundle、镜像以 docker-archive（内存 ustar 构造）经 `ctr -n k8s.io images import` 播入。
- 修复：bundle 目录名解析改为清单驱动（`etcd 3.5.21-0` 带连字符的版本曾致 naive split 错位）。
- **tier-1-offline PASS**：VM 出站到全部制品源 IP 被 iptables REJECT 的前提下，单命令装出 Ready 集群、全部 pod 健康、二遍全 skip（镜像导入幂等按参考集合核对）。

## M1 交付总结（全部 7 步完成）

| 验收 | 结果 |
|---|---|
| tier-0（systemd 容器，幂等双遍） | PASS |
| tier-1（单节点 all-in-one，干净循环 225s） | PASS |
| tier-2（1CP+1worker，join+幂等，296s） | PASS |
| tier-3（3CP+kube-vip HA，干净循环） | PASS |
| tier-1-offline（断网安装+幂等） | PASS |
| 单元测试 | 71 全绿 |

已知留白：join 任务需要控制面 bootstrap token 的跨节点协作（kubeadm token create → worker join flag 注入，M1 步骤 3 接线）；`delete cluster` 命令未实现。
