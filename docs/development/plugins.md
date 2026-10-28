# 插件集成指南

kurin 里的"插件"不是动态扩展（没有插件系统/playbook——这是刻意的架构选择），
而是**可替换的实现有界集合**：CNI、HA 模式、发行版、制品来源。每个集成点
接受"新增一个变体"的标准方式，记在这里。

## 原则

- 插件是**编译期变体**，tagged enum 选定，schema 上行而不为自由文本——未知的
  `kind = "foo"` 直接报错，不存在"装了但装错"的状态。
- 新变体必须有**验收级的 e2e**（tier-1 最小）才合入；没有 e2e 的不算完成。
- 新增变体进 manifest 时 sha256 必须上游实抓（registry digest 或官方 .sha256
  端点），不得猜。

## 集成点 1：CNI 插件

`network.cni.kind = "calico"|"flannel"|"cilium"|"none"`。新增一个 CNI 的标准三步：

1. **加 enum 变体**：`kurin-config/src/config.rs` 的 `CniConfig`。
2. **写插件模块**：`kurin/src/tasks/cni/<name>.rs`，实现 `Task`：
   - 引用只到 `Cluster`（pod_cidr 等）与制品（manifest 里的 entry），不发明数据。
   - 有 pinned 上游 manifest 优先走 [artifacts 制品](#集成点-3制品；bump 到 manifest.json，
     sha256 上游实抓），并在 `zone::rewrite_image_ref` 后放行 zone=cn 的镜像改写
     （calico 用 CR `registry` 字段、flannel 用 manifest 字符串改写、cilium 用
     `--set image.repository`——随手查阅三者怎么做的，别乱编）。
   - check 探测收敛态（不是"装上了"，而是 ready 数与期望数相符）。
   - 在 `tasks/cni.rs` 的 `plugin_task` 分派注册一行。
3. **e2e 验证**：`e2e/tier1/<name>.sh`（同 flannel.sh/cilium.sh 结构），跑通
   cluster-ready + DS ready==desired + node Ready，二遍幂等。

现有实现的差异点：calico 用 tigera-operator + CR 表达配置、k3s-flannel 由 k3s
内嵌（同 pod_cidr 传参）、cilium 是 CLI 驱动。三者全集覆盖你几乎都会遇到：
manifest-apply、CR-模板、CLI-驱动。

## 集成点 2：HA 模式

`ha.mode = "kube-vip"|"external-endpoint"|"kurin-lb"`。

新增一个 HA 模式（比如 HostPrefix LB、Anycast）：在 config `HaMode` 加变体 +
plan.rs 的分派 + `tasks/<name>.rs`（`Vip`/`ApiReady` 阶段任务）+ 校验
（cp≥2、vip 规则）。kube-vip 与 kurin-lb 是两个现成模板：
- 渲染模板进 `kurin/templates/<dir>/`（MiniJinja），禁 `format!` 拼 YAML。
- LB 节点类型如果是"装了东西但不是集群成员"：新增 `NodeRole` 变体并在 plan.rs
  排除 containerd/kubelet/images/join 任务（详见 `k8s_nodes` 的过滤）。
- 验收 e2e：一早证明节点 READY + `/readyz` 经 VIP 通 + 全 pod 健康 + 幂等。

## 集成点 3：发行版

`kubernetes.distribution = "k8s"|"k3s"`。新增一个发行版（如 OpenShift、MicroK8s、
RKE2）时：

1. `config.rs` 的 `Distribution` 加变体。
2. `kurin/src/tasks/<distro>.rs`：实现 `stage_tasks(cluster) -> Vec<(Stage, Arc<dyn Task>)>`
   （参考 `kurin/src/tasks/k3s.rs`：每类任务都是 prepare/vip/bootstrap/api-ready/join/post 的
   组合）。
3. plan.rs 加 distribution 分支（复用 preflight/artifacts/sysctl/kernel-deps；
   跳过该发行版不需要的 kubeadm/containerd-unit/kubelet-unit）。
4. 制品（发行版单二进制）进 manifest，`install_for` 的一处注册（Direct 或 Tar）。

## 集成点 4：制品与镜像源

所有可下载件走 `kurin/src/artifacts/manifest.json`：

```json
{
  "name": "my-binary",
  "kind": "binary",              // binary | image | manifest
  "version": "1.2.3",
  "file": "tarball-name-{arch}.tar.gz",
  "url_template": "https://…/v{version}/…-{arch}…",
  "sha256": { "linux-amd64": "…", "linux-arm64": "…" }
}
```

- **sha256 必须真实上游**（registry head `docker-content-digest` 或官方 `.sha256`
  端点），否则 `kurin artifact` 下载校验直接拒。
- `install_for`（`tasks/install.rs`）决定它落在节点哪里（Direct 目标路径 vs Tar 解压目录）。
- zone=cn 对镜像等的改写由 `zone.rs` 的表驱动；要进新 mirror，一行表项
  + `scripts/check-mirrors.sh` 加一条 digest 比对即可（CI 每周自动验证）。

## 集成点 5：任务与编排

这个层面不是"插件"而是引擎内部接口，但常被误认为是扩展点：

- 每个有副作用的步骤都是 `Task`（`engine.rs`）：`check_state → apply → verify`。
- 新增经典任务放进 `tasks/<stage>.rs` 并注册到 `plan.rs` 相应 stage；
  通用节点循环已在 plan.rs，不用自己开 SSH 连接池——拿到 `&dyn Session` 就好。
- 失败语义：
  - `Outcome::Unknown` 是"网络断了，不知道远端成没成"，引擎自动重新探测；
  - `Drifted("reason")` 会出现在 `--dry-run --check` 的计划面前，是人类可读信息，写清楚。
- 二进制替换**必须**写临时名再 `mv -f`（`tasks/install.rs` / `upgrade.rs` 的
  样本）——直接覆写运行中的二进制会触发 ETXTBSY 并使 ssh 通道静默挂起
  （uv高优先级样本，详见 ssh.rs `write_file` 的 doc comment）。

## 验收门槛（一个合入插件的最小定义）

- config schema 有合法格式、未知字段报错、与现有字段无重叠。
- 至少一个真实 e2e：cluster-ready + 每个节点 Ready + 全部 pod Running
  + 二遍全幂等。
- zone=cn 路径（若适用）有镜像引用改写，digest 一致性已有 check-mirrors.sh
  覆盖（manifest 新增镜像时自动纳入）。

如果这个门槛过不了，那不是"还差一点点"——是尚未集成，别合。
