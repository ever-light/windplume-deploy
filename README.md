# windplume-deploy

一个单二进制、内嵌中文网页的内网部署服务。它可以管理多个相互独立的 Docker
Compose 项目，从公开 OCI Registry、GitHub Packages 或 Docker Hub 读取镜像
标签，并用 SQLite 保存每个项目的期望版本、任务、有限长度日志和回退结果。

服务不包含登录、权限、Webhook、自动部署、任意命令接口或 Docker 凭据管理。默认只监听 `127.0.0.1`。

## 构建

需要 Rust 1.96 或更新版本和 Docker Compose v2：

```bash
cargo build --release
```

网页资源已编译进二进制，不需要 Node、CDN 或静态文件目录。

每次代码推送到 `master` 分支时，GitHub Actions 会自动构建静态链接的 Linux
x86_64 二进制，并创建 GitHub Release。版本格式为
`0.1.<GitHub Actions run number>`，其中 `0.1` 来自 `Cargo.toml` 当前包版本的
主、次版本。

Release 包名类似
`windplume-deploy-0.1.12-linux-x86_64.tar.gz`，同时提供 SHA256 校验文件。
压缩包包含可直接安装到宿主机的二进制、初始化脚本、示例配置和 systemd
unit，不需要项目源码或 Rust 构建环境。

## 首次安装

项目没有应用层的用户或“空间/工作区”，无需在网页中额外创建。下面的
`windplume-deploy` 是宿主机上的 Linux 服务账号，两个目录分别保存配置和
SQLite 数据。若使用仓库提供的 systemd unit，需要在首次安装时创建一次；后续
升级无需重复创建。专用用户不是程序的硬性要求，但可以隔离服务文件和 GHCR
凭据，避免进程直接以 root 身份运行，因此初始化脚本默认创建并使用它。由于该
用户可以访问 Docker socket，其主机权限实际上仍接近 root，只应将网页开放到
可信内网。

从 GitHub Release 下载并校验压缩包，复制到部署机后解压。进入解压后的目录，
直接执行：

```bash
sudo ./install.sh
```

脚本会创建专用系统用户和目录、授予 Docker socket 访问权限，并安装二进制、
示例配置和 systemd unit。它可以重复执行，且不会覆盖已有的 `config.yaml`。
为避免使用错误的 Compose 路径，脚本只启用开机启动，不会立即启动服务。

根据脚本最后输出的提示完成以下操作：

```bash
sudoedit /etc/windplume-deploy/config.yaml
sudo -H -u windplume-deploy docker login ghcr.io
sudo -H -u windplume-deploy docker compose version
sudo systemctl start windplume-deploy
```

公开 GHCR/OCI 和 Docker Hub 仓库无需为标签查询配置 Token。使用
`github_packages` 来源读取私有包时，才需要编辑
`/etc/windplume-deploy/github-token`。私有镜像的拉取凭据由 Docker 保存在服务
用户的 home 目录，本程序不读取 Docker 密码；应使用同一服务用户分别登录所需
Registry。若要让内网其他主机访问，应显式把监听地址改为受防火墙保护的内网 IP
或 `0.0.0.0`。

## 配置多个 Compose 项目

顶层 `projects` 是相互独立的 Compose 项目列表。每个项目拥有自己的 Compose
文件、服务、override 和部署锁；不同项目可以同时部署，同一项目内的部署串行
执行。一个项目的 `compose.files` 也可以包含多个按顺序叠加的基础 Compose
文件。

每个服务通过 `version_source.type` 选择标签来源：

- `oci_registry`：公开的 OCI Registry，例如 GHCR；支持匿名 Bearer challenge。
- `github_packages`：GitHub Packages REST API，支持用户或组织包及私有包。
- `docker_hub`：Docker Hub 公共仓库，通过其 OCI Distribution 接口读取标签。

配置结构示例：

```yaml
projects:
  - id: windplume
    name: Windplume
    compose:
      project_name: windplume
      files:
        - /opt/windplume/compose.yaml
      health_timeout_seconds: 120
      command_timeout_seconds: 600
    services:
      - id: frontend
        name: Frontend
        image: ghcr.io/ever-light/windplume-frontend
        compose_service: frontend
        tag_pattern: '^\d+\.\d+\.\d+$'
        version_source:
          type: oci_registry
          registry: ghcr.io
          repository: ever-light/windplume-frontend
```

完整的多项目、公开 GHCR 和 Docker Hub 示例见 `config.example.yaml`。项目 ID
全局唯一；服务 ID 和 `compose_service` 只需在所属项目内唯一。

## 安装位置

安装脚本只使用以下系统位置：

| 位置 | 内容 | 删除服务时是否保留 |
| --- | --- | --- |
| `/usr/local/bin/windplume-deploy` | 可执行文件 | 可直接删除 |
| `/etc/windplume-deploy/` | 配置和 GitHub Token | 建议确认后删除 |
| `/var/lib/windplume-deploy/` | SQLite、各项目的 Compose override 和 Docker 登录凭据 | 建议备份后删除 |
| `/etc/systemd/system/windplume-deploy.service` | systemd unit | 停用服务后删除 |

此外会创建同名的 `windplume-deploy` 系统用户和用户组，并由
`systemctl enable` 创建标准的 systemd 启用链接。配置、程序和运行状态分开放置
是为了让权限、备份和升级边界保持清楚；不建议把 Token、数据库和可执行文件
混放在同一个 `/opt` 目录中。

## 运行与排障

前台运行：

```bash
RUST_LOG=windplume_deploy=debug cargo run -- --config ./config.yaml
```

查看服务和日志：

```bash
systemctl status windplume-deploy
journalctl -u windplume-deploy -f
curl http://127.0.0.1:8180/health
```

程序只原子改写数据目录下的
`projects/<project-id>/compose.deploy.yaml`，不会修改任何基础 Compose、`.env`
或业务仓库。启动时会把遗留的 queued/running 任务标为 interrupted，并分别从
SQLite 重建每个项目的 override。

升级时解压新的 Release 包，在新目录中先备份数据库，再重新执行安装脚本并
启动。已有配置、Token 和数据不会被覆盖：

```bash
sudo systemctl stop windplume-deploy
sudo cp /var/lib/windplume-deploy/deploy.db /var/lib/windplume-deploy/deploy.db.backup
sudo ./install.sh
sudo systemctl start windplume-deploy
```

接入生产 Compose 前，应在真实内网部署机对每个 Compose 项目用非生产服务各
演练一次成功升级和失败回退。此步骤需要 Docker socket 与目标镜像，私有镜像还
需要相应 Registry 的拉取凭据，不能由离线构建代替。

## 验证

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
