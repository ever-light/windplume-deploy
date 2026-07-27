const $ = (selector) => document.querySelector(selector);
let projects = [];
let selected = null;
let latestVersion = null;
let diagnostics = [];

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
    <div class="actions" style="justify-content:flex-end;margin-top:14px"><button class="service-logs" data-project="${esc(project.id)}" data-service="${esc(service.id)}">查看日志</button></div>
  </article>`;
}

async function loadProjects() {
  try {
    projects = await api("/api/projects");
    const busyCount = projects.filter((project) => project.deployment_in_progress).length;
    $("#global").textContent = busyCount ? `${busyCount} 个项目部署中` : "空闲";
    $("#global").className = `pill ${busyCount ? "" : "ok"}`;
    $("#projects").innerHTML =
      projects
        .map(
          (project) => `<section class="project-block">
            <div class="project-head">
              <div><h3>${esc(project.id)}</h3><p class="muted">${esc(project.compose_files.join(", "))}</p></div>
              ${project.deployment_in_progress ? badge("部署中") : ""}
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
  $("#versions").innerHTML = '<tr><td colspan="4">正在读取…</td></tr>';
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
            <td>${version.updated_at ? new Date(version.updated_at).toLocaleString() : "—"}</td>
            <td><code>${esc(version.digest)}</code></td>
            <td><button class="deploy" data-version="${esc(version.version)}" ${project.deployment_in_progress || version.version === service.desired_version ? "disabled" : ""}>部署</button></td>
          </tr>`,
        )
        .join("") || '<tr><td colspan="4">没有符合规则的标签</td></tr>';
    document.querySelectorAll(".deploy").forEach((button) => {
      button.onclick = () => confirmDeploy(button.dataset.version);
    });
  } catch (error) {
    diagnose(`${projectId} / ${serviceId} 版本查询`, error.message);
    $("#versions").innerHTML = `<tr><td colspan="4" class="drift">${esc(error.message)}</td></tr>`;
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
            ? "部署成功"
            : `部署失败：${deployment.error_message || deployment.status}`,
        );
        return;
      }
    } catch (error) {
      diagnose(`部署任务 ${id}`, error.message);
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
            <td>${esc(deployment.previous_version)} → ${esc(deployment.target_version)}</td>
            <td>${badge(deployment.status)}</td><td>${elapsed(deployment)}</td>
            <td>${esc(deployment.rollback_status)}</td>
          </tr>`,
        )
        .join("") || '<tr><td colspan="6">暂无部署历史</td></tr>';
    document.querySelectorAll("#history tr[data-id]").forEach((row) => {
      row.onclick = () => showDetail(row.dataset.id);
    });
  } catch (error) {
    diagnose("部署历史", error.message);
    toast(error.message);
  }
}

async function showDetail(id) {
  try {
    const deployment = await api(`/api/deployments/${id}`);
    $("#detail-summary").textContent =
      `${deployment.project_id} / ${deployment.service_id}: ${deployment.previous_version || "未部署"} → ${deployment.target_version} · ${deployment.status}` +
      (deployment.error_message ? ` · ${deployment.error_message}` : "");
    $("#detail-log").textContent = deployment.command_log || "（无日志）";
    $("#detail").showModal();
  } catch (error) {
    diagnose(`部署详情 ${id}`, error.message);
    toast(error.message);
  }
}

async function showContainerLogs(projectId, serviceId) {
  $("#container-logs-summary").textContent =
    `${projectId} / ${serviceId} · 最近 200 行`;
  $("#container-logs-content").textContent = "正在读取…";
  $("#container-logs").showModal();
  try {
    const data = await api(
      `/api/projects/${encodeURIComponent(projectId)}/services/${encodeURIComponent(serviceId)}/logs`,
    );
    $("#container-logs-summary").textContent =
      `${projectId} / ${serviceId} · 最近 ${data.tail} 行`;
    $("#container-logs-content").textContent = data.logs || "（暂无日志）";
  } catch (error) {
    diagnose(`${projectId} / ${serviceId} 容器日志`, error.message);
    $("#container-logs-content").textContent = `读取失败：${error.message}`;
  }
}

$("#refresh").onclick = () =>
  selected &&
  selectService(selected.project.id, selected.service.id, true);
$("#deploy-latest").onclick = () =>
  latestVersion && confirmDeploy(latestVersion);
$("#history-refresh").onclick = loadHistory;
$("#diagnostics-clear").onclick = () => {
  diagnostics = [];
  $("#diagnostics").textContent = "（暂无诊断信息）";
};
loadProjects();
loadHistory();
setInterval(() => {
  loadProjects();
  loadHistory();
}, 10000);
