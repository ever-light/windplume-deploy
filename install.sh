#!/usr/bin/env bash
set -Eeuo pipefail

service_name="windplume-deploy"
service_user="windplume-deploy"
service_group="windplume-deploy"
config_dir="/etc/windplume-deploy"
data_dir="/var/lib/windplume-deploy"
project_root="/opt/windplume"
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
update_path_source="${script_dir}/windplume-deploy-update.path"
update_unit_source="${script_dir}/windplume-deploy-update.service"
update_helper_source="${script_dir}/windplume-deploy-update"
public_key_source="${script_dir}/release-signing-public.pem"
if [[ ! -f "${update_path_source}" ]]; then
  update_path_source="${script_dir}/deploy/windplume-deploy-update.path"
  update_unit_source="${script_dir}/deploy/windplume-deploy-update.service"
  update_helper_source="${script_dir}/deploy/windplume-deploy-update"
  public_key_source="${script_dir}/deploy/release-signing-public.pem"
fi

for source_file in \
  "${binary_source}" \
  "${config_source}" \
  "${unit_source}" \
  "${update_path_source}" \
  "${update_unit_source}" \
  "${update_helper_source}" \
  "${public_key_source}"; do
  if [[ ! -f "${source_file}" ]]; then
    echo "安装包不完整，缺少文件：${source_file}" >&2
    exit 1
  fi
done

for command_name in chgrp chmod docker find getent groupadd id install openssl sed systemctl useradd usermod; do
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    echo "缺少必要命令：${command_name}" >&2
    exit 1
  fi
done

if ! openssl pkey -pubin -in "${public_key_source}" -noout >/dev/null 2>&1; then
  echo "Release 签名公钥不是有效的 PEM 公钥：${public_key_source}" >&2
  exit 1
fi
public_key_bits="$(openssl pkey -pubin -in "${public_key_source}" -text -noout 2>/dev/null | \
  sed -n 's/.*Public-Key: (\([0-9][0-9]*\) bit).*/\1/p')"
if [[ -z "${public_key_bits}" || "${public_key_bits}" -lt 3072 ]]; then
  echo "Release 签名公钥必须是 RSA 3072 位或更高：${public_key_source}" >&2
  exit 1
fi

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
install -d -o "${service_user}" -g "${service_group}" -m 0750 "${data_dir}/update"
install -d -o root -g root -m 0700 "${data_dir}/update-root"
install -d -o root -g "${service_group}" -m 0750 "${data_dir}/update-status"
install -d -o root -g root -m 0755 /usr/local/libexec

# Compose 项目默认部署在这里。服务需要遍历目录并读取 Compose、.env 和
# env_file；目录上的 setgid 可让之后创建的内容继续继承服务组。
if [[ -d "${project_root}" ]]; then
  chgrp -R "${service_group}" "${project_root}"
  chmod -R g+rX "${project_root}"
  find "${project_root}" -type d -exec chmod g+s {} +
else
  install -d -o root -g "${service_group}" -m 2750 "${project_root}"
fi

install -o root -g root -m 0755 "${binary_source}" "/usr/local/bin/${service_name}"
install -o root -g root -m 0644 "${unit_source}" "/etc/systemd/system/${service_name}.service"
install -o root -g root -m 0644 \
  "${update_path_source}" "/etc/systemd/system/${service_name}-update.path"
install -o root -g root -m 0644 \
  "${update_unit_source}" "/etc/systemd/system/${service_name}-update.service"
install -o root -g root -m 0755 \
  "${update_helper_source}" "/usr/local/libexec/${service_name}-update"
install -o root -g root -m 0644 \
  "${public_key_source}" "${config_dir}/release-signing-public.pem"

if [[ ! -e "${config_dir}/config.yaml" ]]; then
  install -o root -g "${service_group}" -m 0640 \
    "${config_source}" "${config_dir}/config.yaml"
  config_created=true
else
  config_created=false
fi

systemctl daemon-reload
systemctl enable "${service_name}.service"
systemctl enable --now "${service_name}-update.path"

echo
echo "安装文件已就绪，服务尚未启动。"
if [[ "${config_created}" == true ]]; then
  echo "1. 修改配置：sudoedit ${config_dir}/config.yaml"
else
  echo "1. 已保留现有配置：${config_dir}/config.yaml"
fi
echo "2. ${project_root} 已配置为可由 ${service_user} 用户读取"
echo "3. 公开 Docker Hub/GHCR 无需登录；需要认证时使用：sudo -H -u ${service_user} docker login ghcr.io"
echo "4. 启动服务：sudo systemctl start ${service_name}"
echo "5. 查看状态：systemctl status ${service_name}"
echo "6. 已启用 GitHub Release 手动自更新助手"
echo "7. 自更新协议已启用独立 Release 签名验证"
