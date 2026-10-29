# 在线安装

目标：控制机能访问公网（GitHub Releases、镜像仓库），节点能访问控制机
（SSH）与公网（或走 [国内镜像](mirrors.md)）。

## 1. 准备节点

- 支持的系统见 [安装 · 支持的节点](../getting-started/install.md#支持的节点)。
- 控制机能 `ssh root@<node>`（或可 sudo 的普通用户）。
- 节点时间同步；`/` 至少 5 GiB 可用（建议 20 GiB）；控制面建议 ≥2 GiB 内存。
- 关闭 swap（`kurin precheck` 会检查并给出提示）。

## 2. 写配置

```toml
version = "v1alpha1"
name = "demo"

[kubernetes]
version = "1.33.1"   # 在线 create 只接受 embedded 版本；其他版本见下方说明

[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"

[network.cni]
kind = "calico"

[ssh]
user = "root"

[ssh.auth]
method = "private_key"
key-path = "/root/.ssh/id_ed25519"

[nodes.node01]
address = "192.168.10.11"
role = "control-plane"
```

密码登录把 `[ssh.auth]` 换成：

```toml
[ssh.auth]
method = "password"
password = { env = "KURIN_SSH_PASSWORD" }
```

秘密引用的完整写法（env/file/字面值、节点级覆盖）见
[配置参考 §秘密引用](../reference/configuration.md#秘密引用secretref)。

在线 create 的 embedded manifest 只 pin 1.33.1；要装其他版本，先用
`kurin package create base --kubernetes <ver>` 制作 base 包并通过
`--package` 传入。`kurin versions` 会打印当前构建的版本矩阵。

## 3. 预检

```bash
kurin precheck -f demo.toml
```

只读检查：swap、内核模块、端口占用、磁盘、时间同步、OS 支持等。发现问题
先修，不要直接安装。

## 4. 预览计划

```bash
# 完全离线：只渲染任务图
kurin create cluster -f demo.toml --dry-run

# 真 SSH 探测，标注 would-change / unchanged
kurin create cluster -f demo.toml --dry-run --check
```

## 5. 安装

```bash
kurin create cluster -f demo.toml
```

安装完成后 kurin 会把 admin.conf 合并进本地 kubeconfig（context
`cluster-demo`）并切换过去：

```bash
kubectl get nodes
kubectl get pods -A
```

也可以直接验证：

```bash
kurin status -f demo.toml
```

`status` 的退出码可用于健康门禁（非 0 表示有节点/组件/证书异常）。

## 6. 中断与续跑

任意阶段中断后：

```bash
kurin create cluster -f demo.toml --resume
```

`--resume` 从 `$XDG_STATE_HOME/kurin/runs/<cluster>.bb.json` 恢复已完成任务；
计划指纹变化时会拒绝续跑，此时去掉 `--resume` 重新执行（幂等任务会跳过已
收敛的步骤）。

## 7. 销毁

```bash
kurin delete cluster -f demo.toml --dry-run   # 先看计划
kurin delete cluster -f demo.toml
```

删除节点上的 kubeadm/containerd/制品与配置，不删除节点操作系统。

## 下一步

- 多节点/HA：[高可用](ha.md)
- 断网环境：[离线安装](offline.md)
- 浏览器管理：[Web UI](../getting-started/ui.md)