# 参考

- [CLI 参考](cli.md) —— 全部命令、参数、示例、退出码
- [Kubernetes 版本支持](kubernetes-versions.md) —— 默认 pin、包版本、升级策略、`kurin versions`
- [配置参考](configuration.md) —— v1alpha1/v1alpha2 字段与校验
- [命令 × 形态矩阵](command-matrix.md) —— 组合支持状态
- [Registry 契约](registry-contracts.md) —— registry 模式与任务
- JSON Schema：[cluster.v1alpha2.json](../schema/cluster.v1alpha2.json)、
  [addon.v1alpha1.json](../schema/addon.v1alpha1.json)、
  [package.v1.json](../schema/package.v1.json)

配置字段的权威来源是 `kurin-config` 的 Rust 类型与
`kurin config schema --version v1alpha2`；本目录的 schema 文件由
`just schema` 生成，漂移会让测试失败。