# Wind Plume Deploy 实施计划

## 1. 目标与边界

在本目录实现一个独立 Rust 内网部署服务 `wind-plume-deploy`，管理一个 Docker Compose 项目中的多个 GHCR 私有镜像服务。

第一版必须具备：

- 从 GitHub Packages API 读取私有个人账号下的容器包版本。
- 在内嵌网页中展示服务、可部署版本、实际运行状态和部署历史。
- 用户选择版本并确认后，生成 Compose override，执行 `docker compose up -d <service>`。
- 部署完成后检查容器运行状态或 Docker healthcheck。
- 部署失败时恢复旧 override 并尝试回退到旧版本。
- 使用 SQLite 保存期望版本、任务状态、部署结果和有限长度日志。
- 不提供用户体系、登录、权限、Webhook、自动部署、通知、多主机管理和任意命令执行。

首个服务适配当前已发布的镜像：

```text
ghcr.io/ever-light/wind-plume-cloud-identity-service
```

当前镜像同时存在 `0.1.<run number>` 和 commit SHA 标签；页面默认只展示符合 `^\d+\.\d+\.\d+$` 的标签。

## 2. 技术选型与项目结构

使用单个 Cargo binary crate，Rust edition 2024，最低 Rust 版本 1.96.1。

建议依赖：

- `axum`、`tokio`：HTTP 服务与异步任务。
- `reqwest`（rustls）：GitHub API。
- `serde`、`serde_json`、`serde_yaml`：配置、API 和 override YAML。
- `sqlx`（sqlite、migrate、runtime-tokio）：SQLite 存储与内嵌 migration。
- `tracing`、`tracing-subscriber`：结构化日志。
- `tower-http`：请求日志、超时、敏感请求头处理。
- `chrono`、`uuid`、`thiserror`、`anyhow`、`regex`、`semver`。

目录结构：

```text
wind-plume-deploy/
  Cargo.toml
  README.md
  config.example.yaml
  deploy/
    wind-plume-deploy.service
  examples/
    compose.yaml
  migrations/
    0001_init.sql
  src/
    main.rs
    config.rs
    error.rs
    state.rs
    github.rs
    compose.rs
    deployment.rs
    storage.rs
    web.rs
    assets/
      index.html
      app.js
      app.css
  tests/
    api.rs
    deployment.rs
```

网页资源通过 `include_str!` 编译进二进制，不使用 Node、前端构建工具、外部 CDN 或运行时静态文件目录。

## 3. 配置契约

程序接受 `--config <path>`，默认读取 `/etc/wind-plume-deploy/config.yaml`。

配置示例：

```yaml
server:
  listen: 127.0.0.1:8180

github:
  token_file: /etc/wind-plume-deploy/github-token
  api_base: https://api.github.com
  cache_seconds: 60

storage:
  data_dir: /var/lib/wind-plume-deploy
  history_limit: 500
  max_log_bytes: 65536

compose:
  project_name: wind-plume
  file: /opt/wind-plume/compose.yaml
  health_timeout_seconds: 120
  command_timeout_seconds: 600

services:
  - id: identity
    name: Identity Service
    github_owner: ever-light
    github_package: wind-plume-cloud-identity-service
    image: ghcr.io/ever-light/wind-plume-cloud-identity-service
    compose_service: identity-service
    tag_pattern: '^\d+\.\d+\.\d+$'
```

启动时必须校验：

- `services.id` 和 `compose_service` 唯一，且只允许字母、数字、短横线和下划线。
- 镜像名、GitHub owner/package、Compose 文件必须存在且不能为空。
- `tag_pattern` 能正常编译。
- GitHub Token 文件存在、非空；Token 内容不得进入错误消息或日志。
- Compose 基础文件存在；数据目录可创建和写入。
- timeout、缓存时间和日志上限均大于零。

默认仅监听 loopback。需要内网直接访问时由运维显式改为内网 IP 或 `0.0.0.0`。

GitHub Token 只负责调用 Packages API。镜像拉取使用运行该程序的 Linux 用户预先完成的 `docker login ghcr.io`，程序不执行登录、不保存 Docker 密码。

## 4. SQLite 模型

使用 `<data_dir>/deploy.db`，启动时执行内嵌 migration。SQLite 开启 WAL、foreign keys 和合理的 busy timeout。

`service_state`：

- `service_id TEXT PRIMARY KEY`
- `desired_version TEXT NOT NULL`
- `image TEXT NOT NULL`
- `updated_at TEXT NOT NULL`
- `last_deployment_id TEXT NULL`

`deployments`：

- `id TEXT PRIMARY KEY`，UUID。
- `service_id TEXT NOT NULL`
- `previous_version TEXT NULL`
- `target_version TEXT NOT NULL`
- `status TEXT NOT NULL`
- `started_at TEXT NOT NULL`
- `finished_at TEXT NULL`
- `command_log TEXT NOT NULL DEFAULT ''`
- `error_message TEXT NULL`
- `rollback_status TEXT NULL`

允许的部署状态固定为：`queued`、`running`、`succeeded`、`failed`、`interrupted`。回退状态固定为：`not_needed`、`succeeded`、`failed`、`unavailable`。

启动时将遗留的 `queued` 或 `running` 记录更新成 `interrupted`。`service_state` 只在部署和健康检查均成功后更新，是期望版本的持久化事实源。

每次写入新历史后删除超过 `history_limit` 的最旧记录。命令 stdout/stderr 合并保存，按 UTF-8 安全边界只保留最后 `max_log_bytes`。

## 5. GitHub Packages 客户端

调用 GitHub REST API 的“列出个人用户包版本”接口，包类型固定为 `container`，处理分页直到没有下一页。

请求头必须包含：

- `Authorization: Bearer <token>`
- GitHub 推荐的 API version header。
- 明确的 `User-Agent`。
- `Accept: application/vnd.github+json`。

将包版本中的 container tags 展平成页面版本条目，保留：

- tag/version
- package version id
- digest/name（API 有值时）
- created_at、updated_at

过滤掉不匹配服务 `tag_pattern` 的标签，按语义版本降序排序；无法解析为 semver 的匹配标签按发布时间降序排列。相同标签去重。

成功结果按服务内存缓存 `cache_seconds`。普通页面刷新可使用缓存；部署前必须绕过缓存重新查询，确认目标标签仍存在。GitHub 返回 401/403/404、限流或网络错误时向 API 返回可读但不泄漏 Token 的错误，不使用过期缓存冒充最新数据。

## 6. Compose override 与部署状态机

override 固定写入 `<data_dir>/compose.deploy.yaml`，内容由全部 `service_state` 加上当前候选版本生成：

```yaml
services:
  identity-service:
    image: ghcr.io/ever-light/wind-plume-cloud-identity-service:0.1.37
```

写入必须采用同目录临时文件、flush、sync、rename 的原子替换方式。不得修改业务 Compose、业务 `.env` 或业务 Git 仓库。

程序启动后根据 `service_state` 重新生成一次 override；没有任何成功状态时生成合法的空 `services: {}`。

部署请求流程：

1. 校验服务 ID、目标 tag 格式，并绕过缓存确认 tag 存在。
2. 全局部署互斥锁保证同一时间只有一个任务；忙时 API 返回 `409 deployment_in_progress`。
3. 创建 `queued` 记录，异步任务开始后改为 `running`。
4. 读取数据库中最后成功版本，生成包含目标版本的候选 override。
5. 在基础 Compose 文件目录执行参数数组，禁止经过 shell：

   ```text
   docker compose --project-name <project> -f <base> -f <override> up -d <compose_service>
   ```

6. 命令超过 `command_timeout_seconds` 时终止子进程并视为失败。
7. 命令成功后运行 `docker compose ... ps -q <service>`，再对所有容器执行 `docker inspect`：
   - 配置了 healthcheck 的容器必须全部变成 `healthy`。
   - 没有 healthcheck 的容器必须全部为 `running`。
   - `unhealthy`、退出、查不到容器或超过 `health_timeout_seconds` 都失败。
8. 成功时事务更新 `service_state` 和部署记录，再按数据库状态写出最终 override。
9. 失败时按旧 `service_state` 恢复 override，并再次执行相同 `up -d` 及健康检查：
   - 有旧版本或基础 Compose 可恢复时记录真实回退结果。
   - 没有可恢复目标时记录 `rollback_status=unavailable`。
   - 原部署无论回退成功与否都保持 `failed`。

进程捕获 stdout/stderr，但日志中不得出现 GitHub Authorization header、Token 或环境变量全集。

“实际运行版本”不只依赖数据库：使用 `docker compose ps -q` + `docker inspect .Config.Image` 获取实际镜像引用。页面同时显示 desired image、actual image 和 drift 状态。

## 7. HTTP API

统一 JSON 错误格式：

```json
{"code":"package_query_failed","message":"无法读取镜像版本"}
```

接口：

```text
GET  /health
GET  /api/services
GET  /api/services/{id}/packages?refresh=false
POST /api/services/{id}/deploy
GET  /api/deployments?limit=50
GET  /api/deployments/{id}
GET  /
GET  /assets/app.js
GET  /assets/app.css
```

`GET /api/services` 返回每个配置服务的：基本信息、desired version/image、actual image、容器状态、是否 drift、是否有全局部署任务运行。

`GET /api/services/{id}/packages` 返回过滤排序后的版本；`refresh=true` 绕过缓存。

部署请求：

```json
{"version":"0.1.37"}
```

接受后返回 HTTP 202：

```json
{"deployment_id":"<uuid>","status":"queued"}
```

未知服务返回 404，格式非法或不存在的版本返回 400/422，全局忙返回 409，上游 GitHub 故障返回 502，内部存储或 Docker 故障返回 500。命令执行结果记录在任务中，不把长日志直接放入部署 POST 响应。

服务不启用 CORS；POST 必须使用 `Content-Type: application/json`。不实现认证和 CSRF Token，这是明确的内网边界。

`/health` 检查配置已加载且 SQLite 可读写；GitHub 或 Docker 暂时不可用不令进程自身变成 unhealthy。

## 8. 内嵌网页行为

使用一页式原生 HTML/CSS/JavaScript：

- 顶部显示 Compose 项目和全局部署状态。
- 每个服务卡片显示 desired/actual 镜像、running/healthy/unhealthy/unknown 状态和 drift 提示。
- 点击服务后加载版本表，显示版本、发布时间、digest、当前版本标记。
- “部署”按钮打开确认对话框，明确展示服务、旧版本和目标版本。
- 提交后按 deployment id 每 2 秒轮询，直到终态；部署过程中禁用所有部署按钮。
- 历史表显示时间、服务、版本变化、状态、耗时和回退状态；可展开查看经过截断的日志。
- 页面刷新后从 API 恢复状态，不依赖浏览器本地存储。

视觉保持简单、响应式、无外部字体和图片，支持中文界面。

## 9. 运行与运维文件

README 必须覆盖：

1. 创建只具备读取包权限且能访问目标私有包的 GitHub Token，并以 `0600` 保存。
2. 使用最终运行 systemd 服务的同一 Linux 用户执行 `docker login ghcr.io`。
3. 确认该用户可运行 `docker compose version` 和读取基础 Compose。
4. 创建 `/etc/wind-plume-deploy`、`/var/lib/wind-plume-deploy` 并设置所有权。
5. 前台运行、systemd 安装、日志查看和升级步骤。
6. 说明 Docker socket 权限近似宿主机 root 权限，运行用户和网页监听范围必须受控。

systemd 示例使用专用用户 `wind-plume-deploy`，设置 `Restart=on-failure`、明确配置路径和工作目录；不使用过度限制且会阻止 Docker CLI/Socket 的沙箱选项。

`examples/compose.yaml` 展示业务服务如何在基础 Compose 中声明，镜像值可为任意安全默认值，因为最终由 override 覆盖。示例服务带 healthcheck。

## 10. 测试计划

单元测试：

- 完整配置、重复 ID、非法 regex、缺失 Token、非法 timeout。
- GitHub 分页响应解析、多 tag 展开、去重、过滤、semver 排序。
- override 对多个服务的稳定生成、YAML 转义和原子替换。
- 合法状态转换、日志 UTF-8 截断、历史清理。
- Docker inspect JSON 对 running/healthy/unhealthy/no-healthcheck 的解析。

API/集成测试：

- 服务列表和空状态。
- GitHub 成功、鉴权失败、限流、分页和刷新缓存。
- 未知服务、非法 tag、不存在 tag、正确的 202 响应。
- 同时提交两个部署时第二个返回 409。
- 使用可注入的 `CommandRunner` fake 覆盖部署成功、命令非零、命令超时、健康超时、回退成功和回退失败。
- 重启时将运行中任务标记为 interrupted，并从数据库重建 override。
- 静态资源和 JSON content type 正确，POST 拒绝非 JSON。

可选、默认忽略的 Docker 端到端测试使用临时 Compose 项目切换两个本地测试镜像，不依赖真实 GitHub。

交付前运行：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

## 11. 完成标准

- 一个 release 二进制即可运行，网页无需额外文件或前端服务。
- 能读取并展示 `ever-light/wind-plume-cloud-identity-service` 的可读版本标签。
- 能选择任意已存在版本部署，并正确显示任务进度和最终健康状态。
- 失败时恢复旧 override、尝试回退并保留可诊断日志。
- 实际运行镜像与期望版本不一致时页面明确显示 drift。
- 重启不丢失最后成功版本和历史，不会把半完成任务误报为成功。
- 代码中没有拼接 shell、任意命令 API、明文 Token、外部网页依赖或对业务仓库文件的修改。

## 12. 实施顺序

1. 创建 Cargo 工程、配置模型、错误模型和 tracing。
2. 加入 SQLite migration、repository 和启动恢复逻辑。
3. 实现 GitHub Packages 客户端、分页、过滤和缓存。
4. 实现 override 生成、命令执行抽象、容器状态检查。
5. 实现部署状态机、互斥、超时、回退和日志限制。
6. 实现 HTTP API 和统一错误响应。
7. 实现内嵌网页及轮询交互。
8. 添加示例配置、Compose、systemd 和 README。
9. 补齐单元/集成测试，运行 fmt、clippy、test。
10. 在真实内网部署机使用非生产测试服务完成一次升级和一次回退演练，再接入正式 Compose。
