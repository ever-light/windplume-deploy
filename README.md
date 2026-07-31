# windplume-deploy

一个单二进制、内嵌中文网页的内网 Docker Compose 版本部署服务。只需提供
Compose 文件路径，程序会调用 `docker compose config` 自动识别项目名、服务和
镜像，从公开 Docker Hub、GHCR 或标准 OCI Registry 读取 tag，并支持逐服务选择
版本、显式拉取、健康检查和失败回退。

服务不包含应用登录、权限、Webhook、无人值守自动部署或 Docker 凭据管理，默认
只监听 `127.0.0.1:8180`。所有页面写操作使用运行时 CSRF Token，并校验浏览器
`Origin` 与请求 `Host`；这用于阻止跨站请求，不等同于用户认证。

## 配置

最小配置只包含 Compose 文件的绝对路径：

```yaml
projects:
  - compose_files:
      - /opt/my-project/compose.yaml
```

多个 `-f` 文件按配置顺序合并：

```yaml
projects:
  - compose_files:
      - /opt/my-project/compose.yaml
      - /opt/my-project/compose.production.yaml
    health_timeout_seconds: 180
    command_timeout_seconds: 900
```

程序从 Compose 的最终规范化结果自动取得：

- Compose 项目名，并将其用作内部项目 ID 和后续 `--project-name`；
- `services:` 下的服务名；
- 环境变量替换后的最终 `image`；
- Docker Hub、GHCR 或其他 OCI Registry 的 repository。

所有带 `image:` 的服务都会显示在页面中。只有 `build:`、没有 `image:` 的本地
构建服务会被忽略。默认列出 Registry 返回的全部非空 tag；符合 `x.y.z` 的 tag
按 SemVer 排序并可使用“部署最新版本”，其他 tag 仍可手动选择。

修改已配置的 Compose 文件后，可在对应项目标题旁点击“刷新 Compose”。
刷新会重新识别服务、镜像和 Registry 来源，但不会执行 `pull` 或 `up`，
也不会立即将挂载、环境变量等变更应用到运行中的容器。修改 Compose
项目名时仍需重启服务。

`storage.data_dir` 默认是 `/var/lib/windplume-deploy`。镜像版本列表默认在内存中缓存
7 天，程序重启后重新查询 Registry；页面的“刷新版本”可随时强制更新。
监听地址、缓存和存储限制仅在需要覆盖时配置：

```yaml
server:
  listen: 127.0.0.1:8180

registries:
  cache_seconds: 604800

storage:
  data_dir: /var/lib/windplume-deploy
  history_limit: 500
  max_log_bytes: 65536
```

## 相对路径

程序始终把第一份 Compose 文件的父目录同时用作命令工作目录和
`--project-directory`，原始 Compose 文件始终位于生成的镜像 override 之前。
因此以下相对路径不会因为 override 保存在数据目录而改变：

```yaml
services:
  api:
    image: ghcr.io/example/api:1.0.0
    volumes:
      - ./data:/app/data
    env_file:
      - ./api.env
    build:
      context: ./backend
```

使用多个 `-f` 文件时，Docker Compose 按自身规则以第一份文件为相对路径基准。
若 Compose 使用顶层 `include`，included 文件仍由 Compose 按其各自项目目录
解析。

## 部署流程

选择一个服务和 tag 后，程序会：

1. 在数据目录生成只覆盖目标服务镜像的 `compose.deploy.yaml`；
2. 执行 `docker compose pull <service>`；
3. 读取已拉取镜像的 RepoDigest 和本地 Image ID，并将 override 固定为
   `repository@sha256:...`；
4. 执行 `docker compose up -d --no-deps <service>`；
5. 等待全部副本进入 `healthy`，没有 healthcheck 时等待 `running`，同时确认
   所有副本运行同一个已验证 Image ID；
6. 失败时恢复旧 override，并使用上一个已提交镜像回退。

程序不会改写原始 Compose、`.env`、`env_file` 或业务目录。
tag 只作为用户可读版本和拉取入口；成功部署、运行一致性检查和新版回滚均以
不可变 digest/Image ID 为准。

每个服务还提供以下生命周期操作：

- “重建当前版本”执行 `docker compose up -d --force-recreate --no-deps
  <service>`，重新读取 Compose、`.env` 和 `env_file`，但不切换版本或主动
  拉取镜像。
- “停止”保留容器；后续行为仍受服务的 Docker restart policy 影响。
- “下线”停止并删除目标服务容器，不删除镜像、命名卷、绑定目录或
  项目共享网络。可通过“重建当前版本”再次上线。
- “回滚上一版本”恢复该服务上一个成功部署的不可变镜像制品。

部署、回滚、重建、停止和下线都记录在“操作历史”中，可手动清除 30 天前已完成的
记录。同一 Compose 项目的操作互斥执行。重建后若新配置不健康，程序会记录
失败，但无法自动恢复被用户原地修改的旧 Compose 配置。

部署任务会持久化 `pulling`、`starting`、`checking` 和 `committing` 阶段。
如果管理程序在部署期间重启，启动时会检查实际容器：已验证候选镜像仍健康时
补记成功，否则主动恢复上一个已提交版本；无法恢复时会在操作历史中明确标记，
不会把中断任务静默当成成功。

## 0.2 状态兼容性

0.2 使用全新的 digest 制品模型，不兼容 0.1 的 SQLite 状态库，也不会尝试把
旧 tag 基线转换成可信 digest。0.1 到 0.2 这一次不能使用页面自更新：应先停止
管理服务，备份或删除数据目录下的 `deploy.db`、`deploy.db-wal` 和
`deploy.db-shm`，人工安装 0.2 二进制或 Release 包，再启动服务。后续 0.2.x
版本可以继续使用页面自更新。

重置管理状态不会删除或重启原有业务容器，但它们会显示为“未纳管”；第一次
通过 0.2 成功部署后建立新基线。

程序会拒绝直接打开带有旧 migration checksum 的数据库，以免静默生成不可靠的
回滚记录。操作历史无需保留时可以直接重建；需要留档时应先复制旧数据库。

每个服务卡片提供“查看日志”按钮，按需执行
`docker compose logs --no-color --tail <行数> <service>`。首次显示最近 50 行，
向上滚动到顶部后依次扩展至 100 行和最多 200 行。该功能不会持续跟踪或存储
容器日志。

## 系统空间与镜像清理

页面按需读取系统盘、内存、负载、运行时间以及 Docker 镜像、容器和构建缓存
空间；数据不写入数据库，也不进行后台持续采集。Docker 报告的可回收空间和
镜像标称大小都可能包含共享层，因此实际释放空间可能更少。

镜像列表仅包含当前受管 Compose 服务使用的 repository。只有同时满足以下条件的
镜像才能被选择清理：

- 没有被任何运行或停止状态的容器引用；
- 不是当前已提交的部署镜像；
- 不是可用于回退的上一版本镜像；
- 删除前重新读取 Docker 后仍属于受管 repository。

清理会按完整 Image ID 逐个执行，不使用全局 `docker image prune -a`，也不会
删除容器或卷。部署、回滚、自更新或另一项清理运行时，新的清理请求会被拒绝。
构建缓存可单独手动清理，范围固定为 7 天前未使用的缓存；它不会删除已标记镜像，
但之后的构建可能需要重新下载依赖。

## 构建与安装

需要 Rust 1.96 或更新版本、Docker 和 Docker Compose v2：

```bash
cargo build --release
```

GitHub Release 包含二进制、示例配置、安装脚本和 systemd unit。解压后执行：

```bash
sudo ./install.sh
sudoedit /etc/windplume-deploy/config.yaml
sudo systemctl start windplume-deploy
```

安装脚本创建 `windplume-deploy` 系统用户，将其加入 `docker` 组，安装文件并启用
systemd 服务，但不会在配置完成前启动服务。安装脚本还会启用独立的 systemd
更新助手。脚本会将 `/opt/windplume` 目录树的组
设为 `windplume-deploy`，授予组读取和目录进入权限，并对目录设置 setgid，使以后
创建的项目继续继承该组。Compose 文件、`.env` 和 `env_file` 因此可由服务读取。

## 程序自更新

页面从固定的公开 GitHub 仓库检查最新稳定 Release，仅在用户确认后更新。
更新前会校验 asset 名称、稳定 SemVer 和 archive SHA-256。普通服务进程只负责
下载并暂存文件，不会执行尚未授权的候选二进制。root 更新助手会先把候选文件复制
到 root 私有目录，使用安装时固定的 RSA 公钥验证独立 Release 签名，再校验候选
二进制报告的精确版本；未通过签名验证的文件不会以 root 身份执行。

替换 `/usr/local/bin/windplume-deploy` 和重启由 root 运行的独立 systemd unit
完成。新版本未能就绪时，助手会恢复旧二进制并重启。Deploy 页面会短暂
中断，但更新过程不执行任何业务 Compose 命令，不会停止业务容器。
已安装环境的更新控制文件固定位于 `/var/lib/windplume-deploy/update`，
不受业务状态的 `storage.data_dir` 自定义值影响。

首次启用自更新必须人工执行一次包含更新助手和 Release 公钥的新版 `install.sh`。
后续常规版本可从页面更新。自更新只替换主二进制；若 Release 说明要求更新
systemd 或 helper 协议，需再次人工执行安装脚本。

## 发布与签名

普通 push 和 Pull Request 只运行格式、Clippy、测试、ShellCheck 和依赖漏洞审计。
发布前先把 `Cargo.toml` 的版本更新为稳定的 `X.Y.Z`，然后推送完全匹配的
`vX.Y.Z` 标签。标签与 Cargo 版本不一致时，发布工作流会拒绝执行。

Release 签名使用 RSA 3072 位或更高密钥。公钥保存在
`deploy/release-signing-public.pem` 并随安装包部署到
`/etc/windplume-deploy/release-signing-public.pem`；私钥不得进入仓库，应配置在
GitHub `release` Environment 的 `RELEASE_SIGNING_PRIVATE_KEY_PEM` Secret 中。
发布工作流会先确认私钥与仓库公钥匹配，再对最终二进制生成
`windplume-deploy-X.Y.Z-linux-x86_64.sig`。

轮换密钥时，先通过人工安装包部署包含新公钥的版本，再用新私钥发布后续版本。
不要仅通过网页更新同时更换信任公钥。若更新失败且自动回退也失败，可从可信
Release 重新下载安装包、人工核对 SHA-256 和签名后再次运行 `install.sh`。

公开 Docker Hub 和公开 GHCR 的 tag 查询及镜像拉取通常不需要 Token。如果
Registry 需要认证，请使用服务用户登录；服务会从该用户的 Docker 配置中读取
内联登录凭据用于 tag 查询，镜像拉取也由 Docker 使用同一份凭据。GHCR 私有包
需要具有 `read:packages` 权限的 classic PAT，PAT 作为密码输入：

```bash
sudo -H -u windplume-deploy docker login ghcr.io -u YOUR_GITHUB_USERNAME
```

安装的 systemd unit 将 Docker 配置目录固定为
`/var/lib/windplume-deploy/.docker`。若 `docker login` 使用系统 credential helper
而非在 `config.json` 中保存内联 `auth`，当前版本的 tag 查询无法读取该凭据；可为
此服务使用不配置 credential helper 的独立 Docker 配置。

该服务用户能访问 Docker socket，主机权限实际上接近 root，只应把网页开放在
可信内网。CSRF/Origin 校验不能识别操作者身份。若监听 `0.0.0.0`，应同时配置
防火墙或带认证的反向代理，并让代理保留原始 `Host`。

## 运行和验证

前台运行：

```bash
RUST_LOG=windplume_deploy=debug cargo run -- --config ./config.yaml
```

查看服务：

```bash
systemctl status windplume-deploy
journalctl -u windplume-deploy -f
curl http://127.0.0.1:8180/health
```

项目检查：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
