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
压缩包包含可直接安装到宿主机的二进制、示例配置和 systemd unit，不需要容器
运行环境。

## 首次安装

1. 创建专用系统用户和目录，并让它能够访问 Docker socket。注意：Docker socket 权限实际上接近宿主机 root 权限，只应授予可信专用用户。

   ```bash
   sudo useradd --system --home /var/lib/wind-plume-deploy --shell /usr/sbin/nologin wind-plume-deploy
   sudo install -d -o wind-plume-deploy -g wind-plume-deploy /etc/wind-plume-deploy /var/lib/wind-plume-deploy
   ```

2. 创建能读取目标私有个人包的 GitHub Token，将它单独保存且限制权限。Token 只调用 Packages API，不用于拉取镜像。

   ```bash
   sudo install -o wind-plume-deploy -g wind-plume-deploy -m 0600 /dev/null /etc/wind-plume-deploy/github-token
   sudoedit /etc/wind-plume-deploy/github-token
   ```

3. 以 systemd 最终使用的同一 Linux 用户登录 GHCR。Docker 会自行保存拉取凭据；本程序不执行登录，也不读取密码。

   ```bash
   sudo -u wind-plume-deploy docker login ghcr.io
   sudo -u wind-plume-deploy docker compose version
   sudo -u wind-plume-deploy test -r /opt/wind-plume/compose.yaml
   ```

4. 复制并修改配置。`services.id` 与 `compose_service` 必须唯一。若要让内网其他主机访问，显式把监听地址改为受防火墙保护的内网 IP 或 `0.0.0.0`。

   ```bash
   sudo install -o root -g wind-plume-deploy -m 0640 config.example.yaml /etc/wind-plume-deploy/config.yaml
   ```

5. 安装二进制与 systemd unit：

   ```bash
   sudo install -m 0755 target/release/wind-plume-deploy /usr/local/bin/wind-plume-deploy
   sudo install -m 0644 deploy/wind-plume-deploy.service /etc/systemd/system/wind-plume-deploy.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now wind-plume-deploy
   ```

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

升级时先备份 `/var/lib/wind-plume-deploy/deploy.db`，替换 release 二进制后重启：

```bash
sudo systemctl stop wind-plume-deploy
sudo cp /var/lib/wind-plume-deploy/deploy.db /var/lib/wind-plume-deploy/deploy.db.backup
sudo install -m 0755 target/release/wind-plume-deploy /usr/local/bin/wind-plume-deploy
sudo systemctl start wind-plume-deploy
```

接入生产 Compose 前，应在真实内网部署机用非生产服务各演练一次成功升级和失败回退。此步骤需要实际 GHCR 凭据、Docker socket 与目标镜像，不能由离线构建代替。

## 验证

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
