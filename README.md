# windplume-deploy

一个单二进制、内嵌中文网页的内网 Docker Compose 版本部署服务。只需提供
Compose 文件路径，程序会调用 `docker compose config` 自动识别项目名、服务和
镜像，从公开 Docker Hub、GHCR 或标准 OCI Registry 读取 tag，并支持逐服务选择
版本、显式拉取、健康检查和失败回退。

服务不包含应用登录、权限、Webhook、无人值守自动部署或 Docker 凭据管理，默认
只监听 `127.0.0.1:8180`。

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

`storage.data_dir` 默认是 `/var/lib/windplume-deploy`。监听地址、缓存和存储限制
仅在需要覆盖时配置：

```yaml
server:
  listen: 127.0.0.1:8180

registries:
  cache_seconds: 60

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

1. 在数据目录生成只覆盖镜像 tag 的 `compose.deploy.yaml`；
2. 执行 `docker compose pull <service>`；
3. 执行 `docker compose up -d <service>`；
4. 等待容器进入 `healthy`，没有 healthcheck 时等待 `running`；
5. 失败时恢复旧 override，并使用本地旧镜像回退。

程序不会改写原始 Compose、`.env`、`env_file` 或业务目录。

每个服务卡片提供“查看日志”按钮，按需执行
`docker compose logs --no-color --tail 200 <service>` 并显示最近 200 行。
该功能不会持续跟踪或存储容器日志。

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
systemd 服务，但不会在配置完成前启动服务。脚本会将 `/opt/windplume` 目录树的组
设为 `windplume-deploy`，授予组读取和目录进入权限，并对目录设置 setgid，使以后
创建的项目继续继承该组。Compose 文件、`.env` 和 `env_file` 因此可由服务读取。

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
可信内网。若监听 `0.0.0.0`，应同时配置防火墙或带认证的反向代理。

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
