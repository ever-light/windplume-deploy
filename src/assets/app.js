const $ = (selector) => document.querySelector(selector);
let projects = [];
let selected = null;
let latestVersion = null;
let latestSystemRelease = null;
let systemUpdatePolling = false;
let diagnostics = [];
let containerLogsRequest = 0;
let csrfToken = null;
let projectsLoading = false;
let historyLoading = false;
let systemResourcesLoading = false;
let systemResourcesLoaded = false;
let runtimeRefreshTimer = null;

const esc = (value) =>
  String(value ?? "—").replace(
    /[&<>"']/g,
    (char) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        char
      ],
  );

async function api(url, options) {
  const request = { ...(options || {}) };
  const method = String(request.method || "GET").toUpperCase();
  if (!["GET", "HEAD", "OPTIONS"].includes(method)) {
    if (!csrfToken) {
      const response = await fetch("/api/session", { cache: "no-store" });
      if (!response.ok) throw new Error("无法建立安全会话");
      csrfToken = (await response.json()).csrf_token;
    }
    request.headers = {
      ...(request.headers || {}),
      "X-Windplume-CSRF": csrfToken,
    };
  }
  const response = await fetch(url, request);
  let data;
  try {
    data = await response.json();
  } catch {
    data = { message: await response.text() };
  }
  if (!response.ok) {
    throw new Error(data.message || `请求失败 (${response.status})`);
  }
  return data;
}

function toast(message) {
  const element = $("#toast");
  element.textContent = message;
  element.style.display = "block";
  setTimeout(() => (element.style.display = "none"), 4000);
}

function diagnose(scope, message) {
  diagnostics.unshift({ at: new Date(), scope, message });
  diagnostics = diagnostics.slice(0, 100);
  $("#diagnostics").textContent = diagnostics
    .map((item) => `[${item.at.toLocaleString()}] ${item.scope}: ${item.message}`)
    .join("\n");
}

function badge(status) {
  const ok = ["healthy", "running", "succeeded"].includes(status);
  const bad = ["unhealthy", "failed", "exited", "dead", "interrupted"].includes(
    status,
  );
  const label = status === "loading" ? "读取中" : status;
  return `<span class="pill ${ok ? "ok" : bad ? "bad" : ""}">${esc(label)}</span>`;
}

function formatBytes(bytes) {
  const value = Number(bytes || 0);
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const index = Math.min(
    Math.floor(Math.log(value) / Math.log(1024)),
    units.length - 1,
  );
  const scaled = value / 1024 ** index;
  return `${scaled >= 10 || index === 0 ? scaled.toFixed(0) : scaled.toFixed(1)} ${units[index]}`;
}

function formatUptime(seconds) {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  return `${days ? `${days} 天 ` : ""}${hours} 小时 ${minutes} 分`;
}

function protectedReason(reason) {
  return (
    {
      container: "容器引用",
      current: "当前部署",
      rollback: "回退版本",
      multiple_tags: "存在多个标签",
    }[reason] || reason
  );
}

function metric(label, value, detail = "") {
  return `<article><span>${esc(label)}</span><strong>${esc(value)}</strong>${detail ? `<small>${esc(detail)}</small>` : ""}</article>`;
}

function updateImageCleanupButton() {
  const selected = document.querySelectorAll(".image-select:checked").length;
  const button = $("#image-cleanup");
  button.disabled = selected === 0;
  button.textContent = selected ? `清理选中镜像（${selected}）` : "清理选中镜像";
}

async function loadSystemResources() {
  if (systemResourcesLoading) return;
  systemResourcesLoading = true;
  $("#system-resources-refresh").disabled = true;
  try {
    const [overview, inventory] = await Promise.all([
      api("/api/system/overview"),
      api("/api/system/images"),
    ]);
    const systemDisk = overview.system_disk;
    const dataDisk = overview.data_disk;
    const sameDisk = systemDisk.mount_point === dataDisk.mount_point;
    const systemDiskDetail = `挂载于 ${systemDisk.mount_point} · 已用 ${formatBytes(systemDisk.used_bytes)}，可用 ${formatBytes(systemDisk.available_bytes)}`;
    const dataDiskDetail = sameDisk
      ? `与系统盘相同 · 可用 ${formatBytes(dataDisk.available_bytes)}`
      : `挂载于 ${dataDisk.mount_point} · 已用 ${formatBytes(dataDisk.used_bytes)}，可用 ${formatBytes(dataDisk.available_bytes)}`;
    const memoryDetail = `可用 ${formatBytes(overview.memory.available_bytes)}`;
    $("#system-metrics").innerHTML = [
      metric(
        "系统盘",
        `${systemDisk.used_percent.toFixed(0)}%`,
        systemDiskDetail,
      ),
      metric(
        "数据盘",
        `${dataDisk.used_percent.toFixed(0)}%`,
        dataDiskDetail,
      ),
      metric(
        "内存",
        `${overview.memory.used_percent.toFixed(0)}%`,
        memoryDetail,
      ),
      metric(
        "Docker 镜像",
        `${overview.docker.images.total} 个 · ${formatBytes(overview.docker.images.size_bytes)}`,
        `可回收约 ${formatBytes(overview.docker.images.reclaimable_bytes)}`,
      ),
      metric(
        "构建缓存",
        formatBytes(overview.docker.build_cache.size_bytes),
        `7 天前缓存可手动清理；Docker 估算可回收 ${formatBytes(overview.docker.build_cache.reclaimable_bytes)}`,
      ),
      metric(
        "系统负载",
        overview.load_average.map((value) => value.toFixed(2)).join(" / "),
        "1 / 5 / 15 分钟",
      ),
      metric("运行时间", formatUptime(overview.uptime_seconds)),
    ].join("");
    $("#build-cache-cleanup").disabled =
      overview.docker.build_cache.reclaimable_bytes === 0;
    $("#image-cleanup-summary").textContent =
      `识别到 ${inventory.images.length} 个受管镜像，其中 ${inventory.removable_count} 个当前可清理，标称大小合计 ${formatBytes(inventory.removable_size_bytes)}；共享层会影响实际释放空间。`;
    $("#managed-images").innerHTML =
      inventory.images
        .map((image) => {
          const services = image.services
            .map((service) => `${service.project_id} / ${service.service_id}`)
            .join("，");
          const status = image.removable
            ? '<span class="pill">可清理</span>'
            : image.protected_reasons
                .map((reason) => `<span class="pill ok">${esc(protectedReason(reason))}</span>`)
                .join(" ");
          const aliases = image.aliases
            .map((alias) => `${alias.repository}:${alias.tag}`)
            .join("，");
          return `<tr>
            <td class="check-cell"><input class="image-select" type="checkbox" value="${esc(image.id)}" ${image.removable ? "" : "disabled"} aria-label="选择 ${esc(image.repository)}:${esc(image.tag)}"></td>
            <td><div class="image-name"><code>${esc(aliases)}</code><span class="muted">${esc(image.id.slice(0, 19))}…</span></div></td>
            <td>${esc(services)}</td>
            <td>${esc(formatBytes(image.size_bytes))}</td>
            <td>${esc(image.created_at)}</td>
            <td>${status}</td>
          </tr>`;
        })
        .join("") || '<tr><td colspan="6">本机没有受管 Compose 镜像</td></tr>';
    document.querySelectorAll(".image-select").forEach((checkbox) => {
      checkbox.onchange = updateImageCleanupButton;
    });
    updateImageCleanupButton();
    systemResourcesLoaded = true;
  } catch (error) {
    diagnose("系统空间与镜像", error.message);
    $("#system-metrics").innerHTML =
      metric("读取失败", error.message);
    $("#managed-images").innerHTML =
      `<tr><td colspan="6" class="drift">${esc(error.message)}</td></tr>`;
    $("#image-cleanup-summary").textContent = "系统资源读取失败";
    $("#image-cleanup").disabled = true;
    $("#build-cache-cleanup").disabled = true;
  } finally {
    $("#system-resources-refresh").disabled = false;
    systemResourcesLoading = false;
  }
}

function selectTopTab(tab, updateLocation = true) {
  const selectedTab = tab === "system" ? "system" : "deploy";
  for (const name of ["deploy", "system"]) {
    const active = name === selectedTab;
    $(`#panel-${name}`).hidden = !active;
    const button = $(`#tab-${name}`);
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
  }
  if (updateLocation) {
    const target = selectedTab === "system"
      ? `${window.location.pathname}${window.location.search}#system`
      : `${window.location.pathname}${window.location.search}`;
    window.history.replaceState(null, "", target);
  }
  if (selectedTab === "system" && !systemResourcesLoaded) {
    void loadSystemResources();
  }
}

async function cleanupImages() {
  const imageIds = [
    ...new Set(
      [...document.querySelectorAll(".image-select:checked")].map(
        (checkbox) => checkbox.value,
      ),
    ),
  ];
  if (!imageIds.length) return;
  if (
    !window.confirm(
      `确定删除选中的 ${imageIds.length} 个镜像吗？\n\n服务会在删除前重新检查容器、当前部署和回退版本引用。此操作不可恢复。`,
    )
  ) {
    return;
  }
  $("#image-cleanup").disabled = true;
  try {
    const result = await api("/api/system/images/cleanup", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ image_ids: imageIds }),
    });
    if (result.failed.length) {
      diagnose(
        "镜像清理",
        result.failed.map((item) => `${item.id}: ${item.message}`).join("\n"),
      );
    }
    toast(
      `已删除 ${result.deleted_ids.length} 个镜像` +
        (result.failed.length ? `，${result.failed.length} 个删除失败` : ""),
    );
    await loadSystemResources();
  } catch (error) {
    diagnose("镜像清理", error.message);
    toast(error.message);
    updateImageCleanupButton();
  }
}

async function cleanupBuildCache() {
  if (
    !window.confirm(
      "确定清理 7 天前未使用的 Docker 构建缓存吗？\n\n不会删除镜像、容器或卷，但之后构建可能需要重新下载依赖。",
    )
  ) {
    return;
  }
  $("#build-cache-cleanup").disabled = true;
  try {
    await api("/api/system/build-cache/cleanup", { method: "POST" });
    toast("构建缓存清理完成");
    await loadSystemResources();
  } catch (error) {
    diagnose("构建缓存清理", error.message);
    toast(error.message);
  }
}

function serviceCard(project, service) {
  const imageStatus = service.image_status;
  const deploymentVersion =
    imageStatus === "unmanaged"
      ? '<span class="muted">未纳管</span>'
      : `<strong>${esc(service.desired_version)}</strong>`;
  const driftDetails =
    imageStatus === "drift"
      ? `<span>镜像状态</span><span class="drift">运行镜像偏离部署基线</span>
         <span>基线镜像</span><code>${esc(service.desired_image)}</code>`
      : "";
  const runningImage = service.container_status === "loading"
    ? "正在读取…"
    : service.replicas > 1 && !service.actual_image
      ? "多个副本镜像不一致"
      : service.actual_image;
  const busy = project.deployment_in_progress;
  const missing = ["down", "unknown", "loading"].includes(
    service.container_status,
  );
  const stopped = ["exited", "dead"].includes(service.container_status);
  return `<article class="card" data-project="${esc(project.id)}" data-service="${esc(service.id)}">
    <div class="card-head">
      <div><h3>${esc(service.id)}</h3><span class="muted">${esc(service.version_source)}${service.replicas > 1 ? ` · ${esc(service.replicas)} 副本` : ""}</span></div>
      ${badge(service.container_status)}
    </div>
    <div class="kv">
      <span>部署版本</span>${deploymentVersion}
      <span>运行镜像</span><code>${esc(runningImage)}</code>
      ${driftDetails}
    </div>
    <div class="actions service-actions" style="justify-content:flex-end;flex-wrap:wrap;margin-top:14px">
      <button class="service-logs" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${missing ? "disabled" : ""}>查看日志</button>
      <button class="rollback" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy || !service.rollback_available ? "disabled" : ""}>回滚上一版本</button>
      <button class="lifecycle" data-action="recreate" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy ? "disabled" : ""}>重建当前版本</button>
      <button class="lifecycle" data-action="stop" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy || missing || stopped ? "disabled" : ""}>停止</button>
      <button class="lifecycle danger" data-action="down" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy || missing ? "disabled" : ""}>下线</button>
    </div>
  </article>`;
}

async function loadProjects() {
  if (projectsLoading) return;
  projectsLoading = true;
  try {
    projects = await api("/api/projects");
    if (runtimeRefreshTimer) {
      clearTimeout(runtimeRefreshTimer);
      runtimeRefreshTimer = null;
    }
    if (
      !document.hidden &&
      projects.some((project) => project.runtime_refreshing)
    ) {
      runtimeRefreshTimer = setTimeout(loadProjects, 1000);
    }
    const busyCount = projects.filter((project) => project.deployment_in_progress).length;
    $("#global").textContent = busyCount ? `${busyCount} 个项目操作中` : "空闲";
    $("#global").className = `pill ${busyCount ? "" : "ok"}`;
    $("#projects").innerHTML =
      projects
        .map(
          (project) => `<section class="project-block">
            <div class="project-head">
              <div><h3>${esc(project.id)}</h3><p class="muted">${esc(project.compose_files.join(", "))}</p></div>
              <div class="actions"><button class="refresh-compose" data-project="${esc(project.id)}" ${project.deployment_in_progress ? "disabled" : ""}>刷新 Compose</button>${project.deployment_in_progress ? badge("操作中") : project.runtime_refreshing ? badge("状态刷新中") : project.runtime_error ? `<span class="pill bad" title="${esc(project.runtime_error)}">状态读取失败</span>` : ""}</div>
            </div>
            <div class="cards">${project.services.map((service) => serviceCard(project, service)).join("")}</div>
          </section>`,
        )
        .join("") || '<p class="muted">没有配置 Compose 项目</p>';
    document.querySelectorAll(".card").forEach((card) => {
      card.onclick = () => selectService(card.dataset.project, card.dataset.service);
    });
    document.querySelectorAll(".service-logs").forEach((button) => {
      button.onclick = (event) => {
        event.stopPropagation();
        showContainerLogs(button.dataset.project, button.dataset.service);
      };
    });
    document.querySelectorAll(".refresh-compose").forEach((button) => {
      button.onclick = () => refreshCompose(button.dataset.project, button);
    });
    document.querySelectorAll(".lifecycle").forEach((button) => {
      button.onclick = (event) => {
        event.stopPropagation();
        confirmLifecycle(
          button.dataset.project,
          button.dataset.service,
          button.dataset.action,
        );
      };
    });
    document.querySelectorAll(".rollback").forEach((button) => {
      button.onclick = (event) => {
        event.stopPropagation();
        confirmRollback(button.dataset.project, button.dataset.service);
      };
    });
  } catch (error) {
    diagnose("项目状态", error.message);
    $("#projects").innerHTML = `<p class="drift">${esc(error.message)}</p>`;
  } finally {
    projectsLoading = false;
  }
}

async function selectService(projectId, serviceId, refresh = false) {
  const project = projects.find((item) => item.id === projectId);
  const service = project?.services.find((item) => item.id === serviceId);
  if (!project || !service) return;
  selected = { project, service };
  $("#versions-section").hidden = false;
  $("#versions-title").textContent = `${project.id} / ${service.id} · 可部署版本`;
  $("#versions-source").textContent = `版本来源：${service.version_source}`;
  $("#versions").innerHTML = '<tr><td colspan="2">正在读取…</td></tr>';
  latestVersion = null;
  $("#deploy-latest").disabled = true;
  try {
    const data = await api(
      `/api/projects/${encodeURIComponent(projectId)}/services/${encodeURIComponent(serviceId)}/versions?refresh=${refresh}`,
    );
    latestVersion =
      data.versions.find((item) => /^\d+\.\d+\.\d+$/.test(item.version))
        ?.version || null;
    $("#deploy-latest").disabled =
      !latestVersion ||
      project.deployment_in_progress ||
      latestVersion === service.desired_version;
    $("#versions").innerHTML =
      data.versions
        .map(
          (version) => `<tr>
            <td><strong>${esc(version.version)}</strong>${version.version === service.desired_version ? ' <span class="pill ok">当前</span>' : ""}</td>
            <td><button class="deploy" data-version="${esc(version.version)}" ${project.deployment_in_progress || version.version === service.desired_version ? "disabled" : ""}>部署</button></td>
          </tr>`,
        )
        .join("") || '<tr><td colspan="2">没有符合规则的标签</td></tr>';
    document.querySelectorAll(".deploy").forEach((button) => {
      button.onclick = () => confirmDeploy(button.dataset.version);
    });
  } catch (error) {
    diagnose(`${projectId} / ${serviceId} 版本查询`, error.message);
    $("#versions").innerHTML = `<tr><td colspan="2" class="drift">${esc(error.message)}</td></tr>`;
  }
}

function confirmDeploy(version) {
  const { project, service } = selected;
  $("#confirm-text").textContent =
    `项目：${project.id}\n服务：${service.id}\n当前版本：${service.desired_version || "未部署"}\n目标版本：${version}`;
  $("#confirm-go").onclick = async (event) => {
    event.preventDefault();
    $("#confirm-go").disabled = true;
    try {
      const deployment = await api(
        `/api/projects/${encodeURIComponent(project.id)}/services/${encodeURIComponent(service.id)}/deploy`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ version }),
        },
      );
      $("#confirm").close();
      toast(`任务 ${deployment.deployment_id} 已创建`);
      await poll(deployment.deployment_id);
    } catch (error) {
      diagnose(`${project.id} / ${service.id} 创建部署`, error.message);
      toast(error.message);
    } finally {
      $("#confirm-go").disabled = false;
    }
  };
  $("#confirm").showModal();
}

function operationLabel(operation) {
  return (
    {
      deploy: "部署版本",
      rollback: "回滚版本",
      recreate: "重建当前版本",
      stop: "停止",
      down: "下线",
    }[operation] || operation
  );
}

function confirmRollback(projectId, serviceId) {
  const project = projects.find((item) => item.id === projectId);
  const service = project?.services.find((item) => item.id === serviceId);
  if (!service) return;
  $("#confirm-text").textContent =
    `项目：${projectId}\n服务：${serviceId}\n当前版本：${service.desired_version || "未部署"}\n\n将恢复上一个成功部署版本；新部署记录会使用不可变镜像。`;
  $("#confirm-go").onclick = async (event) => {
    event.preventDefault();
    $("#confirm-go").disabled = true;
    try {
      const operation = await api(
        `/api/projects/${encodeURIComponent(projectId)}/services/${encodeURIComponent(serviceId)}/rollback`,
        { method: "POST" },
      );
      $("#confirm").close();
      toast(`回滚操作 ${operation.operation_id} 已创建`);
      await poll(operation.operation_id);
    } catch (error) {
      diagnose(`${projectId} / ${serviceId} 回滚`, error.message);
      toast(error.message);
    } finally {
      $("#confirm-go").disabled = false;
    }
  };
  $("#confirm").showModal();
}

function confirmLifecycle(projectId, serviceId, action) {
  const project = projects.find((item) => item.id === projectId);
  const service = project?.services.find((item) => item.id === serviceId);
  if (!project || !service) return;
  const warning =
    action === "down"
      ? "\n将停止并删除该服务容器，但不删除镜像、命名卷或绑定目录。"
      : action === "recreate"
        ? "\n将重新读取 Compose 和环境文件，并使用当前镜像重建容器。"
        : "\n将停止并保留该服务容器。";
  $("#confirm-text").textContent =
    `项目：${projectId}\n服务：${serviceId}\n操作：${operationLabel(action)}${warning}`;
  $("#confirm-go").onclick = async (event) => {
    event.preventDefault();
    $("#confirm-go").disabled = true;
    try {
      const operation = await api(
        `/api/projects/${encodeURIComponent(projectId)}/services/${encodeURIComponent(serviceId)}/lifecycle`,
        {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ action }),
        },
      );
      $("#confirm").close();
      toast(`操作 ${operation.operation_id} 已创建`);
      await poll(operation.operation_id);
    } catch (error) {
      diagnose(`${projectId} / ${serviceId} ${operationLabel(action)}`, error.message);
      toast(error.message);
    } finally {
      $("#confirm-go").disabled = false;
    }
  };
  $("#confirm").showModal();
}

async function poll(id) {
  for (;;) {
    await new Promise((resolve) => setTimeout(resolve, 2000));
    try {
      const deployment = await api(`/api/deployments/${id}`);
      await loadHistory();
      if (["succeeded", "failed", "interrupted"].includes(deployment.status)) {
        const projectId = deployment.project_id;
        const serviceId = deployment.service_id;
        await loadProjects();
        await selectService(projectId, serviceId);
        toast(
          deployment.status === "succeeded"
            ? `${operationLabel(deployment.operation)}成功`
            : `${operationLabel(deployment.operation)}失败：${deployment.error_message || deployment.status}`,
        );
        return;
      }
    } catch (error) {
      diagnose(`操作任务 ${id}`, error.message);
      toast(error.message);
    }
  }
}

function elapsed(deployment) {
  const end = deployment.finished_at
    ? new Date(deployment.finished_at)
    : new Date();
  return `${Math.max(0, Math.round((end - new Date(deployment.started_at)) / 1000))}s`;
}

async function loadHistory() {
  if (historyLoading) return;
  historyLoading = true;
  try {
    const rows = await api("/api/deployments?limit=50");
    $("#history").innerHTML =
      rows
        .map(
          (deployment) => `<tr data-id="${esc(deployment.id)}">
            <td>${new Date(deployment.started_at).toLocaleString()}</td>
            <td>${esc(deployment.project_id)} / ${esc(deployment.service_id)}</td>
            <td>${esc(operationLabel(deployment.operation))}</td>
            <td>${deployment.operation === "deploy" ? `${esc(deployment.previous_version)} → ${esc(deployment.target_version)}` : esc(deployment.target_version || "当前配置")}</td>
            <td>${badge(deployment.status)}</td><td>${elapsed(deployment)}</td>
            <td>${esc(deployment.rollback_status)}</td>
          </tr>`,
        )
        .join("") || '<tr><td colspan="7">暂无操作历史</td></tr>';
    document.querySelectorAll("#history tr[data-id]").forEach((row) => {
      row.onclick = () => showDetail(row.dataset.id);
    });
  } catch (error) {
    diagnose("操作历史", error.message);
    toast(error.message);
  } finally {
    historyLoading = false;
  }
}

async function showDetail(id) {
  try {
    const deployment = await api(`/api/deployments/${id}`);
    $("#detail-summary").textContent =
      `${deployment.project_id} / ${deployment.service_id} · ${operationLabel(deployment.operation)}${deployment.operation === "deploy" ? ` · ${deployment.previous_version || "未部署"} → ${deployment.target_version}` : ""} · ${deployment.status}`;
    $("#detail-log").textContent =
      deployment.command_log || deployment.error_message || "（无日志）";
    $("#detail").showModal();
  } catch (error) {
    diagnose(`部署详情 ${id}`, error.message);
    toast(error.message);
  }
}

async function showContainerLogs(projectId, serviceId) {
  const content = $("#container-logs-content");
  const requestId = ++containerLogsRequest;
  let currentTail = 0;
  let maxTail = 200;
  let loading = false;
  $("#container-logs-summary").textContent =
    `${projectId} / ${serviceId} · 最近 50 行`;
  content.textContent = "正在读取…";
  $("#container-logs").showModal();

  const load = async (tail, preservePosition) => {
    if (loading) return;
    loading = true;
    const previousHeight = content.scrollHeight;
    const previousTop = content.scrollTop;
    try {
      const data = await api(
        `/api/projects/${encodeURIComponent(projectId)}/services/${encodeURIComponent(serviceId)}/logs?tail=${tail}`,
      );
      if (requestId !== containerLogsRequest) return;
      content.textContent = data.logs || "（暂无日志）";
      currentTail = data.tail;
      maxTail = data.max_tail;
      $("#container-logs-summary").textContent =
        `${projectId} / ${serviceId} · 最近 ${data.tail} 行` +
        (data.tail < data.max_tail
          ? ` · 上滑到顶部继续加载（最多 ${data.max_tail} 行）`
          : " · 已到加载上限");
      content.scrollTop = preservePosition
        ? content.scrollHeight - previousHeight + previousTop
        : content.scrollHeight;
    } catch (error) {
      diagnose(`${projectId} / ${serviceId} 容器日志`, error.message);
      if (currentTail === 0) {
        content.textContent = `读取失败：${error.message}`;
      } else {
        toast(`继续加载日志失败：${error.message}`);
      }
    } finally {
      loading = false;
    }
  };

  content.onscroll = () => {
    if (content.scrollTop <= 1 && currentTail > 0 && currentTail < maxTail) {
      void load(currentTail === 50 ? 100 : maxTail, true);
    }
  };
  await load(50, false);
}

async function cleanupHistory() {
  if (!window.confirm("确定清除 30 天前已完成的操作记录吗？此操作不可恢复。")) {
    return;
  }
  const button = $("#history-cleanup");
  button.disabled = true;
  try {
    const result = await api("/api/deployments/cleanup", { method: "POST" });
    toast(`已清除 ${result.deleted} 条旧操作记录`);
    await loadHistory();
  } catch (error) {
    diagnose("清理操作历史", error.message);
    toast(error.message);
  } finally {
    button.disabled = false;
  }
}

async function refreshCompose(projectId, button) {
  button.disabled = true;
  try {
    const result = await api(
      `/api/projects/${encodeURIComponent(projectId)}/refresh-compose`,
      { method: "POST" },
    );
    if (selected?.project.id === projectId) {
      selected = null;
      latestVersion = null;
      $("#versions-section").hidden = true;
    }
    await loadProjects();
    toast(`Compose 已刷新，识别到 ${result.service_count} 个服务`);
  } catch (error) {
    diagnose(`${projectId} 刷新 Compose`, error.message);
    toast(error.message);
    button.disabled = false;
  }
}

async function loadSystemUpdate(refresh = false) {
  $("#update-check").disabled = true;
  try {
    const data = await api(`/api/system/update?refresh=${refresh}`);
    latestSystemRelease = data.latest;
    $("#update-current").textContent = data.current_version;
    $("#update-latest").innerHTML =
      `<a href="${esc(data.latest.html_url)}" target="_blank" rel="noreferrer">${esc(data.latest.version)}</a>`;
    $("#update-status").textContent = data.status.message;
    const active = [
      "downloading",
      "verifying",
      "ready",
      "installing",
      "rolling_back",
    ].includes(data.status.state);
    $("#update-install").disabled =
      !data.self_update_supported || !data.update_available || active;
    if (!data.self_update_supported) {
      $("#update-status").textContent =
        "自更新不可用（需 Linux x86_64，并需运行 install.sh 安装签名公钥和更新助手）";
    }
    if (active && !systemUpdatePolling) pollSystemUpdate();
  } catch (error) {
    diagnose("系统更新检查", error.message);
    $("#update-status").textContent = error.message;
    $("#update-install").disabled = true;
  } finally {
    $("#update-check").disabled = false;
  }
}

function confirmSystemUpdate() {
  if (!latestSystemRelease) return;
  $("#confirm-text").textContent =
    `目标版本：${latestSystemRelease.version}\n\n更新会下载并校验 GitHub Release，然后重启 Deploy 管理程序。\n管理页面会短暂断开，业务容器不会被停止。`;
  $("#confirm-go").onclick = async (event) => {
    event.preventDefault();
    $("#confirm-go").disabled = true;
    try {
      const result = await api("/api/system/update", { method: "POST" });
      $("#confirm").close();
      $("#update-install").disabled = true;
      toast(`已开始更新到 ${result.target_version}`);
      await pollSystemUpdate();
    } catch (error) {
      diagnose("启动系统更新", error.message);
      toast(error.message);
    } finally {
      $("#confirm-go").disabled = false;
    }
  };
  $("#confirm").showModal();
}

async function pollSystemUpdate() {
  if (systemUpdatePolling) return;
  systemUpdatePolling = true;
  for (let attempt = 0; attempt < 90; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 2000));
    try {
      const status = await api("/api/system/update/status");
      $("#update-status").textContent = status.message;
      if (status.state === "succeeded") {
        toast("系统更新成功，正在重新加载页面");
        window.location.reload();
        return;
      }
      if (["failed", "rolled_back"].includes(status.state)) {
        diagnose("系统更新", status.message);
        toast(status.message);
        await loadSystemUpdate();
        systemUpdatePolling = false;
        return;
      }
    } catch {
      $("#update-status").textContent = "Deploy 正在重启，等待恢复连接…";
    }
  }
  diagnose("系统更新", "等待更新结果超时");
  toast("等待更新结果超时");
  systemUpdatePolling = false;
}

$("#refresh").onclick = () =>
  selected &&
  selectService(selected.project.id, selected.service.id, true);
$("#deploy-latest").onclick = () =>
  latestVersion && confirmDeploy(latestVersion);
$("#history-refresh").onclick = loadHistory;
$("#history-cleanup").onclick = cleanupHistory;
$("#update-check").onclick = () => loadSystemUpdate(true);
$("#update-install").onclick = confirmSystemUpdate;
$("#system-resources-refresh").onclick = loadSystemResources;
$("#image-cleanup").onclick = cleanupImages;
$("#build-cache-cleanup").onclick = cleanupBuildCache;
document.querySelectorAll(".top-tabs button").forEach((button) => {
  button.onclick = () => selectTopTab(button.dataset.tab);
});
$("#diagnostics-clear").onclick = () => {
  diagnostics = [];
  $("#diagnostics").textContent = "（暂无诊断信息）";
};
loadProjects();
loadHistory();
loadSystemUpdate();
selectTopTab(window.location.hash === "#system" ? "system" : "deploy", false);
setInterval(() => {
  if (!document.hidden) {
    loadProjects();
    loadHistory();
  }
}, 10000);
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) {
    loadProjects();
    loadHistory();
  } else if (runtimeRefreshTimer) {
    clearTimeout(runtimeRefreshTimer);
    runtimeRefreshTimer = null;
  }
});
