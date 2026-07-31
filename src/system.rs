use crate::compose::{CommandRunner, repositories_match};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct SystemManager {
    runner: Arc<dyn CommandRunner>,
    cwd: PathBuf,
}

#[derive(Debug, Serialize)]
pub struct SystemOverview {
    pub system_disk: SpaceUsage,
    pub data_disk: SpaceUsage,
    pub memory: SpaceUsage,
    pub swap: SpaceUsage,
    pub load_average: [f64; 3],
    pub uptime_seconds: u64,
    pub docker: DockerUsage,
}

#[derive(Debug, Default, Serialize)]
pub struct SpaceUsage {
    pub mount_point: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
    pub used_percent: f64,
}

#[derive(Debug, Default, Serialize)]
pub struct DockerUsage {
    pub images: DockerResourceUsage,
    pub containers: DockerResourceUsage,
    pub local_volumes: DockerResourceUsage,
    pub build_cache: DockerResourceUsage,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DockerResourceUsage {
    pub total: u64,
    pub active: u64,
    pub size_bytes: u64,
    pub reclaimable_bytes: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ImageReference {
    pub project_id: String,
    pub service_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagedImage {
    pub id: String,
    pub repository: String,
    pub tag: String,
    pub digest: Option<String>,
    pub aliases: Vec<ImageAlias>,
    pub size_bytes: u64,
    pub created_at: String,
    pub containers: u64,
    pub services: Vec<ImageReference>,
    pub protected_reasons: Vec<String>,
    pub removable: bool,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ImageAlias {
    pub repository: String,
    pub tag: String,
    pub digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DockerDfRow {
    #[serde(rename = "Type")]
    kind: String,
    #[serde(rename = "TotalCount")]
    total: String,
    #[serde(rename = "Active")]
    active: String,
    #[serde(rename = "Size")]
    size: String,
    #[serde(rename = "Reclaimable")]
    reclaimable: String,
}

#[derive(Debug, Deserialize)]
struct DockerImageRow {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Repository")]
    repository: String,
    #[serde(rename = "Tag")]
    tag: String,
    #[serde(rename = "Digest")]
    digest: String,
    #[serde(rename = "Size")]
    size: String,
    #[serde(rename = "CreatedAt")]
    created_at: String,
}

impl SystemManager {
    pub fn new(cwd: PathBuf, runner: Arc<dyn CommandRunner>) -> Self {
        Self { runner, cwd }
    }

    pub async fn overview(&self) -> anyhow::Result<SystemOverview> {
        let system_disk = self.run_df(Path::new("/")).await?;
        let data_disk = self.run_df(&self.cwd).await?;
        let docker = self
            .run_docker(&["system", "df", "--format", "{{json .}}"])
            .await?;
        let (meminfo, loadavg, uptime) = tokio::try_join!(
            tokio::fs::read_to_string("/proc/meminfo"),
            tokio::fs::read_to_string("/proc/loadavg"),
            tokio::fs::read_to_string("/proc/uptime")
        )?;
        let (memory, swap) = parse_meminfo(&meminfo)?;
        Ok(SystemOverview {
            system_disk,
            data_disk,
            memory,
            swap,
            load_average: parse_loadavg(&loadavg)?,
            uptime_seconds: parse_uptime(&uptime)?,
            docker: parse_docker_df(&docker)?,
        })
    }

    pub async fn images(
        &self,
        services: &[(String, String, String)],
        protected: &BTreeMap<String, BTreeSet<String>>,
    ) -> anyhow::Result<Vec<ManagedImage>> {
        let output = self
            .run_docker(&[
                "image",
                "ls",
                "--digests",
                "--no-trunc",
                "--format",
                "{{json .}}",
            ])
            .await?;
        let container_images = self.container_image_counts().await?;
        parse_images(&output, services, protected, &container_images)
    }

    pub async fn remove_image(&self, image_id: &str) -> anyhow::Result<String> {
        self.run_docker(&["image", "rm", image_id]).await
    }

    pub async fn prune_build_cache(&self) -> anyhow::Result<String> {
        self.run_docker(&["builder", "prune", "--force", "--filter", "until=168h"])
            .await
    }

    async fn run_df(&self, path: &Path) -> anyhow::Result<SpaceUsage> {
        let args = vec![
            "-B1".into(),
            "--output=target,size,used,avail,pcent".into(),
            path.to_string_lossy().into_owned(),
        ];
        let output = self
            .runner
            .run("df", &args, &self.cwd, COMMAND_TIMEOUT)
            .await?;
        if !output.success {
            anyhow::bail!("df 执行失败: {}", output.log.trim());
        }
        parse_df(&output.log)
    }

    async fn run_docker(&self, args: &[&str]) -> anyhow::Result<String> {
        let args = args
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        self.run_docker_args(&args).await
    }

    async fn run_docker_args(&self, args: &[String]) -> anyhow::Result<String> {
        let output = self
            .runner
            .run("docker", args, &self.cwd, COMMAND_TIMEOUT)
            .await?;
        if !output.success {
            anyhow::bail!("Docker 命令执行失败: {}", output.log.trim());
        }
        Ok(output.log)
    }

    async fn container_image_counts(&self) -> anyhow::Result<BTreeMap<String, u64>> {
        let output = self
            .run_docker(&["container", "ls", "--all", "--quiet", "--no-trunc"])
            .await?;
        let container_ids = output
            .lines()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        let mut counts = BTreeMap::new();
        for chunk in container_ids.chunks(100) {
            let mut args = vec![
                "container".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Image}}".into(),
            ];
            args.extend(chunk.iter().map(|value| (*value).to_owned()));
            let images = self.run_docker_args(&args).await?;
            for image_id in images
                .lines()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                *counts.entry(image_id.to_owned()).or_default() += 1;
            }
        }
        Ok(counts)
    }
}

fn parse_df(value: &str) -> anyhow::Result<SpaceUsage> {
    let line = value
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("df 未返回磁盘信息"))?;
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        anyhow::bail!("df 返回格式无效");
    }
    Ok(SpaceUsage {
        mount_point: fields[0].replace("\\040", " "),
        total_bytes: fields[1].parse()?,
        used_bytes: fields[2].parse()?,
        available_bytes: fields[3].parse()?,
        used_percent: fields[4].trim_end_matches('%').parse()?,
    })
}

fn parse_meminfo(value: &str) -> anyhow::Result<(SpaceUsage, SpaceUsage)> {
    let mut fields = BTreeMap::new();
    for line in value.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if let Some(number) = rest.split_whitespace().next() {
            fields.insert(key, number.parse::<u64>()?.saturating_mul(1024));
        }
    }
    let usage = |total_key: &str, available_key: &str| -> anyhow::Result<SpaceUsage> {
        let total = *fields
            .get(total_key)
            .ok_or_else(|| anyhow::anyhow!("/proc/meminfo 缺少 {total_key}"))?;
        let available = *fields.get(available_key).unwrap_or(&0);
        let used = total.saturating_sub(available);
        Ok(SpaceUsage {
            mount_point: String::new(),
            total_bytes: total,
            used_bytes: used,
            available_bytes: available,
            used_percent: percent(used, total),
        })
    };
    Ok((
        usage("MemTotal", "MemAvailable")?,
        usage("SwapTotal", "SwapFree")?,
    ))
}

fn parse_loadavg(value: &str) -> anyhow::Result<[f64; 3]> {
    let values = value
        .split_whitespace()
        .take(3)
        .map(str::parse)
        .collect::<Result<Vec<f64>, _>>()?;
    values
        .try_into()
        .map_err(|_| anyhow::anyhow!("/proc/loadavg 返回格式无效"))
}

fn parse_uptime(value: &str) -> anyhow::Result<u64> {
    Ok(value
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("/proc/uptime 返回格式无效"))?
        .parse::<f64>()?
        .max(0.0) as u64)
}

fn parse_docker_df(value: &str) -> anyhow::Result<DockerUsage> {
    let mut usage = DockerUsage::default();
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        let row: DockerDfRow = serde_json::from_str(line)?;
        let resource = DockerResourceUsage {
            total: row.total.parse()?,
            active: row.active.parse()?,
            size_bytes: parse_size(&row.size)?,
            reclaimable_bytes: parse_size(
                row.reclaimable
                    .split_whitespace()
                    .next()
                    .unwrap_or_default(),
            )?,
        };
        match row.kind.as_str() {
            "Images" => usage.images = resource,
            "Containers" => usage.containers = resource,
            "Local Volumes" => usage.local_volumes = resource,
            "Build Cache" => usage.build_cache = resource,
            _ => {}
        }
    }
    Ok(usage)
}

fn parse_images(
    value: &str,
    services: &[(String, String, String)],
    protected: &BTreeMap<String, BTreeSet<String>>,
    container_images: &BTreeMap<String, u64>,
) -> anyhow::Result<Vec<ManagedImage>> {
    let mut rows = BTreeMap::<String, Vec<DockerImageRow>>::new();
    for line in value.lines().filter(|line| !line.trim().is_empty()) {
        let row: DockerImageRow = serde_json::from_str(line)?;
        rows.entry(row.id.clone()).or_default().push(row);
    }
    let mut images = Vec::new();
    for (id, rows) in rows {
        let references = rows
            .iter()
            .flat_map(|row| {
                services
                    .iter()
                    .filter(|(_, _, repository)| repositories_match(&row.repository, repository))
                    .map(|(project_id, service_id, _)| ImageReference {
                        project_id: project_id.clone(),
                        service_id: service_id.clone(),
                    })
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if references.is_empty() {
            continue;
        }
        let aliases = rows
            .iter()
            .map(|row| ImageAlias {
                repository: row.repository.clone(),
                tag: row.tag.clone(),
                digest: (row.digest != "<none>").then(|| row.digest.clone()),
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let primary = rows
            .iter()
            .find(|row| {
                services
                    .iter()
                    .any(|(_, _, repository)| repositories_match(&row.repository, repository))
            })
            .expect("managed image must have a matching row");
        let containers = container_images.get(&id).copied().unwrap_or(0);
        let mut reasons = protected.get(&id).cloned().unwrap_or_default();
        if containers > 0 {
            reasons.insert("container".into());
        }
        if aliases.len() > 1 {
            reasons.insert("multiple_tags".into());
        }
        images.push(ManagedImage {
            id,
            repository: primary.repository.clone(),
            tag: primary.tag.clone(),
            digest: (primary.digest != "<none>").then(|| primary.digest.clone()),
            aliases,
            size_bytes: parse_size(&primary.size)?,
            created_at: primary.created_at.clone(),
            containers,
            services: references,
            removable: reasons.is_empty(),
            protected_reasons: reasons.into_iter().collect(),
        });
    }
    images.sort_by(|left, right| {
        left.repository
            .cmp(&right.repository)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.tag.cmp(&right.tag))
    });
    Ok(images)
}

fn parse_size(value: &str) -> anyhow::Result<u64> {
    let split = value
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(value.len());
    let number: f64 = value[..split].parse()?;
    let unit = value[split..].trim();
    let multiplier = match unit {
        "B" | "" => 1_f64,
        "kB" | "KB" => 1_000_f64,
        "MB" => 1_000_000_f64,
        "GB" => 1_000_000_000_f64,
        "TB" => 1_000_000_000_000_f64,
        "KiB" => 1_024_f64,
        "MiB" => 1_048_576_f64,
        "GiB" => 1_073_741_824_f64,
        "TiB" => 1_099_511_627_776_f64,
        _ => anyhow::bail!("无法识别空间单位: {unit}"),
    };
    Ok((number * multiplier).round().max(0.0) as u64)
}

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 * 100.0 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_and_docker_space() {
        let disk =
            parse_df("Mounted on 1B-blocks Used Available Use%\n/ 1000 650 350 65%\n").unwrap();
        assert_eq!(disk.mount_point, "/");
        assert_eq!(disk.available_bytes, 350);
        assert_eq!(disk.used_percent, 65.0);

        let (memory, swap) = parse_meminfo(
            "MemTotal: 4000 kB\nMemAvailable: 2500 kB\nSwapTotal: 1000 kB\nSwapFree: 800 kB\n",
        )
        .unwrap();
        assert_eq!(memory.used_bytes, 1_500 * 1024);
        assert_eq!(swap.available_bytes, 800 * 1024);

        let docker = parse_docker_df(
            r#"{"Active":"7","Reclaimable":"3.005GB (67%)","Size":"4.468GB","TotalCount":"28","Type":"Images"}
{"Active":"0","Reclaimable":"8.142GB","Size":"11.13GB","TotalCount":"98","Type":"Build Cache"}"#,
        )
        .unwrap();
        assert_eq!(docker.images.total, 28);
        assert_eq!(docker.images.reclaimable_bytes, 3_005_000_000);
        assert_eq!(docker.build_cache.size_bytes, 11_130_000_000);
    }

    #[test]
    fn only_lists_managed_repositories_and_protects_referenced_images() {
        let rows = r#"{"Containers":"N/A","CreatedAt":"2026-07-30 00:00:00 +0000 UTC","Digest":"sha256:old","ID":"sha256:old","Repository":"ghcr.io/me/app","Size":"100MB","Tag":"1.0.0"}
{"Containers":"N/A","CreatedAt":"2026-07-31 00:00:00 +0000 UTC","Digest":"sha256:current","ID":"sha256:current","Repository":"ghcr.io/me/app","Size":"110MB","Tag":"1.1.0"}
{"Containers":"N/A","CreatedAt":"2026-07-31 00:00:00 +0000 UTC","Digest":"sha256:other","ID":"sha256:other","Repository":"other/app","Size":"90MB","Tag":"latest"}"#;
        let services = vec![("project".into(), "web".into(), "ghcr.io/me/app".into())];
        let mut protected = BTreeMap::new();
        protected.insert("sha256:old".into(), BTreeSet::from(["rollback".into()]));
        let containers = BTreeMap::from([("sha256:current".into(), 1)]);
        let images = parse_images(rows, &services, &protected, &containers).unwrap();
        assert_eq!(images.len(), 2);
        assert!(!images[0].removable);
        assert_eq!(images[0].protected_reasons, ["container"]);
        assert!(!images[1].removable);
        assert_eq!(images[1].protected_reasons, ["rollback"]);
    }

    #[test]
    fn merges_aliases_for_the_same_image_id_and_does_not_double_count_it() {
        let rows = r#"{"Containers":"N/A","CreatedAt":"2026-07-31 00:00:00 +0000 UTC","Digest":"sha256:same","ID":"sha256:same","Repository":"ghcr.io/me/app","Size":"100MB","Tag":"1.0.0"}
{"Containers":"N/A","CreatedAt":"2026-07-31 00:00:00 +0000 UTC","Digest":"sha256:same","ID":"sha256:same","Repository":"ghcr.io/me/app","Size":"100MB","Tag":"latest"}"#;
        let services = vec![("project".into(), "web".into(), "ghcr.io/me/app".into())];
        let images = parse_images(rows, &services, &BTreeMap::new(), &BTreeMap::new()).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].aliases.len(), 2);
        assert_eq!(images[0].protected_reasons, ["multiple_tags"]);
        assert!(!images[0].removable);
    }
}
