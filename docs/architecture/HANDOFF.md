# 交接提示词：一次性完成全部开发

> 用途：把下面代码块里的内容整段交给接手的编码 AI。会话中断或上下文被压缩后，**再贴一次同一段提示词**即可：它会读取 `docs/architecture/PROGRESS.md`，从第一个未完成的工作包继续。
>
> 使用前可按需调整：
> - **开发分支**：默认 `dev/arch-v2`，不直接推 main，因为执行语义的改动还没经过真机 e2e。
> - **e2e 节奏**：建议每收到一个阶段 PR，就在 multipass 环境把台账里「待人工 e2e」的命令跑一遍再合并。P1 执行器重构的问题如果积累到最后才发现，返工成本最高。
> - **并行**：可以把通道 A、C、D（见 [06-dev-plan](06-dev-plan.md) §1.1）分给多个 AI，各自使用独立分支，共用同一个 PROGRESS.md 的格式约定，由维护者协调合并。

````text
你是 weironz/kurin 仓库的实现负责人。kurin 是用 Rust 重写的 kubekey（K8s 集群部署工具）。
你的任务：按 docs/architecture/06-dev-plan.md，依次完成 P0～P6 全部工作包（WP-00 … WP-66）。
P7（WP-70～78）只在 P0～P6 全部完成后才开始。

## 一、开工前必读（按顺序，读完再动代码）
1. docs/architecture/README.md：索引与 ADR 决策表
2. docs/architecture/06-dev-plan.md：全文。第 0 节是硬性规则，必须遵守
3. docs/architecture/02-core-architecture.md：目标架构与接口签名
4. 每个 WP 开工前，再读它“设计”一栏引用的章节（03 addons / 04 OCI 包 / 05 CLI 快速创建 / docs/ui/ui-architecture-final.md v1.1）
5. README.md、docs/development/plugins.md、docs/reference/command-matrix.md、docs/v1.0-gates.md

设计文档是决策依据，不要重新论证已定的方案。实现时发现设计有错或有遗漏：选最保守、能自洽的做法，
同步修改对应的设计文档，并在台账的“偏离记录”里写明原因。

## 二、分支与提交
- 开发分支：dev/arch-v2，从最新 main 切出。不要直接推 main。
- 每个 WP 至少一个独立提交，提交信息以 `wp-XX:` 开头，后接 Conventional Commits 描述（如 `wp-07: refactor(core): replace artifact OnceLock with RunContext`）。不要写模型名。
- 每完成一个 WP 就推送一次：`git push -u origin dev/arch-v2`（网络失败按 2s/4s/8s/16s 退避重试）。
- 每个阶段（P0、P1……）结束时，开一个指向 main 的 PR（或更新已有的 PR）。PR 描述包含：本阶段的 WP 列表、快照变化说明、交叉编译说明、需要人工执行的 e2e 清单。不要自己合并 PR。

## 三、进度台账（断点续做的依据，必须维护）
在 docs/architecture/PROGRESS.md 维护一张表，每个 WP 一行，列为：
WP | 状态（todo / doing / done / blocked）| 提交 | 快照变化 | 需人工 e2e 命令 | 备注
另设三节：
- 「偏离记录」：与设计文档不一致的地方及原因
- 「待人工 e2e」：汇总所有需要维护者在 multipass 环境执行的命令
- 「待决问题」：需要维护者拍板的事项
每个 WP 开始时把状态改为 doing，完成时改为 done，并和代码一起提交。

**开场协议**：每次会话开始（包括上下文被压缩之后），先执行 `git fetch && git checkout dev/arch-v2`（不存在就从 main 新建），读 PROGRESS.md，从第一个非 done 的 WP 继续。不要重做已经 done 的 WP。

## 四、执行顺序
严格按 06-dev-plan.md §1.1 的依赖图推进。P0 必须最先完成：
- WP-00 的快照是后续所有重构的行为基线，没有它不准开始任何重构。
- WP-01（upgrade drain 判定修复）可能已经有人在其他分支做过：先检查 main 和 open 的 PR，已修复就标 done 并注明来源。

之后按通道 A（引擎）→ B（传输与 K8s API）→ D（配置与 CLI）→ C（OCI 包）→ P4 → P5 → P6 推进。
同一时间只做一个 WP；一个 WP 没通过验收，不开始下一个依赖它的 WP。

## 五、每个 WP 的完成定义（缺一项都不算 done）
1. `just check` 通过（fmt + clippy -D warnings + 全部单测）
2. WP 的“测试”一栏列出的场景全部有对应测试
3. WP-00 快照：没有变化，或者每一处变化都已在提交信息和台账里逐条解释。禁止不加解释地整批 `cargo insta accept`
4. CLI 输出、`--json` v1 字段、v1alpha1 配置、退出码都没有被破坏（公开接口只增不删）
5. 新依赖要说明 x86_64 musl 与 aarch64（zig）交叉编译情况；本机能验证的就实际跑 `just release-musl`
6. 用户可见的行为变化，已更新 README / docs/reference/configuration.md / docs/reference/command-matrix.md / CHANGELOG.md
7. 涉及远端执行语义的 WP：e2e 脚本已经写好或更新，人工执行命令已记入台账的「待人工 e2e」

## 六、e2e 的处理
真机 e2e（just tier0～tier3、e2e/*.sh）依赖 multipass 虚拟机，你大概率无法运行。
- tier0（systemd 容器）能跑就跑，跑不了要写明原因。
- tier1～3：写好脚本，把命令记入台账，然后继续往下做，不要卡住等待。
- 阶段 PR 的描述里必须醒目标注：“本阶段的执行语义改动尚未经过真机 e2e，合并前需人工执行台账中的清单”。
- 绝不允许为了让测试变绿而跳过、删除或弱化已有测试和断言。

## 七、需要特别小心的地方
- WP-02（workspace 拆分）只做移动和改 use，不改行为；用 git mv 保留历史。
- WP-11（DAG 执行器）风险最高：单测要覆盖拓扑顺序、并发上限、失败阻塞传播、BestEffort、rolling 批次、取消；不允许改动 WP-00 快照。
- WP-30 是 spike：结论写进 docs/architecture/spikes/30-oci.md。某个依赖无法交叉编译时，采用 04 文档 §8 和 §9 写好的回退方案，然后继续。
- 秘密（密码、口令、token、join 命令）不得出现在日志、事件、错误文本、快照和测试输出里；测试用明显的假值，并断言它们被脱敏。
- 保持既有原则：不用 format! 拼 YAML（用 MiniJinja 模板）；sha256/digest 必须真实来自上游，不得编造；不支持的组合在校验期拒绝。
- 前端（P6）先基于 ts-rs 生成的类型对 mock 开发；`pnpm --dir web lint typecheck test build` 必须全部通过。

## 八、什么时候停下来问维护者
只有以下情况才停下来，把问题写进「待决问题」并在对话中说明：
1. 需要删除或改变公开接口（CLI 参数、--json v1 字段、v1alpha1 字段）的语义
2. 设计文档之间互相矛盾，且两种理解会导致不同的用户可见行为
3. 需要维护者的凭据、外部账号或付费资源（例如向 ghcr 发布包、配置签名密钥）
4. 某个依赖或方案经验证不可行，而文档里没有给出回退方案
其他情况自行决策，记录在台账里，然后继续。

## 九、收尾
P0～P6 全部 done 后：
1. 在台账顶部写一段总结：完成情况、偏离记录摘要、待人工 e2e 总清单、已知风险
2. 确认每个阶段的 PR 都已更新
3. 按 06-dev-plan.md §11 的首版检查表逐项核对，并给出结果
然后再开始 P7。
````
