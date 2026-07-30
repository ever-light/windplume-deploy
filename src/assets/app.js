const $ = (selector) => document.querySelector(selector);
let projects = [];
let selected = null;
let latestVersion = null;
let latestSystemRelease = null;
let systemUpdatePolling = false;
let diagnostics = [];
let containerLogsRequest = 0;

const esc = (value) =>
  String(value ?? "—").replace(
    /[&<>"']/g,
    (char) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        char
      ],
  );

async function api(url, options) {
  const response = await fetch(url, options);
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
  return `<span class="pill ${ok ? "ok" : bad ? "bad" : ""}">${esc(status)}</span>`;
}

function serviceCard(project, service) {
  const consistency = !service.desired_image
    ? '<span class="muted">未建立部署基线</span>'
    : `<span class="${service.drift ? "drift" : ""}">${service.drift ? "存在 drift" : "一致"}</span>`;
  const busy = project.deployment_in_progress;
  const missing = ["down", "unknown"].includes(service.container_status);
  const stopped = ["exited", "dead"].includes(service.container_status);
  return `<article class="card" data-project="${esc(project.id)}" data-service="${esc(service.id)}">
    <div class="card-head">
      <div><h3>${esc(service.id)}</h3><span class="muted">${esc(service.version_source)}</span></div>
      ${badge(service.container_status)}
    </div>
    <div class="kv">
      <span>期望版本</span><strong>${esc(service.desired_version)}</strong>
      <span>期望镜像</span><code>${esc(service.desired_image)}</code>
      <span>实际镜像</span><code>${esc(service.actual_image)}</code>
      <span>一致性</span>${consistency}
    </div>
    <div class="actions service-actions" style="justify-content:flex-end;flex-wrap:wrap;margin-top:14px">
      <button class="service-logs" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${missing ? "disabled" : ""}>查看日志</button>
      <button class="lifecycle" data-action="recreate" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy ? "disabled" : ""}>重建当前版本</button>
      <button class="lifecycle" data-action="stop" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy || missing || stopped ? "disabled" : ""}>停止</button>
      <button class="lifecycle danger" data-action="down" data-project="${esc(project.id)}" data-service="${esc(service.id)}" ${busy || missing ? "disabled" : ""}>下线</button>
    </div>
  </article>`;
}

async function loadProjects() {
  try {
    projects = await api("/api/projects");
    const busyCount = projects.filter((project) => project.deployment_in_progress).length;
    $("#global").textContent = busyCount ? `${busyCount} 个项目操作中` : "空闲";
    $("#global").className = `pill ${busyCount ? "" : "ok"}`;
    $("#projects").innerHTML =
      projects
        .map(
          (project) => `<section class="project-block">
            <div class="project-head">
              <div><h3>${esc(project.id)}</h3><p class="muted">${esc(project.compose_files.join(", "))}</p></div>
              <div class="actions"><button class="refresh-compose" data-project="${esc(project.id)}" ${project.deployment_in_progress ? "disabled" : ""}>刷新 Compose</button>${project.deployment_in_progress ? badge("操作中") : ""}</div>
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
  } catch (error) {
    diagnose("项目状态", error.message);
    $("#projects").innerHTML = `<p class="drift">${esc(error.message)}</p>`;
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
      recreate: "重建当前版本",
      stop: "停止",
      down: "下线",
    }[operation] || operation
  );
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
        "自更新不可用（需 Linux x86_64，并需运行新版 install.sh 安装签名公钥和 v2 更新助手）";
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
$("#diagnostics-clear").onclick = () => {
  diagnostics = [];
  $("#diagnostics").textContent = "（暂无诊断信息）";
};
loadProjects();
loadHistory();
loadSystemUpdate();
setInterval(() => {
  loadProjects();
  loadHistory();
}, 10000);
