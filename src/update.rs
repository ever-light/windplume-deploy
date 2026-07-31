use crate::error::AppError;
use chrono::Utc;
use flate2::read::GzDecoder;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command as StdCommand,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{fs, sync::RwLock};

const RELEASE_API: &str =
    "https://api.github.com/repos/ever-light/windplume-deploy/releases/latest";
const RELEASE_DOWNLOAD_PREFIX: &str =
    "https://github.com/ever-light/windplume-deploy/releases/download/";
const RELEASE_PAGE_PREFIX: &str = "https://github.com/ever-light/windplume-deploy/releases/tag/";
const INSTALLED_UPDATE_ROOT: &str = "/var/lib/windplume-deploy";
const INSTALLED_UPDATE_HELPER: &str = "/usr/local/libexec/windplume-deploy-update";
const INSTALLED_UPDATE_PATH_UNIT: &str = "/etc/systemd/system/windplume-deploy-update.path";
const INSTALLED_UPDATE_PUBLIC_KEY: &str = "/etc/windplume-deploy/release-signing-public.pem";
const INSTALLED_UPDATE_STATUS: &str = "/var/lib/windplume-deploy/update-status/status.json";
const UPDATE_PROTOCOL_VERSION: &str = "3";
const MAX_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;
const RELEASE_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

pub const BUILD_VERSION: &str = match option_env!("WINDPLUME_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
pub const SELF_UPDATE_SUPPORTED: bool = cfg!(all(target_os = "linux", target_arch = "x86_64"));

#[derive(Clone, Debug, Serialize)]
pub struct ReleaseInfo {
    pub version: String,
    pub html_url: String,
    pub published_at: Option<String>,
    #[serde(skip)]
    archive_url: String,
    #[serde(skip)]
    checksum_url: String,
    #[serde(skip)]
    signature_url: String,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    published_at: Option<String>,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateStatus {
    pub state: String,
    pub target_version: Option<String>,
    pub message: String,
    pub updated_at: String,
}

impl UpdateStatus {
    fn idle() -> Self {
        Self {
            state: "idle".into(),
            target_version: None,
            message: "尚未执行更新".into(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }
}

type ReleaseCache = Arc<RwLock<Option<(Instant, ReleaseInfo)>>>;

#[derive(Clone)]
pub struct UpdateManager {
    client: reqwest::Client,
    data_dir: PathBuf,
    cache: ReleaseCache,
    status: Arc<RwLock<UpdateStatus>>,
    active: Arc<AtomicBool>,
    supported: bool,
}

impl UpdateManager {
    pub fn runtime_root(fallback: &Path) -> PathBuf {
        if update_helper_installed() {
            INSTALLED_UPDATE_ROOT.into()
        } else {
            fallback.into()
        }
    }

    pub fn new(data_dir: PathBuf) -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .user_agent(format!("windplume-deploy/{BUILD_VERSION}"))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| AppError::Internal(error.to_string()))?;
        Ok(Self {
            client,
            data_dir,
            cache: Default::default(),
            status: Arc::new(RwLock::new(UpdateStatus::idle())),
            active: Arc::new(AtomicBool::new(false)),
            supported: SELF_UPDATE_SUPPORTED && update_helper_installed(),
        })
    }

    pub fn self_update_supported(&self) -> bool {
        self.supported
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst) || self.update_dir().join("request").is_file()
    }

    pub fn begin(&self) -> bool {
        if self.update_dir().join("request").is_file() {
            return false;
        }
        self.active
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn cancel(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    pub async fn latest(&self, refresh: bool) -> Result<ReleaseInfo, AppError> {
        if !refresh
            && let Some((at, release)) = self.cache.read().await.as_ref()
            && at.elapsed() < RELEASE_CACHE_TTL
        {
            return Ok(release.clone());
        }
        let response = self
            .client
            .get(RELEASE_API)
            .send()
            .await
            .map_err(|error| AppError::Update(format!("GitHub Release 查询失败: {error}")))?;
        if !response.status().is_success() {
            return Err(AppError::Update(format!(
                "GitHub Release 查询失败 ({})",
                response.status()
            )));
        }
        let release: GitHubRelease = response
            .json()
            .await
            .map_err(|error| AppError::Update(format!("GitHub Release 响应无效: {error}")))?;
        let release = parse_release(release)?;
        *self.cache.write().await = Some((Instant::now(), release.clone()));
        Ok(release)
    }

    pub async fn status(&self) -> UpdateStatus {
        let path = self.status_file();
        if let Ok(bytes) = fs::read(path).await
            && let Ok(status) = serde_json::from_slice(&bytes)
        {
            return status;
        }
        self.status.read().await.clone()
    }

    pub async fn prepare_and_trigger(&self, release: ReleaseInfo) {
        let target = release.version.clone();
        let result = self.prepare(&release).await;
        if let Err(error) = result {
            let status = UpdateStatus {
                state: "failed".into(),
                target_version: Some(target),
                message: error.to_string(),
                updated_at: Utc::now().to_rfc3339(),
            };
            *self.status.write().await = status;
            self.cancel();
            return;
        }

        // A working path unit restarts this process. If it does not respond, release
        // maintenance mode so the running version remains usable.
        for _ in 0..90 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let status = self.status().await;
            if matches!(
                status.state.as_str(),
                "failed" | "rolled_back" | "succeeded"
            ) {
                *self.status.write().await = status;
                self.cancel();
                return;
            }
        }
        if self.is_active() {
            let _ = fs::remove_file(self.update_dir().join("request")).await;
            let status = UpdateStatus {
                state: "failed".into(),
                target_version: Some(target),
                message: "systemd 更新助手未响应，已取消维护状态".into(),
                updated_at: Utc::now().to_rfc3339(),
            };
            *self.status.write().await = status;
            self.cancel();
        }
    }

    async fn prepare(&self, release: &ReleaseInfo) -> Result<(), AppError> {
        self.set_status("downloading", Some(&release.version), "正在下载 Release")
            .await?;
        let (archive, checksum, signature) = tokio::try_join!(
            self.download(&release.archive_url),
            self.download(&release.checksum_url),
            self.download(&release.signature_url)
        )?;
        let expected_name = format!("windplume-deploy-{}-linux-x86_64.tar.gz", release.version);
        let expected_sha = parse_checksum(&checksum, &expected_name)?;
        let archive_sha = format!("{:x}", Sha256::digest(&archive));
        if archive_sha != expected_sha {
            return Err(AppError::Update("Release SHA-256 校验失败".into()));
        }

        self.set_status("verifying", Some(&release.version), "正在校验候选二进制")
            .await?;
        let version = release.version.clone();
        let candidate = tokio::task::spawn_blocking(move || extract_binary(&archive, &version))
            .await
            .map_err(|error| AppError::Update(error.to_string()))??;
        let update_dir = self.update_dir();
        self.set_status(
            "ready",
            Some(&release.version),
            "校验完成，等待 systemd 更新助手",
        )
        .await?;
        stage_update(&update_dir, &candidate, &signature, &release.version).await?;
        Ok(())
    }

    async fn download(&self, url: &str) -> Result<Vec<u8>, AppError> {
        if !url.starts_with(RELEASE_DOWNLOAD_PREFIX) {
            return Err(AppError::Update("Release 资产地址不受信任".into()));
        }
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| AppError::Update(format!("Release 下载失败: {error}")))?;
        if !response.status().is_success() {
            return Err(AppError::Update(format!(
                "Release 下载失败 ({})",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_ARCHIVE_BYTES as u64)
        {
            return Err(AppError::Update("Release 资产超过 100 MiB 限制".into()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| AppError::Update(format!("Release 下载失败: {error}")))?;
        if bytes.len() > MAX_ARCHIVE_BYTES {
            return Err(AppError::Update("Release 资产超过 100 MiB 限制".into()));
        }
        Ok(bytes.to_vec())
    }

    async fn set_status(
        &self,
        state: &str,
        target: Option<&str>,
        message: &str,
    ) -> Result<(), AppError> {
        let status = UpdateStatus {
            state: state.into(),
            target_version: target.map(str::to_owned),
            message: message.into(),
            updated_at: Utc::now().to_rfc3339(),
        };
        *self.status.write().await = status;
        Ok(())
    }

    fn update_dir(&self) -> PathBuf {
        self.data_dir.join("update")
    }

    fn status_file(&self) -> PathBuf {
        if self.data_dir == Path::new(INSTALLED_UPDATE_ROOT) {
            PathBuf::from(INSTALLED_UPDATE_STATUS)
        } else {
            self.update_dir().join("status.json")
        }
    }
}

fn update_helper_installed() -> bool {
    Path::new(INSTALLED_UPDATE_HELPER).is_file()
        && Path::new(INSTALLED_UPDATE_PATH_UNIT).is_file()
        && Path::new(INSTALLED_UPDATE_PUBLIC_KEY).is_file()
        && helper_protocol_matches(Path::new(INSTALLED_UPDATE_HELPER))
}

fn helper_protocol_matches(helper: &Path) -> bool {
    StdCommand::new(helper)
        .arg("--protocol-version")
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == UPDATE_PROTOCOL_VERSION
        })
}

fn parse_release(release: GitHubRelease) -> Result<ReleaseInfo, AppError> {
    if release.draft || release.prerelease {
        return Err(AppError::Update("最新 Release 不是稳定版".into()));
    }
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let parsed = Version::parse(version)
        .map_err(|_| AppError::Update(format!("Release 版本号无效: {}", release.tag_name)))?;
    if !parsed.pre.is_empty() || !parsed.build.is_empty() {
        return Err(AppError::Update("Release 版本必须是稳定的 X.Y.Z".into()));
    }
    if release.html_url != format!("{RELEASE_PAGE_PREFIX}v{version}") {
        return Err(AppError::Update("Release 页面地址不受信任".into()));
    }
    let archive_name = format!("windplume-deploy-{version}-linux-x86_64.tar.gz");
    let checksum_name = format!("{archive_name}.sha256");
    let signature_name = format!("windplume-deploy-{version}-linux-x86_64.sig");
    let archive_url = release
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| AppError::Update(format!("Release 缺少资产 {archive_name}")))?;
    let checksum_url = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| AppError::Update(format!("Release 缺少资产 {checksum_name}")))?;
    let signature_url = release
        .assets
        .iter()
        .find(|asset| asset.name == signature_name)
        .map(|asset| asset.browser_download_url.clone())
        .ok_or_else(|| AppError::Update(format!("Release 缺少资产 {signature_name}")))?;
    if archive_url != format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/{archive_name}")
        || checksum_url != format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/{checksum_name}")
        || signature_url != format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/{signature_name}")
    {
        return Err(AppError::Update("Release 资产地址不受信任".into()));
    }
    Ok(ReleaseInfo {
        version: version.into(),
        html_url: release.html_url,
        published_at: release.published_at,
        archive_url,
        checksum_url,
        signature_url,
    })
}

fn parse_checksum(bytes: &[u8], expected_name: &str) -> Result<String, AppError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AppError::Update("SHA-256 文件不是 UTF-8".into()))?;
    let mut fields = text.split_whitespace();
    let checksum = fields
        .next()
        .filter(|value| value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit()))
        .ok_or_else(|| AppError::Update("SHA-256 文件无效".into()))?;
    let filename = fields
        .next()
        .map(|value| value.trim_start_matches('*'))
        .ok_or_else(|| AppError::Update("SHA-256 文件缺少文件名".into()))?;
    if filename != expected_name {
        return Err(AppError::Update("SHA-256 文件名与 Release 不匹配".into()));
    }
    Ok(checksum.to_ascii_lowercase())
}

fn extract_binary(archive: &[u8], version: &str) -> Result<Vec<u8>, AppError> {
    let expected = format!("windplume-deploy-{version}-linux-x86_64/windplume-deploy");
    let decoder = GzDecoder::new(archive);
    let mut archive = tar::Archive::new(decoder);
    for entry in archive
        .entries()
        .map_err(|error| AppError::Update(format!("Release 压缩包无效: {error}")))?
    {
        let entry =
            entry.map_err(|error| AppError::Update(format!("Release 压缩包无效: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| AppError::Update(format!("Release 路径无效: {error}")))?;
        if path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(AppError::Update("Release 包含不安全路径".into()));
        }
        if path == Path::new(&expected) {
            let mut bytes = Vec::new();
            entry
                .take(MAX_ARCHIVE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| AppError::Update(format!("无法解压候选二进制: {error}")))?;
            if bytes.is_empty() || bytes.len() > MAX_ARCHIVE_BYTES {
                return Err(AppError::Update("候选二进制大小无效".into()));
            }
            return Ok(bytes);
        }
    }
    Err(AppError::Update(
        "Release 压缩包缺少 windplume-deploy 二进制".into(),
    ))
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("路径缺少父目录"))?;
    fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".update-{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, bytes).await?;
    fs::rename(temporary, path).await
}

async fn stage_update(
    update_dir: &Path,
    candidate: &[u8],
    signature: &[u8],
    version: &str,
) -> std::io::Result<()> {
    fs::create_dir_all(update_dir).await?;
    match fs::remove_file(update_dir.join("request")).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match fs::remove_file(update_dir.join("candidate.sha256")).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let candidate_path = update_dir.join("candidate");
    write_atomic(&candidate_path, candidate).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&candidate_path, std::fs::Permissions::from_mode(0o755)).await?;
    }
    write_atomic(&update_dir.join("candidate.sig"), signature).await?;
    write_atomic(&update_dir.join("request"), version.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Cursor;

    fn github_release(version: &str) -> GitHubRelease {
        let archive = format!("windplume-deploy-{version}-linux-x86_64.tar.gz");
        GitHubRelease {
            tag_name: format!("v{version}"),
            html_url: format!(
                "https://github.com/ever-light/windplume-deploy/releases/tag/v{version}"
            ),
            published_at: Some("2026-01-01T00:00:00Z".into()),
            draft: false,
            prerelease: false,
            assets: vec![
                GitHubAsset {
                    name: archive.clone(),
                    browser_download_url: format!("{RELEASE_DOWNLOAD_PREFIX}v{version}/{archive}"),
                },
                GitHubAsset {
                    name: format!("{archive}.sha256"),
                    browser_download_url: format!(
                        "{RELEASE_DOWNLOAD_PREFIX}v{version}/{archive}.sha256"
                    ),
                },
                GitHubAsset {
                    name: format!("windplume-deploy-{version}-linux-x86_64.sig"),
                    browser_download_url: format!(
                        "{RELEASE_DOWNLOAD_PREFIX}v{version}/windplume-deploy-{version}-linux-x86_64.sig"
                    ),
                },
            ],
        }
    }

    #[test]
    fn selects_exact_stable_release_assets() {
        let release = parse_release(github_release("0.1.42")).unwrap();
        assert_eq!(release.version, "0.1.42");
        assert!(release.archive_url.ends_with("0.1.42-linux-x86_64.tar.gz"));
        assert!(release.signature_url.ends_with("0.1.42-linux-x86_64.sig"));
        let mut missing_signature = github_release("0.1.43");
        missing_signature.assets.pop();
        assert!(parse_release(missing_signature).is_err());
        let mut prerelease = github_release("0.2.0");
        prerelease.prerelease = true;
        assert!(parse_release(prerelease).is_err());
        assert!(parse_release(github_release("0.2.0+build.1")).is_err());
    }

    #[test]
    fn validates_checksum_filename_and_digest() {
        let name = "windplume-deploy-0.1.42-linux-x86_64.tar.gz";
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(format!("{digest}  {name}\n").as_bytes(), name).unwrap(),
            digest
        );
        assert!(parse_checksum(format!("{digest}  other.tar.gz\n").as_bytes(), name).is_err());
    }

    #[test]
    fn extracts_only_expected_release_binary() {
        let version = "0.1.42";
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let bytes = b"test binary";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("windplume-deploy-{version}-linux-x86_64/windplume-deploy"),
                Cursor::new(bytes),
            )
            .unwrap();
        let encoder = archive.into_inner().unwrap();
        let compressed = encoder.finish().unwrap();

        assert_eq!(extract_binary(&compressed, version).unwrap(), bytes);
        assert!(extract_binary(&compressed, "0.1.43").is_err());
    }

    #[tokio::test]
    async fn staged_request_is_a_cross_restart_maintenance_lock() {
        let dir = tempfile::tempdir().unwrap();
        let manager = UpdateManager::new(dir.path().into()).unwrap();
        tokio::fs::create_dir_all(dir.path().join("update"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("update/request"), "0.1.42")
            .await
            .unwrap();
        assert!(manager.is_active());
        assert!(!manager.begin());
        tokio::fs::remove_file(dir.path().join("update/request"))
            .await
            .unwrap();
        assert!(!manager.is_active());
    }

    #[tokio::test]
    async fn stages_signature_and_creates_request_last() {
        let dir = tempfile::tempdir().unwrap();
        let update_dir = dir.path().join("update");
        tokio::fs::create_dir_all(&update_dir).await.unwrap();
        tokio::fs::write(update_dir.join("request"), "stale")
            .await
            .unwrap();
        tokio::fs::write(update_dir.join("candidate.sha256"), "legacy")
            .await
            .unwrap();
        stage_update(&update_dir, b"candidate", b"signature", "1.2.3")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read(update_dir.join("candidate.sig"))
                .await
                .unwrap(),
            b"signature"
        );
        assert!(!update_dir.join("candidate.sha256").exists());
        assert_eq!(
            tokio::fs::read_to_string(update_dir.join("request"))
                .await
                .unwrap(),
            "1.2.3"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_old_update_helper_protocol() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("helper");
        std::fs::write(&helper, "#!/bin/sh\necho 2\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!helper_protocol_matches(&helper));
        std::fs::write(&helper, "#!/bin/sh\necho 3\n").unwrap();
        assert!(helper_protocol_matches(&helper));
    }
}
