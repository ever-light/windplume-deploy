# wind-plume-deploy

一个单二进制、内嵌中文网页的内网部署服务。它从个人账号的 GitHub Container Packages 读取私有镜像标签，通过固定的 Docker Compose 项目部署已配置服务，并用 SQLite 保存期望版本、任务、有限长度日志和回退结果。

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
`wind-plume-deploy-0.1.12-linux-x86_64.tar.gz`，同时提供 SHA256 校验文件。
压缩包包含可直接安装到宿主机的二进制、初始化脚本、示例配置和 systemd
unit，不需要项目源码或 Rust 构建环境。

## 首次安装

项目没有应用层的用户或“空间/工作区”，无需在网页中额外创建。下面的
`wind-plume-deploy` 是宿主机上的 Linux 服务账号，两个目录分别保存配置和
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
为避免使用空 Token 或错误的 Compose 路径，脚本只启用开机启动，不会立即启动
服务。

根据脚本最后输出的提示完成以下操作：

```bash
sudoedit /etc/wind-plume-deploy/config.yaml
sudoedit /etc/wind-plume-deploy/github-token
sudo -H -u wind-plume-deploy docker login ghcr.io
sudo -H -u wind-plume-deploy docker compose version
sudo -H -u wind-plume-deploy test -r /opt/wind-plume/compose.yaml
sudo systemctl start wind-plume-deploy
```

GitHub Token 只用于 Packages API，需能读取目标私有个人包；镜像拉取凭据由
Docker 保存在服务用户的 home 目录，本程序不读取密码。`services.id` 与
`compose_service` 必须唯一。若要让内网其他主机访问，应显式把监听地址改为受
防火墙保护的内网 IP 或 `0.0.0.0`。

## 运行与排障

前台运行：

```bash
RUST_LOG=wind_plume_deploy=debug cargo run -- --config ./config.yaml
```

查看服务和日志：

```bash
systemctl status wind-plume-deploy
journalctl -u wind-plume-deploy -f
curl http://127.0.0.1:8180/health
```

程序只原子改写数据目录下的 `compose.deploy.yaml`，不会修改基础 Compose、`.env` 或业务仓库。启动时会把遗留的 queued/running 任务标为 interrupted，并从 SQLite 重建 override。

升级时解压新的 Release 包，在新目录中先备份数据库，再重新执行安装脚本并
启动。已有配置、Token 和数据不会被覆盖：

```bash
sudo systemctl stop wind-plume-deploy
sudo cp /var/lib/wind-plume-deploy/deploy.db /var/lib/wind-plume-deploy/deploy.db.backup
sudo ./install.sh
sudo systemctl start wind-plume-deploy
```

接入生产 Compose 前，应在真实内网部署机用非生产服务各演练一次成功升级和失败回退。此步骤需要实际 GHCR 凭据、Docker socket 与目标镜像，不能由离线构建代替。

## 验证

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
