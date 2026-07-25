#!/usr/bin/env bash
set -Eeuo pipefail

service_name="windplume-deploy"
service_user="windplume-deploy"
service_group="windplume-deploy"
config_dir="/etc/windplume-deploy"
data_dir="/var/lib/windplume-deploy"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

if [[ "${EUID}" -ne 0 ]]; then
  echo "请使用 root 运行：sudo ./install.sh" >&2
  exit 1
fi

binary_source="${script_dir}/windplume-deploy"
config_source="${script_dir}/config.example.yaml"
unit_source="${script_dir}/windplume-deploy.service"
if [[ ! -f "${unit_source}" ]]; then
  unit_source="${script_dir}/deploy/windplume-deploy.service"
fi

for source_file in "${binary_source}" "${config_source}" "${unit_source}"; do
  if [[ ! -f "${source_file}" ]]; then
    echo "安装包不完整，缺少文件：${source_file}" >&2
    exit 1
  fi
done

for command_name in docker getent groupadd id install systemctl useradd usermod; do
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    echo "缺少必要命令：${command_name}" >&2
    exit 1
  fi
done

if ! getent group docker >/dev/null; then
  echo "未找到 docker 用户组，请先安装并启动 Docker。" >&2
  exit 1
fi

if ! docker compose version >/dev/null 2>&1; then
  echo "Docker Compose v2 不可用，请先确保 docker compose version 可正常执行。" >&2
  exit 1
fi

if ! getent group "${service_group}" >/dev/null; then
  groupadd --system "${service_group}"
fi

if ! id -u "${service_user}" >/dev/null 2>&1; then
  useradd \
    --system \
    --gid "${service_group}" \
    --home-dir "${data_dir}" \
    --shell /usr/sbin/nologin \
    "${service_user}"
fi

usermod --append --groups docker "${service_user}"

install -d -o root -g "${service_group}" -m 0750 "${config_dir}"
install -d -o "${service_user}" -g "${service_group}" -m 0750 "${data_dir}"
install -o root -g root -m 0755 "${binary_source}" "/usr/local/bin/${service_name}"
install -o root -g root -m 0644 "${unit_source}" "/etc/systemd/system/${service_name}.service"

if [[ ! -e "${config_dir}/config.yaml" ]]; then
  install -o root -g "${service_group}" -m 0640 \
    "${config_source}" "${config_dir}/config.yaml"
  config_created=true
else
  config_created=false
fi

systemctl daemon-reload
systemctl enable "${service_name}.service"

echo
echo "安装文件已就绪，服务尚未启动。"
if [[ "${config_created}" == true ]]; then
  echo "1. 修改配置：sudoedit ${config_dir}/config.yaml"
else
  echo "1. 已保留现有配置：${config_dir}/config.yaml"
fi
echo "2. 确认 Compose 文件及其 .env/env_file 可由 ${service_user} 用户读取"
echo "3. 公开 Docker Hub/GHCR 无需登录；需要认证时使用：sudo -H -u ${service_user} docker login ghcr.io"
echo "4. 启动服务：sudo systemctl start ${service_name}"
echo "5. 查看状态：systemctl status ${service_name}"
