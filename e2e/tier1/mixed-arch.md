# tier-1 mixed-arch：x86 控制机 + arm64 节点（手工步骤）

> WP-04 的人工 e2e。目标：证明 discover 阶段探测出的节点架构驱动
> 制品选择——控制机是 x86_64、节点是 aarch64 时，节点拿到 arm64 的
> kubeadm/kubelet/kubectl/containerd/CNI 插件与镜像，集群正常建立。
>
> 没有多arch 真机时，用 qemu 用户态或系统级模拟跑 arm64 虚拟机
> （multipass 本身在 Apple Silicon 上原生即 aarch64；下面的路径 B 覆盖
> “控制机也是 arm 原生”的情形）。

## 前置条件

- 一台 x86_64 控制机（能 SSH 到节点、能出网下载制品）。
- 一个 arm64 节点，两条可选路径：
  - **路径 A（qemu system 模拟）**：`qemu-system-aarch64` + Ubuntu
    24.04 arm64 cloud image（`https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-arm64.img`），
    UEFI 固件 `qemu-efi-aarch64`，`-M virt -cpu cortex-a57 -m 4096 -smp 2`，
    用户态网络把 22 端口映射到宿主机。慢，但步骤与真机完全一致。
  - **路径 B（Apple Silicon / arm 服务器上的 multipass）**：直接
    `multipass launch --name kurin-arm --cpus 2 --memory 4G --disk 20G 24.04`，
    得到原生 aarch64 节点；控制机如果也是该 arm 主机，则本用例变成
    “arm 控制机 + arm 节点”（同样验证 facts 路径，只是不混合）。
- 控制机已构建 kurin（`just build`）。

## 步骤

```bash
# 0. 确认两台机器的架构（预期：控制机 x86_64，节点 aarch64）
uname -m          # 控制机：x86_64
ssh <node> uname -m   # 节点：aarch64

# 1. 配置（示例：单控制面 x86 + arm64 worker；把控制面节点跑在控制机
#    能 SSH 到的任一 x86 机器上，或直接用本机自托管节点）
cat > mixed-arch.toml <<'EOF'
version = "v1alpha1"
name = "mixedarch"
[kubernetes]
version = "1.33.1"
[network]
pod-cidr = "10.233.64.0/18"
service-cidr = "10.233.0.0/18"
[network.cni]
kind = "calico"
[ssh]
user = "root"
[ssh.auth]
method = "private_key"
key-path = "<你的私钥>"
[nodes.cp1]
address = "192.168.10.11"      # x86_64 控制面
role = "control-plane"
[nodes.w1]
address = "192.168.10.21"      # aarch64 worker
role = "worker"
EOF

# 2. 干跑：确认 discover 阶段出现在计划树首位，且每个节点的制品架构
#    与真实架构一致（w1 显示 arm64）。未配 nodes.<n>.arch 时会打
#    “arch not probed” 警告——先带 --check 真探测：
./target/release/kurin create cluster -f mixed-arch.toml --dry-run --check | head -20
#   ├── stage: discover (2 tasks)
#   │   ├── discover/facts@cp1 :: arch=amd64 os=ubuntu 24.04 cpu=N mem=…MiB
#   │   └── discover/facts@w1  :: arch=arm64 os=ubuntu 24.04 cpu=N mem=…MiB
#   ├── stage: artifacts (2 tasks)
#   │   ├── artifacts/install@w1 :: fetch+install pinned binaries (arm64)
#   ...

# 3. 纯 --dry-run（零网络）验证 nodes.<n>.arch 回退：
#    在 [nodes.w1] 加 arch = "arm64"，然后
./target/release/kurin create cluster -f mixed-arch.toml --dry-run | grep artifacts/install
# 预期：artifacts/install@w1 :: fetch+install pinned binaries (arm64)

# 4. 真实安装
./target/release/kurin create cluster -f mixed-arch.toml

# 5. 断言（复用 tier1 的断言脚本：bin cfg 两参数）
e2e/tier1/assert.sh target/release/kurin mixed-arch.toml

# 6. 节点侧确认 arm64 二进制
ssh <node> /usr/local/bin/kubelet --version
ssh <node> uname -m    # aarch64

# 7. 清理
./target/release/kurin delete cluster -f mixed-arch.toml
```

## 离线（可选加验）

```bash
# 一个包覆盖两种架构
./target/release/kurin artifact --out bundle --with-kubernetes v1.33.1 --arch amd64,arm64
ls bundle | sed 's/-[^-]*$//' | sort -u   # 应看到同名的 amd64 与 arm64 目录
# 然后走 e2e/tier1/offline.sh 同款流程，配置指向该 bundle
```

## 通过标准

- 计划树首阶段是 `discover`，任务 `describe()` 显示每节点 arch/OS；
- `w1`（arm64）的 artifacts/images 任务显示 `(arm64)`；
- `create cluster` 全绿，`kubectl get nodes -o wide` 两个节点 Ready；
- upgrade 路径不回归（`just tier2` 常规回归单架构即可）。

记录结果到 `docs/progress.md`（R6）。
