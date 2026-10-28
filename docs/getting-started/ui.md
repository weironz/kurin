# Web UI

`kurin serve` 在同一个二进制里启动 HTTP 服务并托管内嵌的前端（React SPA）。
release 二进制默认包含 UI；源码构建需要 `--features ui` 和已构建的
`web/dist`（见 [安装](install.md)）。

## 启动

```bash
kurin serve
```

默认：

- 监听 `127.0.0.1:7080`（只绑定回环地址）
- 数据目录 `$XDG_DATA_HOME/kurin`，未设置时为 `~/.local/share/kurin`
- 首次启动生成 256-bit 访问令牌，写入 `<data-dir>/token`（0600），并在
  终端**只打印一次**登录链接：

```text
登录链接（仅此一次）：http://127.0.0.1:7080/#token=<token>
```

浏览器打开该链接后，前端用 URL fragment 里的 token 换取
`kurin_session` cookie，随后跳转到集群列表。token 不会出现在后续请求的
URL 或 Referer 中。

常用参数：

| 参数 | 说明 |
|---|---|
| `--listen <ADDR>` | 监听地址，默认 `127.0.0.1:7080` |
| `--data-dir <DIR>` | 数据目录（token、`kurin.db`） |
| `--public-host <HOST>` | 额外允许的 `Host` 头，可重复；也用于生成登录链接 |
| `--tls-cert <FILE>` / `--tls-key <FILE>` | PEM 证书链与私钥，启用 HTTPS |
| `--allow-insecure-http` | 允许在非回环地址上使用明文 HTTP（不推荐） |
| `--rotate-token` | 重新生成 token，使所有现有会话失效 |
| `--max-concurrent-ops <N>` | 全局并发操作上限，默认 2 |
| `--max-upload-parallel <N>` | 单次操作的并发载荷上传上限，默认 4 |

## 访问方式

### 本机

直接打开启动时打印的 `http://127.0.0.1:7080/#token=…`。

### 远程（推荐 SSH 隧道）

保持默认的回环绑定，通过 SSH 转发访问，不暴露任何端口：

```bash
ssh -L 7080:127.0.0.1:7080 user@control-host
# 然后在本地浏览器打开 http://127.0.0.1:7080/#token=…
```

如果 token 已丢失，在控制机上重新生成并复制新链接：

```bash
kurin serve --rotate-token
```

### 直接暴露端口

非回环监听必须显式选择 TLS 或明文 HTTP：

```bash
# 明文（仅限受信任的内网/防火墙后；token 和 cookie 会以明文传输）
kurin serve --listen 0.0.0.0:7080 --allow-insecure-http \
  --public-host kurin.example.com:7080

# TLS
kurin serve --listen 0.0.0.0:7080 \
  --tls-cert /etc/kurin/tls/fullchain.pem \
  --tls-key  /etc/kurin/tls/privkey.pem \
  --public-host kurin.example.com
```

`--public-host` 同时参与 `Host` 头白名单和登录链接的 authority。反向代理
（Nginx/Traefik）转发时，把外部域名写进 `--public-host`，并保持
`X-Forwarded-*` 头；TLS 可以在代理层终止，kurin 侧继续监听回环。

### Docker

```bash
docker run -d --name kurin \
  -p 127.0.0.1:7080:7080 \
  -v kurin-data:/data \
  willdockerhub/kurin:latest \
  serve --listen 0.0.0.0:7080 --allow-insecure-http
docker logs kurin   # 复制一次性登录链接
```

完整说明（TLS、卷、反向代理、CLI 模式）见
[Docker 运行 CLI 与 UI](../docker/README.md)。

## 界面功能

| 页面 | 功能 |
|---|---|
| 集群列表 `/` | 集群生命周期、节点数、健康摘要 |
| 新建集群 `/clusters/new` | 9 步向导：名称/发行版/节点/网络/CNI/HA/SSH/addon/确认 |
| 集群详情 `/clusters/:name` | 节点、组件、资源、证书、危险区（加/删节点、销毁） |
| 操作 `/operations`、`/operations/:id` | 阶段×节点网格、实时日志、筛选/搜索/跟随/下载、取消与重试 |
| Addon `/clusters/:name/addons` | 安装/升级/卸载、values 编辑、危险操作确认 |
| 包 `/packages` | 导入/校验/推送/制作 base 包、升级预览与执行、证书续期、TOML 编辑 |
| 关于 `/about` | 版本与构建信息 |

操作在服务端执行：关闭浏览器不会中断安装，重新打开后从断点继续显示日志。
服务重启后，进行中的操作标记为 `interrupted`，可在界面上重试（`retry_of`）。

## 数据与状态

| 路径 | 内容 |
|---|---|
| `<data-dir>/token` | 访问令牌（0600） |
| `<data-dir>/kurin.db` | 集群/操作/会话数据库 |
| `~/.cache/kurin/` | 制品与镜像缓存（与 CLI 共用） |
| `~/.local/state/kurin/runs/` | `--resume` 运行状态（0600） |
| `~/.cache/kurin/clusters/<name>/` | 集群 kubeconfig 等状态 |

`--data-dir` 决定前两项；容器里默认 `XDG_DATA_HOME=/data`，挂载一个卷即可
持久化。

## 安全基线

- 默认只监听回环地址；非回环必须显式 TLS 或 `--allow-insecure-http`。
- `Host` 头白名单，防 DNS rebinding。
- 变更类请求只接受 `application/json`（防 CSRF 表单）。
- 会话 cookie `SameSite=Strict`；`--rotate-token` 立即注销所有会话。
- 令牌只打印一次；丢失后用 `--rotate-token` 重新生成，不要从日志里找旧值。

更完整的设计见 [UI 架构](../ui/ui-architecture-final.md) §6.10。