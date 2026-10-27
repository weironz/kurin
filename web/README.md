# kurin Web 前端

React SPA，构建产物 gzip 后由 `kurin serve`（`--features ui`）经 rust-embed 嵌入单二进制。设计依据见 `docs/ui/ui-architecture-final.md` 与 `docs/architecture/06-dev-plan.md` WP-60。

## 技术栈

Vite + React 18 + TypeScript（strict）+ Ant Design 5（`zh_CN`）+ TanStack Query v5 + React Router v6；测试 Vitest + Testing Library + MSW。

## 常用命令

```bash
pnpm --dir web install        # 安装依赖
pnpm --dir web lint           # ESLint（flat config）
pnpm --dir web typecheck      # tsc --noEmit
pnpm --dir web test           # Vitest（MSW mock 后端）
pnpm --dir web build          # tsc --noEmit + vite build（产出 .gz 资产）

pnpm --dir web dev            # 开发服务器，/api 代理到 127.0.0.1:7080
pnpm --dir web dev:mocks      # 同上，但用 MSW mock（VITE_USE_MOCKS=1），无需后端
```

## 目录结构

```
src/
  api/
    types.ts     # 临时 DTO（WP-55 将由 ts-rs 生成替换）
    client.ts    # /api/v1 fetch 封装：错误码映射、hash 令牌登录
    sse.ts       # /operations/{id}/events SSE：Last-Event-ID、seq 去重、退避重连
  components/    # AppShell（侧边栏 + 登出头）
  pages/         # 各路由页面（多数为占位，WP-61+ 实现）
  i18n/zh-CN.ts  # 文案字典（键英文、值中文），t() 查表
  mocks/         # MSW handlers（测试 & dev:mocks 共用）
  test/setup.ts  # Vitest 全局 setup
```

## 登录流程

启动 `kurin serve` 后终端打印 `http://127.0.0.1:7080/#token=…`；前端从 hash 取令牌 `POST /api/v1/session` 换取 HttpOnly cookie，随即清除 hash（UI §6.10）。

## 注意

- 不使用任何 CDN；所有资源打包进 `dist`。
- `src/api/types.ts` 为**手写临时草稿**，WP-55 会在 `src/api/generated/` 生成 ts-rs 绑定并替换它。
