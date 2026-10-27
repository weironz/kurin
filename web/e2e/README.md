# WP-66：真实环境端到端验收

这组 Playwright 用例对接真实 `kurin serve`（tier1/tier2），覆盖 UI 文档
M2 的验收场景。它与 `web/tests/` 的 mock 套件分开：

```sh
cd web
pnpm test:e2e:real        # 未设置 KURIN_E2E_URL 时全部 skip
```

## 前置

- 一台可 SSH 的节点（tier1 单节点即可；tier2 用于升级场景）。
- 已构建的 `kurin` 二进制，`kurin serve` 正在运行并打印了令牌 URL。
- 浏览器能访问该地址；测试机与 serve 同机时用 `http://127.0.0.1:7080`。

## 环境变量

| 变量 | 必填 | 说明 |
|---|---|---|
| `KURIN_E2E_URL` | 是 | `kurin serve` 地址；未设置则全部 skip |
| `KURIN_E2E_TOKEN` | 是 | serve 启动时打印的令牌（`/#token=…` 里的值） |
| `KURIN_E2E_CLUSTER` | 否 | 本次创建的集群名（默认 `e2e-<时间戳>`） |
| `KURIN_E2E_TEMPLATE` | 否 | 向导模板卡片文案（默认 `单节点体验`） |
| `KURIN_E2E_BULK` | 否 | 向导第 3 步批量节点文本（默认 `192.0.2.11 control-plane`） |
| `KURIN_E2E_AUTH` | 否 | `key`（默认）或 `password` |
| `KURIN_E2E_KEY_PATH` | key 时 | 管理机上的私钥路径（默认 `/root/.ssh/id_ed25519`） |
| `KURIN_E2E_PASSWORD_ENV` | password 时 | 密码环境变量名 |
| `KURIN_E2E_ADD_BULK` | 否 | 加 worker 的批量文本；设置后跑加节点场景 |
| `KURIN_E2E_DELETE_NODE` | 否 | 要删除的节点名；设置后跑删节点场景 |
| `KURIN_E2E_DATA_DIR` | 秘密扫描时 | serve 的 `--data-dir` |
| `KURIN_E2E_SECRET` | 秘密扫描时 | 明文密码；断言它不出现在 DB/事件/API |
| `KURIN_E2E_CLI` | 抢锁场景 | `kurin` 二进制路径 |
| `KURIN_E2E_CONFIG` | 抢锁场景 | 数据目录下 `clusters/<name>/cluster.toml` 镜像 |
| `KURIN_E2E_XDG_STATE_HOME` | 抢锁场景 | serve 与 CLI 共用的 `XDG_STATE_HOME`（锁目录） |
| `KURIN_E2E_RESTART_CMD` | 重启场景 | 重启 serve 的 shell 命令（如 `systemctl restart kurin`） |

## 场景

- `lifecycle.spec.ts`：创建 → 状态 → 加 worker → 删 worker → 销毁。
- `resilience.spec.ts`：
  - 创建中关闭浏览器，重开后操作仍在服务端继续；
  - 重启 serve 后活动操作变为 `interrupted`，页面「重新执行」创建新操作；
  - Web 操作进行中，CLI 抢锁被拒绝（`is busy`）；
  - 创建完成后，密码明文不出现在数据库、事件、日志与 API 响应中。

## 手工记录

维护者执行后把结果写入 `docs/progress.md`（日期、tier、通过/失败项）。
