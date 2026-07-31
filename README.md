# windplume-deploy

面向可信内网的轻量 Docker Compose 部署面板。程序以单个 Rust 二进制运行，内嵌中文
网页；配置 Compose 文件后，会自动识别项目、服务和镜像，并从 Docker Hub、GHCR
或标准 OCI Registry 查询 tag。

主要能力：

- 按服务选择版本，拉取镜像并固定到不可变 digest；
- 检查容器运行状态和 healthcheck，失败时回退；
- 回滚上一成功版本，重建、停止、下线服务；
- 查看最近容器日志和部署历史；
- 在独立“系统资源”页查看主机与 Docker 空间，安全清理历史镜像和构建缓存；
- 通过签名 GitHub Release 手动更新管理程序。

项目不提供用户认证、权限、Webhook 或自动部署，默认监听 `127.0.0.1:8180`。所有
页面写操作使用运行时 CSRF Token，并校验浏览器 `Origin` 与请求 `Host`；这只能
防止跨站请求，不能识别操作者身份。

## 安装与配置

运行环境需要 Linux、Docker、Docker Compose v2；源码构建还需要 Rust 1.96 或更新
版本。下载 GitHub Release 并解压后执行：

```bash
sudo ./install.sh
sudoedit /etc/windplume-deploy/config.yaml
sudo systemctl start windplume-deploy
```

安装脚本会创建 `windplume-deploy` 系统用户，将其加入 `docker` 组，安装 systemd
服务、自更新助手和签名公钥，并保留已有配置。脚本还会让服务用户可读取
`/opt/windplume` 下的 Compose、`.env` 和 `env_file`。

最小配置只需要 Compose 文件的绝对路径：

```yaml
projects:
  - compose_files:
      - /opt/my-project/compose.yaml
```

多个 Compose 文件按声明顺序作为 `-f` 参数合并。以下是可覆盖的默认值：

```yaml
server:
  listen: 127.0.0.1:8180

registries:
  cache_seconds: 604800

storage:
  data_dir: /var/lib/windplume-deploy
  history_limit: 500
  max_log_bytes: 65536

projects:
  - compose_files:
      - /opt/my-project/compose.yaml
      - /opt/my-project/compose.production.yaml
    health_timeout_seconds: 120
    command_timeout_seconds: 600
```

程序通过 `docker compose config` 读取最终项目名、服务名和环境变量替换后的镜像。
带 `image:` 的服务会显示在页面中；只有 `build:`、没有 `image:` 的服务会被忽略。
符合 `x.y.z` 的 tag 按 SemVer 排序，其他非空 tag 仍可手动选择。版本结果默认缓存
7 天，可在页面强制刷新。

修改 Compose 后可点击“刷新 Compose”重新识别服务和镜像。刷新不会执行 `pull`、
`up`，也不会把环境变量、挂载等变更立即应用到容器；项目名变化仍需重启管理服务。

第一份 Compose 文件的父目录同时作为命令工作目录和 `--project-directory`，因此
相对挂载、`env_file`、构建上下文及多文件合并仍遵循 Docker Compose 的路径规则。
程序只在数据目录生成镜像 override，不修改业务 Compose 或环境文件。

## 部署与回退

部署一个 tag 时，程序会：

1. 写入目标服务的临时镜像 override，并执行 `docker compose pull`；
2. 读取 RepoDigest 和本地 Image ID，将 override 固定为 `repository@sha256:...`；
3. 执行 `docker compose up -d --no-deps`；
4. 等待所有副本进入 `healthy`，无 healthcheck 时等待 `running`，并确认 Image ID；
5. 成功后提交状态，失败时恢复旧 override 并尝试回退上一制品。

tag 只作为版本入口和显示值；部署一致性与回退以 digest 和 Image ID 为准。服务卡片
还提供：

- **回滚上一版本**：恢复上一成功部署的不可变镜像；
- **重建当前版本**：重新读取 Compose 和环境文件，不主动拉取或切换版本；
- **停止**：停止但保留容器；
- **下线**：停止并删除服务容器，不删除镜像、卷、绑定目录或共享网络。

同一项目的操作互斥执行，并记录到操作历史；页面可清除 30 天前已完成的记录。
部署阶段会持久化，管理程序重启后会检查候选容器：符合目标镜像且健康时补记成功，
否则尝试恢复上一提交版本。容器日志按需读取最近 50 行，向上滚动最多扩展到 200
行，不持续跟踪或入库。

## 系统空间与镜像清理

页面按需读取根文件系统、数据目录所在文件系统、内存、负载、运行时间及 Docker
空间，不持续采集或保存历史。系统盘与数据盘位于同一挂载点时会明确标记。Docker
的“可回收空间”和镜像标称大小可能包含共享层，实际释放量可能更少。

镜像列表只显示当前 Compose 服务涉及的 repository。镜像满足以下全部条件时才可
手动清理：

- 没有被运行或停止状态的容器引用；
- 不是当前已提交镜像或上一回退镜像；
- 删除前重新检查后仍属于受管 repository。

程序按完整 Image ID 删除，不调用全局 `docker image prune -a`，也不删除容器或卷。
部署、回退、自更新或其他清理运行时会拒绝新清理。构建缓存需单独确认，只清理 7
天前未使用的缓存；之后构建可能需要重新下载依赖。

## 管理程序自更新

页面只接受固定公开仓库中的稳定 `vX.Y.Z` Release。普通服务进程校验资产名称、
下载地址、归档 SHA-256 和大小后暂存候选二进制；root 更新助手再使用安装时固定的
RSA 公钥验证独立签名和候选版本，然后替换 `/usr/local/bin/windplume-deploy`。
候选版本未能保持运行时，助手会尝试恢复旧二进制；只有旧版本确认稳定运行后才标记
回退成功，新旧版本均无法启动时会明确标记失败。业务容器不会因此停止。

更新阶段和结果同时写入状态文件与 systemd journal。管理页面不可用时可通过 SSH
排查：

```bash
journalctl -u windplume-deploy.service -u windplume-deploy-update.service -e
```

自更新控制文件固定位于 `/var/lib/windplume-deploy/update`，不受自定义
`storage.data_dir` 影响。首次启用或 Release 要求更新 systemd/helper 协议时，必须
人工重新运行新版 `install.sh`；普通后续版本只替换主二进制。

### 从 0.1 升级到 0.2

0.2 的 digest 状态模型不兼容 0.1 SQLite 数据库。首次升级必须停止服务，备份或
删除 `deploy.db`、`deploy.db-wal`、`deploy.db-shm`，人工安装 0.2 Release 后再启动。
重建数据库不会删除或重启业务容器，但已有服务会显示为“未纳管”，直到第一次通过
0.2 成功部署建立新基线。后续 0.2.x 可使用页面自更新。

## 私有 Registry 与安全边界

公开 Docker Hub/GHCR 通常无需登录。私有 Registry 可让服务用户执行登录，例如：

```bash
sudo -H -u windplume-deploy docker login ghcr.io -u YOUR_GITHUB_USERNAME
```

Docker 配置目录固定为 `/var/lib/windplume-deploy/.docker`。程序只能读取
`config.json` 中的内联 `auth`，不能调用系统 credential helper；GHCR 私有包需要
具有 `read:packages` 权限的 classic PAT。

服务用户可访问 Docker socket，主机权限实际上接近 root。只应向可信内网开放；
监听 `0.0.0.0` 时应配合防火墙或带认证的反向代理，并保留原始 `Host`。

## 开发与发布

```bash
cargo build --release
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
```

本地运行：

```bash
RUST_LOG=windplume_deploy=debug cargo run -- --config ./config.yaml
```

发布时先更新 `Cargo.toml` 版本，再推送完全一致的 `vX.Y.Z` 标签。GitHub Actions 会
运行格式、Clippy、测试、真实 Docker Compose 集成、ShellCheck 和依赖审计，随后
构建 x86_64 Linux musl 二进制、生成 SHA-256 和 RSA 签名并创建 Release。签名私钥
只保存在 GitHub `release` Environment；轮换密钥时必须先通过人工安装包部署新公钥。

运行状态可通过以下命令检查：

```bash
systemctl status windplume-deploy
journalctl -u windplume-deploy -f
curl http://127.0.0.1:8180/health
```
