#!/usr/bin/env bash
set -Eeuo pipefail

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
helper="${repo_dir}/deploy/windplume-deploy-update"
test_dir="$(mktemp -d)"
trap 'rm -rf -- "${test_dir}"' EXIT

fail() {
  echo "update helper test failed: $*" >&2
  exit 1
}

make_binary() {
  local path="$1"
  local version="$2"
  install -d "$(dirname -- "${path}")"
  printf '#!/usr/bin/env bash\nprintf '\''windplume-deploy %s\\n'\''\n' "${version}" > "${path}"
  chmod 0755 "${path}"
}

setup_case() {
  local root="$1"
  install -d "${root}/var/lib/windplume-deploy/update" \
    "${root}/var/lib/windplume-deploy/update-root" \
    "${root}/var/lib/windplume-deploy/update-status" \
    "${root}/etc/windplume-deploy" \
    "${root}/usr/local/bin" \
    "${root}/usr/local/libexec" \
    "${root}/mock-bin"
  chmod 0700 "${root}/var/lib/windplume-deploy/update-root"
  make_binary "${root}/usr/local/bin/windplume-deploy" "1.0.0"
  openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:3072 \
    -out "${root}/private.pem" >/dev/null 2>&1
  openssl pkey -in "${root}/private.pem" -pubout \
    -out "${root}/etc/windplume-deploy/release-signing-public.pem" >/dev/null 2>&1
  chmod 0644 "${root}/etc/windplume-deploy/release-signing-public.pem"
  printf '#!/usr/bin/env bash\nexit 0\n' > "${root}/mock-bin/systemctl"
  printf '#!/usr/bin/env bash\nexit 0\n' > "${root}/mock-bin/sleep"
  chmod 0755 "${root}/mock-bin/systemctl" "${root}/mock-bin/sleep"
}

stage_candidate() {
  local root="$1"
  local version="$2"
  local update_dir="${root}/var/lib/windplume-deploy/update"
  make_binary "${update_dir}/candidate" "${version}"
  sha256sum "${update_dir}/candidate" | awk '{print $1}' > "${update_dir}/candidate.sha256"
  openssl dgst -sha256 -sign "${root}/private.pem" \
    -out "${update_dir}/candidate.sig" "${update_dir}/candidate"
  printf '%s' "${version}" > "${update_dir}/request"
}

run_helper() {
  local root="$1"
  PATH="${root}/mock-bin:${PATH}" \
    WINDPLUME_UPDATE_TESTING=1 \
    WINDPLUME_UPDATE_TEST_ROOT="${root}" \
    WINDPLUME_UPDATE_SERVICE_GROUP=root \
    "${helper}"
}

[[ "$("${helper}" --protocol-version)" == "2" ]] || fail "protocol version"

success_root="${test_dir}/success"
setup_case "${success_root}"
stage_candidate "${success_root}" "1.1.0"
run_helper "${success_root}"
[[ "$("${success_root}/usr/local/bin/windplume-deploy" --version)" == "windplume-deploy 1.1.0" ]] || fail "valid signed update"
grep -Fq '"state":"succeeded"' "${success_root}/var/lib/windplume-deploy/update-status/status.json" || fail "success status"

signature_root="${test_dir}/bad-signature"
setup_case "${signature_root}"
stage_candidate "${signature_root}" "1.1.0"
printf 'forged' > "${signature_root}/var/lib/windplume-deploy/update/candidate.sig"
if run_helper "${signature_root}"; then
  fail "forged signature accepted"
fi
[[ "$("${signature_root}/usr/local/bin/windplume-deploy" --version)" == "windplume-deploy 1.0.0" ]] || fail "forged signature changed binary"

downgrade_root="${test_dir}/downgrade"
setup_case "${downgrade_root}"
stage_candidate "${downgrade_root}" "0.9.0"
if run_helper "${downgrade_root}"; then
  fail "downgrade accepted"
fi

version_root="${test_dir}/version-mismatch"
setup_case "${version_root}"
stage_candidate "${version_root}" "1.1.0"
make_binary "${version_root}/var/lib/windplume-deploy/update/candidate" "1.1.1"
sha256sum "${version_root}/var/lib/windplume-deploy/update/candidate" | awk '{print $1}' \
  > "${version_root}/var/lib/windplume-deploy/update/candidate.sha256"
openssl dgst -sha256 -sign "${version_root}/private.pem" \
  -out "${version_root}/var/lib/windplume-deploy/update/candidate.sig" \
  "${version_root}/var/lib/windplume-deploy/update/candidate"
if run_helper "${version_root}"; then
  fail "signed candidate with mismatched version accepted"
fi

oversize_root="${test_dir}/oversize"
setup_case "${oversize_root}"
stage_candidate "${oversize_root}" "1.1.0"
truncate -s 104857601 "${oversize_root}/var/lib/windplume-deploy/update/candidate"
if run_helper "${oversize_root}"; then
  fail "oversized candidate accepted"
fi

rollback_root="${test_dir}/rollback"
setup_case "${rollback_root}"
stage_candidate "${rollback_root}" "1.1.0"
# The generated mock must evaluate $1 when it runs, not while this test writes it.
# shellcheck disable=SC2016
printf '#!/usr/bin/env bash\nif [[ "$1" == "is-active" ]]; then exit 1; fi\nexit 0\n' \
  > "${rollback_root}/mock-bin/systemctl"
chmod 0755 "${rollback_root}/mock-bin/systemctl"
if run_helper "${rollback_root}"; then
  fail "failed restart reported success"
fi
[[ "$("${rollback_root}/usr/local/bin/windplume-deploy" --version)" == "windplume-deploy 1.0.0" ]] || fail "rollback did not restore old binary"
grep -Fq '"state":"rolled_back"' "${rollback_root}/var/lib/windplume-deploy/update-status/status.json" || fail "rollback status"

echo "update helper tests passed"
