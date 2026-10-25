# 进度日志

## W0 — 契约冻结（2026-10-25）✅

交付：

- **kurin-config** crate：`v1alpha1` schema（TOML/YAML 双格式、deny_unknown_fields、tagged enum 互斥、强类型 CIDR/IpAddr/Version）、四段流水线（parse → version gate → defaults → validate）、miette 行列级错误渲染（已冒烟验证：`unknownfield` 高亮到 6:1）。
- **kurin** crate（lib + thin CLI）：`Task` trait（check/apply + `Verified/Unknown` tri-state）、`Session` trait、petgraph 线性阶段图（preflight → prepare → bootstrap → join → cni → post）、`render_tree` dry-run、`create cluster --dry-run` / `create config [--print-versions]` 命令、rust-embed + minijinja 模板（kubeadm-init、containerd config）、编译期版本表。
- 13 个单测全绿；`kurin create cluster -f examples/cluster.example.toml --dry-run` 输出完整 6 阶段计划树。

已知留白（W1 接管）：

- russh SessionPool 未实现（CLI 执行路径显式报错引导）
- join 任务需要从控制面取 bootstrap token（跨节点 Session 协作，W1 executor 接线）
- artifact 模型、tier-0 e2e、模板全集、版本 sha256 校验值

## W1 — 七轨并行（进行中）

| 轨 | 范围 | 状态 |
|---|---|---|
| A kurin-config | insta 快照、YAML 例、校验深化（CIDR 冲突精化） | 待开工 |
| B templates | systemd unit（containerd/kubelet）、kube-vip manifest、kubeadm join、Calico Installation CR 模板化 | 待开工 |
| C artifact | 制品清单、下载缓存 sha256 校验、`--offline <dir>` 读取 | 待开工 |
| D ssh | russh 连接池 + Semaphore + backon、SessionPool 实现 | 待开工 |
| E engine | DAG 拓扑并行执行、事件 JSON sink、dry-run --check 接线、resume 状态文件 | 待开工 |
| F e2e | tier-0 Makefile + systemd 容器断言 | 待开工 |
| G 版本表 | 真实 URL + sha256 校验值收集 | 待开工 |
