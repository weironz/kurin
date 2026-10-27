# WP-30 Spike：依赖与仓库兼容性

> 状态：完成 · 2026-10-27 · 对应 [06-dev-plan §5 WP-30](../06-dev-plan.md) 与 [04-oci-packages §8/§9](../04-oci-packages.md)
> Spike 代码：`spikes/oci/`（独立 crate，`Cargo.toml` 内含空 `[workspace]`，不进入根 workspace，也不进入发布构建）
> 测试环境：Linux x86_64 · Docker 无 sudo · rustc 1.99.1 · cargo-zigbuild 0.24.0 · zig 0.14.1

本 spike 只回答三个问题：

1. `oci-client` / `oci-spec` / `zstd` / `rusqlite(bundled)` 能否为 `x86_64-unknown-linux-musl` 与 `aarch64-unknown-linux-musl` 交叉编译？
2. zot、distribution v3、harbor 如何对待“子 manifest 不全”的多架构 index？需要什么配置才能接受？
3. 三者是否接受 OCI 1.1 的 `artifactType` manifest/index，是否实现 referrers API？

---

## 0. 结论摘要

| 问题 | 结论 |
|---|---|
| 依赖交叉编译 | **可行**。四个直接依赖及其 C 依赖（`zstd-sys`、`libsqlite3-sys`、rustls 的 `aws-lc-rs`）在 `cargo zigbuild` 下两个 musl 目标均构建成功，产物为静态 ELF。**不需要** 04 §8 的 `ruzstd` + gzip 兜底。 |
| `cargo build --target x86_64-unknown-linux-musl` | spike 当时**失败**，原因是本机缺少 `x86_64-linux-musl-gcc`（`cc-rs` 报 `ToolNotFound`），不是 crate 本身不可交叉编译。改用 `cargo zigbuild` 后成功。后续已安装 `musl-tools`，原生 musl-gcc 路径同样可用（`just release-musl` 已实测通过）。 |
| 只含 amd64 子 manifest 的 index | 三个仓库**都接受**（index 本身完整，只是平台集合只有 amd64）。 |
| index 声称 amd64+arm64、但 arm64 子 manifest 未推送 | **zot 接受（201）**；**distribution v3.1.2 拒绝（400 MANIFEST_BLOB_UNKNOWN）**；**harbor 2.13.0 拒绝（400 MANIFEST_BLOB_UNKNOWN）**。distribution/harbor 均无放宽配置（zot 无需配置）。 |
| 自定义 `artifactType` 的 manifest/index | 三个仓库**都接受并原样保留** `artifactType`。 |
| referrers API | **zot：支持**（含 `artifactType` 过滤、`OCI-Filters-Applied`）；**harbor：支持**（含过滤、推送时返回 `OCI-Subject`）；**distribution v3.1.2：不支持**（`404 page not found`，也没有 fallback tag）。 |

对 kurin 的直接结论：

- 默认必须推送 index 引用的**全部**子 manifest；`--full-multiarch` 不是可选项而是 distribution/harbor 的硬要求。zot 虽然接受残缺 index，但其他架构的节点拉取时会 404，因此也不能依赖。
- SBOM/签名发现不能只依赖 referrers API：distribution v3 没有该 API，也没有 fallback tag。kurin 的包 index 应显式携带 SBOM/签名 descriptor（04 §3.1 已这样设计），referrers 仅作为可选增强。
- `oci-spec` 必须与 `oci-client` 使用的版本对齐（0.9.x），否则依赖图里会出现两份 `oci-spec`，类型不能互换。

---

## 1. Spike 代码

```text
spikes/oci/
├── Cargo.toml            # [workspace] 空表 → 独立于根 workspace
├── src/main.rs           # 探针程序（clap 子命令）
├── compose/
│   ├── docker-compose.yml    # zot + distribution v3 + harbor 2.13.0
│   ├── zot-config.json
│   ├── distribution-config.yml
│   └── harbor/               # harbor.yml + prepare 生成的配置/数据（.gitignore）
└── .gitignore                # target、仓库数据与 harbor 生成的密钥/配置不入库
```

程序子命令：

| 子命令 | 作用 |
|---|---|
| `deps` | 运行 zstd 往返、rusqlite 内存库、`oci-spec` 反序列化、`oci-client` 类型构造，证明链接与运行正常 |
| `push-multiarch` | 推送 amd64 镜像（config+layer+manifest），再推送 index；`--mode complete` 只含 amd64，`--mode missing-arm64`/`both` 额外声明一个从未推送的 arm64 子 manifest |
| `push-artifact` | 推送带自定义 `artifactType` 与 `subject` 的 OCI 1.1 manifest，再用 `oci-client::push_manifest` 推送带自定义 `artifactType` 的 index |
| `referrers` | 直接 GET referrers API（打印状态码/关键响应头/正文），并用 `oci_client::pull_referrers` 再查一次 |
| `get` | 按 tag/digest 取 manifest/index |

所有推送都走 `oci-client` 的公开 API（`push_blob`、`push_manifest_raw`、`push_manifest`），没有手写 registry 协议。

---

## 2. 依赖交叉编译

### 2.1 版本矩阵

| 依赖 | 版本 | 含 C/汇编 | x86_64-musl | aarch64-musl |
|---|---|---|---|---|
| `oci-client` | 0.16.1 | 否（纯 Rust，rustls） | ✅ | ✅ |
| `oci-spec` | 0.9.0 | 否 | ✅ | ✅ |
| `zstd` → `zstd-sys` | 0.13.3 → 2.1.2+zstd.1.5.7 | 是（C + amd64 汇编） | ✅ | ✅ |
| `rusqlite` → `libsqlite3-sys` | 0.37.0 → 0.35.0（SQLite 3.50.2） | 是（C，bundled） | ✅ | ✅ |
| `rustls` → `aws-lc-rs` | 0.23.46 → 1.18.2 | 是（C/汇编） | ✅ | ✅ |

> `oci-client` 0.16.1 依赖 `oci-spec 0.9.0`。若 crate 里再写 `oci-spec = "0.8"`，依赖图会出现两份 `oci-spec`，`Descriptor`/`Digest` 等类型不互通（编译报 `trait bound ... not satisfied`）。spike 已改为 `0.9`，`cargo tree -i oci-spec` 只剩一份。

### 2.2 命令与结果

环境准备（无 sudo）：

```bash
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo install cargo-zigbuild --locked          # 0.24.0
# zig 0.14.1 解压到 ~/.local/opt/zig-0.14.1，软链到 ~/.local/bin/zig
```

目标 1：`x86_64-unknown-linux-musl`

```console
$ cd spikes/oci && cargo build --target x86_64-unknown-linux-musl
  cargo:warning=Compiler family detection failed due to error: ToolNotFound:
      failed to find tool "x86_64-linux-musl-gcc": No such file or directory (os error 2)
  error occurred in cc-rs: failed to find tool "x86_64-linux-musl-gcc":
      No such file or directory (os error 2)
EXIT=101
```

这是本机缺少原生 musl 工具链，不是依赖问题。用 zig 提供 C 工具链：

```console
$ cargo zigbuild --target x86_64-unknown-linux-musl
    Finished `dev` profile [unoptimized + debuginfo] target(s)
EXIT=0
```

目标 2：`aarch64-unknown-linux-musl`

```console
$ cargo zigbuild --target aarch64-unknown-linux-musl
    Finished `dev` profile [unoptimized + debuginfo] target(s)
EXIT=0
```

产物与运行验证：

```console
$ file target/x86_64-unknown-linux-musl/debug/kurin-oci-spike \
       target/aarch64-unknown-linux-musl/debug/kurin-oci-spike
...: ELF 64-bit LSB executable, x86-64, ... statically linked, with debug_info
...: ELF 64-bit LSB executable, ARM aarch64, ... statically linked, with debug_info

$ ./target/x86_64-unknown-linux-musl/debug/kurin-oci-spike deps
zstd: ok (1792 -> 46 bytes, round-trip identical)
rusqlite: ok (bundled 3.50.2, row k 1792 bytes)
oci-spec: ok ({"mediaType":"application/vnd.oci.image.manifest.v1+json",...})
oci-client: ok (content-type application/vnd.oci.image.index.v1+json)
RUN_EXIT=0
```

### 2.3 一个失败的替代方案（记录，避免后续踩坑）

尝试用 `CC_x86_64_unknown_linux_musl="zig cc -target x86_64-linux-musl"` 直接跑 `cargo build`：

```console
error: unable to parse target query 'x86_64-unknown-linux-musl': UnknownOperatingSystem
```

原因：`cc-rs` 会额外追加 `--target=x86_64-unknown-linux-musl`（Rust triple），而 zig 只认 `x86_64-linux-musl`。**不要**用 `CC=zig cc` 手工替代 `cargo-zigbuild`。

### 2.4 结论

- 04 §8 的“不可行时改用 `ruzstd`（只解压）+ gzip”**不需要启用**；`zstd-sys` 与 bundled SQLite 在 zig 下两个 musl 目标都能编译链接。
- 根 `justfile` 的 `release-musl` 目前 x86_64 走原生 `musl-gcc`。CI/发布机需安装 `musl-tools`（本机已安装并实测通过）；没有原生工具链时统一改为 `cargo zigbuild`（本 spike 已验证后者）。
- 新增依赖的 PR 描述可直接引用本节结论。

---

## 3. 测试环境

### 3.1 容器与端口

全部为新容器，名字带 `kurin-spike-` 前缀，端口 ≥ 15000；未触碰既有 `local_registry` / `kubeasz`。

| 服务 | 镜像 | 容器名 | 宿主端口 |
|---|---|---|---|
| zot | `ghcr.io/project-zot/zot-linux-amd64:latest`（v2.1.22，distribution-spec 1.1.1） | `kurin-spike-zot` | 15000 |
| distribution | `registry:3`（v3.1.2） | `kurin-spike-distribution` | 15001 |
| harbor | `goharbor/*:v2.13.0`（内置 registry 为 `docker/distribution v2.8.3-21-gea065fef`） | `kurin-spike-harbor-*` / `kurin-spike-nginx` | 15080 |

### 3.2 启动

```bash
cd spikes/oci/compose
docker compose -p kurin-spike up -d
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:15000/v2/   # 200
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:15001/v2/   # 200
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:15080/v2/   # 401（需认证）
```

docker compose 插件本机原本缺失，已安装到 `~/.docker/cli-plugins/docker-compose`（无需 sudo）。

harbor 首次需要生成配置。`goharbor/prepare` 以 root 写文件，之后要把属主改回当前用户，并保证配置文件可被容器内 uid 10000 读取（否则 `harbor-log` 的 rsyslog 读不到配置，报 `no active actions configured`）：

```bash
mkdir -p harbor/input && cp harbor/harbor.yml harbor/input/harbor.yml
docker run --rm -v $PWD/harbor/input:/input -v $PWD/harbor/data:/data \
  -v $PWD/harbor:/compose_location -v $PWD/harbor/common/config:/config \
  -v /:/hostfs --privileged goharbor/prepare:v2.13.0 prepare
# 配置目录归当前用户；数据目录按容器内 uid 授权
docker run --rm -v $PWD/harbor:/w alpine:3.20 sh -c '
  chown -R 1003:1003 /w/common /w/harbor.yml
  chown -R 999:999   /w/data/database /w/data/redis
  chown -R 10000:10000 /w/data/registry /w/data/job_logs /w/data/ca_download /w/data/logs /w/data/secret
  chmod -R a+rX /w/common/config'
# 1003 换成当前用户的 uid:gid（id -u / id -g）
```

> `harbor/harbor.yml` 里的 `data_volume` 是绝对路径，换机器复现时改成自己的路径。

创建测试项目（harbor 匿名推送默认不允许）：

```bash
curl -u admin:Harbor12345 -X POST http://localhost:15080/api/v2.0/projects \
  -H 'Content-Type: application/json' -d '{"project_name":"spike","public":true}'
# HTTP 201
```

### 3.3 配置要点

**zot**（`compose/zot-config.json`）：

```json
{
  "distSpecVersion": "1.1.0",
  "storage": { "rootDirectory": "/var/lib/registry" },
  "http": { "address": "0.0.0.0", "port": "15000" },
  "log": { "level": "debug" }
}
```

> 注意：**不要**写 `"extensions": {"referrers": {"enabled": true}}`。zot v2.1.22 会启动失败：
> `'Extensions' has invalid keys: referrers`。zot 的 referrers 是内建且始终开启的，没有开关。

**distribution v3**（`compose/distribution-config.yml`）：

```yaml
version: 0.1
storage:
  filesystem:
    rootdirectory: /var/lib/registry
  delete:
    enabled: true
http:
  addr: :15001
```

没有任何 referrers/多架构校验相关的配置项；`v3.1.2` 源码中不存在 referrers 实现（`grep -ri referrer` 仅命中认证文档）。

**harbor**：默认配置即可；referrers 与 `OCI-Subject` 开箱可用。index 子 manifest 校验由内置 registry 完成，无开关。

---

## 4. 多架构 index 行为

测试镜像内容固定：config 90 B、layer 30 B、amd64 manifest 441 B；index 中 amd64 条目指向真实 manifest，arm64 条目指向一个从未推送的 `sha256:1111…1111`。

### 4.1 场景 A：index 只含 amd64（完整）

```bash
B=./target/x86_64-unknown-linux-musl/debug/kurin-oci-spike
$B push-multiarch --registry http://localhost:15000 --repo spike/multiarch --tag v1 --mode complete
$B push-multiarch --registry http://localhost:15001 --repo spike/multiarch --tag v1 --mode complete
$B push-multiarch --registry http://localhost:15080 --repo spike/multiarch --tag v1 --mode complete \
   --username admin --password Harbor12345
```

三个仓库结果一致：

```console
$ curl -s -o /dev/null -w '%{http_code}\n' \
    -H 'Accept: application/vnd.oci.image.index.v1+json' \
    http://localhost:15000/v2/spike/multiarch/manifests/v1     # 200
$ curl ... http://localhost:15001/v2/spike/multiarch/manifests/v1  # 200
$ curl -u admin:Harbor12345 ... http://localhost:15080/v2/spike/multiarch/manifests/v1  # 200
```

返回体（三者相同）：

```json
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json",
 "manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json",
 "digest":"sha256:c25ccfe3f7fc4fd1f1178386c4a5d45efebca03add21a062f93199ae05f39e3b",
 "size":441,"platform":{"architecture":"amd64","os":"linux"}}]}
```

**结论：只携带所选架构的子 manifest 与 blob 是可行的**，前提是 index 只声明实际存在的条目。

### 4.2 场景 B：index 声明 amd64+arm64，但 arm64 未推送

```bash
$B push-multiarch --registry http://localhost:15000 --repo spike/multiarch --tag v1 --mode both
```

zot：

```console
== push index as tag v1-both (mode=both)
  manifest sha256:c57fd579... (application/vnd.oci.image.index.v1+json) -> ...
== GET /v2/spike/multiarch/manifests/v1-both -> 200 OK
  body: {..."manifests":[{amd64...},{"digest":"sha256:1111...","platform":{"architecture":"arm64","os":"linux"}}]}
== GET missing arm64 child sha256:1111... -> 404 Not Found
```

distribution v3.1.2：

```console
Error: push manifest as application/vnd.oci.image.index.v1+json
Caused by: Server error: url http://localhost:15001/v2/spike/multiarch/manifests/v1-both,
  code: 400, message: {"errors":[{"code":"MANIFEST_BLOB_UNKNOWN",
  "message":"blob unknown to registry","detail":"sha256:1111...1111"}]}
```

harbor 2.13.0：

```console
Caused by: Server error: url http://localhost:15080/v2/spike/multiarch/manifests/v1-both,
  code: 400, message: {"errors":[{"code":"MANIFEST_BLOB_UNKNOWN",
  "message":"blob unknown to registry","detail":"sha256:1111...1111"}]}
```

直接 curl 复现：

```console
$ curl -X PUT -H 'Content-Type: application/vnd.oci.image.index.v1+json' \
    --data-binary @idx-missing.json \
    http://localhost:15001/v2/spike/multiarch/manifests/incomplete
HTTP 400
{"errors":[{"code":"MANIFEST_BLOB_UNKNOWN",...}]}
```

### 4.3 结论表

| 仓库 | 版本 | 完整 amd64-only index | 声明了未推送子 manifest 的 index | 可放宽的配置 |
|---|---|---|---|---|
| zot | v2.1.22 | 201 / 200 | **201（接受）**，缺失子 manifest 取时 404 | 无需配置（行为固定） |
| distribution | v3.1.2 | 201 / 200 | **400 MANIFEST_BLOB_UNKNOWN** | **无**（源码无该开关） |
| harbor | v2.13.0 | 201 / 200 | **400 MANIFEST_BLOB_UNKNOWN** | **无** |

**对 kurin 的要求**：`kurin package push` 必须在推送 index 之前确认所有 `manifests[]` 条目都已存在于目标仓库；`--full-multiarch` 的语义应是“补齐所有架构的子 manifest”，而不是“允许残缺 index”。对 zot 也不能利用其宽松行为——其他架构节点拉取时会 404。

---

## 5. `artifactType` 与 referrers

### 5.1 推送 OCI 1.1 artifact

```bash
$B push-artifact --registry http://localhost:15000 --repo spike/artifacts --tag sbom3
```

推送内容：

- artifact manifest：`artifactType = application/vnd.kurin.spike.v1`，`config.mediaType = application/vnd.oci.empty.v1+json`，`subject` 指向基础镜像 manifest，`layers[0]` 带 `org.opencontainers.image.title`。
- index：`artifactType = application/vnd.kurin.spike.v1`，通过 `oci-client::Client::push_manifest`（typed API）推送。

三个仓库都返回 201，且重新 GET 后 `artifactType` 原样保留：

```console
== GET /v2/spike/artifacts/manifests/sbom3-index -> 200 OK
{"artifactType":"application/vnd.kurin.spike.v1","manifests":[...],
 "mediaType":"application/vnd.oci.image.index.v1+json","schemaVersion":2}
```

> **API 陷阱**：`oci-client 0.16.1` 的 `ImageIndexEntry` **没有** `artifactType` 字段。用 typed `push_manifest` 推送 index 时，条目级的 `artifactType` 会丢失（index 级的会保留）。若需要条目级 `artifactType`（04 §3.1 的包 index 需要），必须用 `push_manifest_raw` 自行序列化 JSON。spike 的 `push-artifact` 同时演示了两条路径。

### 5.2 referrers API

```bash
SUB=sha256:c25ccfe3f7fc4fd1f1178386c4a5d45efebca03add21a062f93199ae05f39e3b
curl -D - http://localhost:15000/v2/spike/artifacts/referrers/$SUB
```

| 仓库 | `GET /v2/<repo>/referrers/<digest>` | `?artifactType=` 过滤 | `OCI-Filters-Applied` | 推送带 `subject` 时的 `OCI-Subject` | fallback tag `sha256-<hex>` |
|---|---|---|---|---|---|
| zot v2.1.22 | **200** + index | ✅ 返回匹配项 | ✅ `artifactType` | ✅ | 不需要 |
| distribution v3.1.2 | **404** `404 page not found` | — | — | ❌ 无该响应头 | ❌ 未创建 |
| harbor v2.13.0 | **200** + index | ✅ 返回匹配项 | ✅ `artifactType` | ✅ | 不需要 |

原始证据：

```console
# zot
HTTP 200
Content-Type: application/vnd.oci.image.index.v1+json
{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[
 {"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:b484...","size":668,
  "artifactType":"application/vnd.kurin.spike.v1"}, ...]}

# distribution
HTTP 404
Content-Type: text/plain; charset=utf-8
404 page not found

# harbor
HTTP 200
Content-Type: application/vnd.oci.image.index.v1+json
{"schemaVersion":2,...,"manifests":[{... "artifactType":"application/vnd.kurin.spike.v1"}, ...]}
```

过滤请求（zot / harbor 均返回 `OCI-Filters-Applied: artifactType`）：

```console
$ curl -D - 'http://localhost:15000/v2/spike/artifacts/referrers/sha256:c25c...?artifactType=application%2Fvnd.kurin.spike.v1'
HTTP 200
Oci-Filters-Applied: artifactType
```

`oci-client::pull_referrers` 的可用性：**必须传 digest 引用**（`<host>/<repo>@sha256:...`），传 tag 会报
`Getting referrers for a tag is not supported`。按 digest 调用时：

```console
# zot
oci-client pull_referrers: ok, 2 referrer(s): ["sha256:b484...", "sha256:bbe5..."]
# harbor
oci-client pull_referrers: ok, 3 referrer(s): [...]
# distribution
oci-client pull_referrers: ERROR Server error: ... code: 404, message: 404 page not found
```

### 5.3 推送 `subject` 时各仓库的响应头

```console
# zot
HTTP/1.1 201 Created
Docker-Content-Digest: sha256:1a9b028f...
Oci-Subject: sha256:c25ccfe3...

# distribution
HTTP/1.1 201 Created
Docker-Content-Digest: sha256:1a9b028f...
（无 Oci-Subject）

# harbor
HTTP/1.1 201 Created
Docker-Content-Digest: sha256:1a9b028f...
Oci-Subject: sha256:c25ccfe3...
```

distribution 接受并存储了带 `subject` 的 manifest，但既不建立索引也不提供查询；`tags/list` 中也没有 fallback tag：

```json
{"name":"spike/artifacts","tags":["sbom","sbom-base","sbom-index","sbom2"]}
```

**对 kurin 的要求**：SBOM/签名发现必须走“包 index 内显式 descriptor”这条主路径（04 §3.1 已如此设计）。referrers API 只能作为可选增强；对 distribution v3 需要能容忍 404，并且不要假设 fallback tag 存在。

---

## 6. 对 04 的修订

本 spike 证明 04 有两处需要按实测结果修正（已在同一 PR 中修改 `docs/architecture/04-oci-packages.md`）：

1. **§8 “交叉编译”行**：原文为“需与 SQLite 一起在 x86_64 musl 与 aarch64 zig 下验证；不可行时改用 `ruzstd`”。实测两者均可交叉编译，已改为“已验证；`cargo zigbuild` 同时覆盖两个目标，`ruzstd` 仅作应急”，并补充 `oci-spec` 版本对齐与 `ImageIndexEntry` 缺 `artifactType` 的注意事项。
2. **§9 第一行风险**：原文为“spike 验证 zot、distribution、harbor”。实测结论已写入：zot 接受残缺 index，distribution v3 与 harbor 拒绝；三者均无放宽配置。因此 `--full-multiarch` 的定位改为“必须补齐子 manifest”，而不是“兜底选项”。

§3.1 的包格式设计本身**不需要修改**：它已经要求“多架构镜像默认只携带所选架构的子 manifest 与 blob，index 本身原样保留”，并且把 SBOM/签名放在 index 的 referrers 中。实测只影响“推送顺序/完整性校验”和“referrers 不可用时的降级”，不改变格式。

---

## 7. 对后续 WP 的落地建议

- **WP-31（layout/store）**：`oci-spec` 锁 `0.9`；`rusqlite` 用 `bundled`；zstd 用 `zstd` crate 即可，无需 `ruzstd`。
- **WP-33/34（registry 分发）**：推送前先 `HEAD /v2/<repo>/manifests/<child-digest>` 校验每个子 manifest；缺失则先补推，再推 index。不要依赖 zot 的宽松行为。
- **WP-35/36（签名/SBOM）**：主路径写进包 index 的 descriptor；referrers 查询失败（404）时静默降级；对 distribution v3 不要写 fallback tag。
- **WP-37（kurin 管理的仓库）**：如果 kurin 自己部署 zot，配置里不要写 `extensions.referrers`；如果部署 distribution v3，需接受“没有 referrers API”这一事实。

---

## 8. 复现命令清单

```bash
# 1) 交叉编译
cd spikes/oci
cargo zigbuild --target x86_64-unknown-linux-musl
cargo zigbuild --target aarch64-unknown-linux-musl
./target/x86_64-unknown-linux-musl/debug/kurin-oci-spike deps

# 2) 起仓库
cd compose && docker compose -p kurin-spike up -d

# 3) 多架构 index
B=../target/x86_64-unknown-linux-musl/debug/kurin-oci-spike
$B push-multiarch --registry http://localhost:15000 --repo spike/multiarch --tag v1 --mode complete
$B push-multiarch --registry http://localhost:15000 --repo spike/multiarch --tag v1 --mode both   # zot 接受
$B push-multiarch --registry http://localhost:15001 --repo spike/multiarch --tag v1 --mode both   # 400
$B push-multiarch --registry http://localhost:15080 --repo spike/multiarch --tag v1 --mode both \
   --username admin --password Harbor12345                                                        # 400

# 4) artifactType + referrers
$B push-artifact --registry http://localhost:15000 --repo spike/artifacts --tag sbom3
$B referrers --registry http://localhost:15000 --repo spike/artifacts --digest <subject-digest>
$B referrers --registry http://localhost:15001 --repo spike/artifacts --digest <subject-digest>
$B referrers --registry http://localhost:15080 --repo spike/artifacts --digest <subject-digest> \
   --username admin --password Harbor12345

# 5) 清理
cd compose && docker compose -p kurin-spike down -v
```

---

## 9. 环境清理

测试结束后执行 `docker compose -p kurin-spike down -v`，删除全部 `kurin-spike-*` 容器与网络；既有 `local_registry`、`kubeasz` 未做任何改动。harbor 数据目录 `spikes/oci/compose/harbor/data` 与 `data/{zot,distribution}` 为本地测试数据，可随时删除。