# Docker：运行 CLI 与 Web UI

同一个镜像同时提供 CLI 和 Web UI：

```text
willdockerhub/kurin:0.5.1
willdockerhub/kurin:latest
```

镜像内容：`kurin` 二进制（含内嵌前端）+ `ca-certificates`。入口点是
`kurin`，默认参数 `--help`。

## 拉取

```bash
docker pull willdockerhub/kurin:0.5.1
docker run --rm willdockerhub/kurin:0.5.1 --version
```

## CLI 模式

### 查看帮助 / 生成配置

```bash
docker run --rm willdockerhub/kurin:0.5.1 --help
docker run --rm willdockerhub/kurin:0.5.1 create config
```

### 用配置安装集群

把配置、SSH 密钥和需要持久化的状态挂进容器：

```bash
docker run --rm -it \
  -v "$PWD:/work" \
  -v "$HOME/.ssh:/root/.ssh:ro" \
  -v kurin-cache:/root/.cache/kurin \
  -v kurin-state:/root/.local/state \
  -w /work \
  willdockerhub/kurin:0.5.1 \
  create cluster -f cluster.toml --dry-run
```

去掉 `--dry-run` 即真实安装。说明：

- `-v "$PWD:/work"`：配置和输出文件在容器内可见。
- `-v "$HOME/.ssh:/root/.ssh:ro"`：私钥认证；配置里的 `key-path` 要写容器内
  路径（如 `/root/.ssh/id_ed25519`）。
- `kurin-cache`：制品/镜像缓存，避免每次重新下载。
- `kurin-state`：`--resume` 状态、锁、日志。
- 密码认证用环境变量，不要把密码写进命令行：

```bash
docker run --rm -it \
  -e KURIN_SSH_PASSWORD \
  -v "$PWD:/work" -w /work \
  willdockerhub/kurin:0.5.1 \
  create cluster -f cluster.toml
```

配置里写 `password = { env = "KURIN_SSH_PASSWORD" }`。

- 如果节点在宿主机的 host-only 网络、容器默认桥接不可达，加
  `--network host`（Linux）。此时容器内 `127.0.0.1` 是宿主机。
- SELinux 主机（Fedora/RHEL）给挂载加 `:z`，例如
  `-v "$PWD:/work:z"`。
- 容器内默认 `HOME=/root`，`XDG_DATA_HOME=/data`；CLI 的缓存/状态仍在
  `/root/.cache`、`/root/.local/state`，按需挂载。

### 其他命令

```bash
# 预检
docker run --rm -v "$PWD:/work" -w /work \
  -v "$HOME/.ssh:/root/.ssh:ro" \
  willdockerhub/kurin:0.5.1 precheck -f cluster.toml

# 状态
docker run --rm -v "$PWD:/work" -w /work \
  -v "$HOME/.ssh:/root/.ssh:ro" \
  willdockerhub/kurin:0.5.1 status -f cluster.toml

# 制作离线包（需要公网）
docker run --rm -v /srv:/srv willdockerhub/kurin:0.5.1 \
  package create base --kubernetes 1.33.1 --arch amd64 \
  -o file:///srv/kurin-base.tar.zst
```

## UI 模式

### 启动

```bash
docker run -d --name kurin \
  -p 127.0.0.1:7080:7080 \
  -v kurin-data:/data \
  willdockerhub/kurin:0.5.1 \
  serve --listen 0.0.0.0:7080 \
        --allow-insecure-http \
        --public-host localhost:7080
```

- 容器内必须监听 `0.0.0.0` 才能被端口映射访问；非回环监听需要
  `--allow-insecure-http`（或 TLS）。
- `--public-host localhost:7080` 让一次性登录链接指向
  `http://localhost:7080/#token=…`，可直接在宿主机浏览器打开。
- `-v kurin-data:/data` 持久化 token 与 `kurin.db`。

获取一次性登录链接：

```bash
docker logs kurin
```

输出示例：

```text
登录链接（仅此一次）：http://localhost:7080/#token=<token>
```

打开链接即可。若 token 丢失，用同一数据卷重启并加 `--rotate-token`（新 token 会打印到
容器日志）：

```bash
docker rm -f kurin
docker run -d --name kurin \
  -p 127.0.0.1:7080:7080 \
  -v kurin-data:/data \
  willdockerhub/kurin:0.5.1 \
  serve --listen 0.0.0.0:7080 \
        --allow-insecure-http \
        --public-host localhost:7080 \
        --rotate-token
docker logs kurin
```

### TLS

把证书挂进容器，监听非回环地址：

```bash
docker run -d --name kurin \
  -p 127.0.0.1:7080:7080 \
  -v kurin-data:/data \
  -v /etc/kurin/tls:/tls:ro \
  willdockerhub/kurin:0.5.1 \
  serve --listen 0.0.0.0:7080 \
        --tls-cert /tls/fullchain.pem \
        --tls-key  /tls/privkey.pem \
        --public-host kurin.example.com
```

如果 TLS 在反向代理终止，容器内继续用 `--allow-insecure-http`，但只把端口
映射到 `127.0.0.1`，由代理对外。

### 远程访问

推荐保持 `-p 127.0.0.1:7080:7080`，通过 SSH 隧道：

```bash
ssh -L 7080:127.0.0.1:7080 user@docker-host
# 浏览器打开 http://localhost:7080/#token=…
```

### Compose

```yaml
services:
  kurin:
    image: willdockerhub/kurin:0.5.1
    container_name: kurin
    command:
      - serve
      - --listen
      - 0.0.0.0:7080
      - --allow-insecure-http
      - --public-host
      - localhost:7080
    ports:
      - "127.0.0.1:7080:7080"
    volumes:
      - kurin-data:/data
    restart: unless-stopped

volumes:
  kurin-data:
```

## 构建与推送

```bash
# 本地构建（多阶段：Node 构建前端 → Rust 编译 → Debian slim 运行）
docker build -t willdockerhub/kurin:0.5.1 .

# 冒烟
docker run --rm willdockerhub/kurin:0.5.1 --version
docker run --rm willdockerhub/kurin:0.5.1 create config | head

# 推送
docker tag willdockerhub/kurin:0.5.1 willdockerhub/kurin:latest
docker push willdockerhub/kurin:0.5.1
docker push willdockerhub/kurin:latest
```

镜像默认按构建机架构（本仓库发布为 linux/amd64）。需要多架构时：

```bash
docker buildx build --platform linux/amd64,linux/arm64 \
  -t willdockerhub/kurin:0.5.1 --push .
```

构建依赖：Docker、能访问 crates.io / npm registry / Docker Hub 的网络。
前端在 `web` 阶段用 pnpm 构建，Rust 在 `build` 阶段以
`--features ui` 编译，`web/dist` 被 rust-embed 打进二进制。

## 安全提示

- 不要把 UI 端口直接暴露到公网；优先 SSH 隧道或反向代理 + TLS。
- 明文 HTTP 会暴露 token 和会话 cookie，只用于受信任的本机/内网。
- 容器内 CLI 以 root 运行；挂载 SSH 私钥时用只读挂载，并限制容器权限。
- 生产环境建议固定版本 tag（`0.5.1`），不要用 `latest`。

## 相关

- [Web UI](../getting-started/ui.md)
- [安装](../getting-started/install.md)
- [CLI 参考](../reference/cli.md)