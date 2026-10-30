# seed 仓库（无 registry 节点的 pull 语义）

默认的 `preload` 会把镜像以 docker-archive 流式上传到节点再 `ctr images
import`。如果希望节点以 **pull** 语义拿镜像、但不想部署 registry 节点，
用 `delivery = "seed"`：

```toml
version = "v1alpha2"
name = "seed-demo"

[packages]
base = "file:///media/usb/kurin-base-v1.37.2.tar.zst"
delivery = "seed"

[nodes.node01]
address = "192.168.10.11"
role = "control-plane"
```

## 要求

- 仅 v1alpha2（v1alpha1 没有 `[packages]`）。
- 不能同时配置 `[registry]`（任何 kind）或 `role = "registry"` 节点。
- 必须有离线载荷来源：`packages.base` 或 `artifacts.offline-dir`。

## 行为

1. `registry/seed-serve@controller`（registry 阶段，先于 images）在控制机
   启动一个**只读** OCI registry，直接读本地包/blob 缓存；按
   `mirror/<上游主机>/<路径>` 布局登记镜像。
2. `registry/seed-mirror@node` 对每个节点建立 SSH 远程转发
   （`ssh -R`），把控制机的服务暴露到节点的 `127.0.0.1:<port>`，并写运行时
   mirror：
   - containerd：`hosts.toml`（显式 `http://` host）
   - CRI-O：`registries.conf.d`（`insecure = true`）
   - k3s：`registries.yaml`
3. `images/import` 变为 no-op：镜像由运行时经 mirror 拉取；二进制载荷仍走
   SSH 流式上传。
4. 操作结束即拆除：控制机停掉 seed 仓库，SSH 转发随会话池消失。它**不能**
   作为集群长期的镜像来源。

## 安全边界

- 只绑 `127.0.0.1`、无认证、只读（`POST/PUT/DELETE` 与 `blobs/uploads/`
  一律 405）。
- 节点只能经自己的 SSH 连接看到它，节点之间互不可见；控制机之外不可达。

## 验证

安装时事件流应出现：

```text
registry/seed-serve@controller   done
registry/seed-mirror@node01      done
images/import@node01             skip (no-op mirror delivery)
```

节点上：

```bash
crictl images | head
grep -r 127.0.0.1 /etc/containerd/certs.d/ 2>/dev/null
```

安装结束后，节点到 `127.0.0.1:<port>` 的连接应失效（转发已拆除）。

## 相关

- [配置参考 §种子仓库](../reference/configuration.md#种子仓库delivery--seedwp-77)
- [离线安装](offline.md)
- [私有 registry](registry.md)