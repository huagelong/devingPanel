/* Browser prototype: credentials are not persisted; the screen-lock verifier is. */
"use strict";
const $ = (s, root = document) => root.querySelector(s);
const $$ = (s, root = document) => [...root.querySelectorAll(s)];
const esc = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
const paths = {
  lock: '<rect x="4" y="10" width="16" height="11" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/>',
  download: '<path d="M12 3v12m-5-5 5 5 5-5M4 16v5h16v-5"/>',
  upload: '<path d="M12 16V4m-5 5 5-5 5 5M4 16v5h16v-5"/>',
  cloud: '<path d="M6 18a4 4 0 0 1-1-8 7 7 0 0 1 13-2 5 5 0 0 1 0 10H6Z"/>',
  grid: '<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
  server:
    '<rect x="3" y="3" width="18" height="7" rx="2"/><rect x="3" y="14" width="18" height="7" rx="2"/><path d="M7 6.5h.01M7 17.5h.01M11 6.5h6M11 17.5h6"/>',
  folder:
    '<path d="M3 7V5a2 2 0 0 1 2-2h5l2 3h7a2 2 0 0 1 2 2v11a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/>',
  rocket:
    '<path d="M12 16 8 12c2-6 7-9 13-9 0 6-3 11-9 13ZM8 12H3l4-6h5M12 16v5l6-4v-5M5 16l-2 5 5-2"/><circle cx="16" cy="8" r="1.5"/>',
  box: '<path d="m12 3 9 5v9l-9 5-9-5V8l9-5ZM3 8l9 5 9-5M12 13v9M7.5 5.5l9 5"/>',
  settings:
    '<path d="m9 3-1 3-3 1-2 3 2 2-1 3 2 3 3-1 2 3h3l1-3 3-1 2-3-2-2 1-3-2-3-3 1-2-3H9Z"/><circle cx="11.5" cy="11.5" r="3"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  sun: '<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M5 5l1 1M18 18l1 1M5 19l1-1M18 6l1-1"/>',
  moon: '<path d="M20.9 13A9 9 0 0 1 11 3.1 9 9 0 1 0 20.9 13Z"/>',
  menu: '<path d="M4 6h16M4 12h16M4 18h16"/>',
  close: '<path d="m6 6 12 12M6 18 18 6"/>',
  refresh:
    '<path d="M20 11a8 8 0 0 0-14-5L3 9M3 3v6h6M4 13a8 8 0 0 0 14 5l3-3M15 15h6v6"/>',
  chevron: '<path d="m9 5 7 7-7 7"/>',
  up: '<path d="m6 14 6-6 6 6"/>',
  down: '<path d="m6 10 6 6 6-6"/>',
  trash: '<path d="M3 6h18M9 6V3h6v3M5 6l1 15h12l1-15M10 10v7M14 10v7"/>',
  check: '<path d="m5 12 4 4L19 6"/>',
  alert: '<path d="m12 3 10 18H2L12 3ZM12 9v5M12 17h.01"/>',
  terminal: '<path d="m4 6 6 6-6 6M12 18h8"/>',
  eye: '<path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/>',
  pencil: '<path d="M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  copy: '<rect x="8" y="8" width="13" height="13" rx="2"/><path d="M16 8V3H3v13h5"/>',
  shield:
    '<path d="m12 3 8 3v6c0 5-8 9-8 9s-8-4-8-9V6l8-3Z"/><path d="m8 12 3 3 5-6"/>',
};
const icon = (name) =>
  I18n.html`<svg class="icon" viewBox="0 0 24 24" aria-hidden="true">${paths[name] || paths.box}</svg>`;
const button = (label, action, variant = "", extra = "", localize = true) =>
  I18n.html`<button type="button" class="${variant}" data-action="${action}" ${I18n.t(extra)}>${localize ? I18n.t(label) : label}</button>`;
const ib = (name, label, action, extra = "") =>
  button(
    icon(name),
    action,
    "ghost icon-btn",
    `aria-label="${I18n.t(label)}" title="${I18n.t(label)}" ${extra}`,
  );
const badge = (label, tone = "muted") =>
  I18n.html`<span class="badge ${tone}"><span class="dot"></span>${esc(I18n.t(label))}</span>`;
const progress = (n, tone = "") =>
  I18n.html`<div class="progress ${tone}" role="progressbar" aria-valuenow="${n}" aria-valuemin="0" aria-valuemax="100" aria-label="进度"><span style="width:${n}%"></span></div>`;
const field = (label, control, hint = "") =>
  I18n.html`<label class="field">${I18n.t(label)}${control}${hint ? I18n.html`<small>${I18n.t(hint)}</small>` : ""}</label>`;
const input = (name, value = "", attrs = "") =>
  I18n.html`<input name="${name}" value="${esc(value)}" ${I18n.t(attrs)}>`;
const select = (name, values, selected = "", attrs = "") =>
  I18n.html`<select name="${name}" ${I18n.t(attrs)}>${values
    .map((v) => {
      const [value, text] = Array.isArray(v) ? v : [v, v];
      const userData =
        (name === "service" && value !== "") ||
        [
          "deploy-project",
          "container-host",
          "project-group",
          "language",
          "screen-language",
        ].includes(name) ||
        (name === "host-group" && value !== "all") ||
        (name === "env-scope" && value !== "common");
      return I18n.html`<option value="${esc(value)}" ${String(value) === String(selected) ? "selected" : ""}>${esc(userData ? text : I18n.t(text))}</option>`;
    })
    .join("")}</select>`;
const card = (title, description, content) =>
  I18n.html`<section class="card"><div class="card-head"><h2>${I18n.t(title)}</h2>${description ? I18n.html`<p class="muted">${I18n.t(description)}</p>` : ""}</div><div class="pad">${content}</div></section>`;
const table = (headers, rows) =>
  I18n.html`<div class="table-wrap"><table><thead><tr>${headers.map((h) => I18n.html`<th ${h === "操作" ? 'class="action-col"' : ""}>${I18n.t(h)}</th>`).join("")}</tr></thead><tbody>${rows.join("")}</tbody></table></div>`;
const row = (cells) =>
  I18n.html`<tr>${cells.map((c, i) => I18n.html`<td${i === cells.length - 1 && String(c).includes('class="actions"') ? ' class="action-col"' : ""}>${c?.i18nParts ? I18n.t(c) : c}</td>`).join("")}</tr>`;
const tabs = (items, active, action = "tab") =>
  I18n.html`<div class="tabs" role="tablist">${items.map(([id, label]) => button(label, `${action}:${id}`, "", `role="tab" aria-selected="${id === active}"`)).join("")}</div>`;
const menu = (items) =>
  I18n.html`<details class="menu"><summary aria-label="更多操作">${icon("more")}</summary><div class="menu-list">${items.join("")}</div></details>`;
const head = (title, actions = "", localize = true) =>
  I18n.html`<div class="page-head"><h1>${localize ? I18n.t(title) : title}</h1><div class="actions">${actions}</div></div>`;
const dataHead = (title, actions = "") => head(title, actions, false);
const go = (label, path, style = "", symbol = "") =>
  I18n.html`<a class="btn ${style}" href="#/${path}">${symbol ? icon(symbol) : ""}${I18n.t(label)}</a>`;
const empty = (text, label, action, symbol = "folder") =>
  I18n.html`<div class="empty">${icon(symbol)}<h2>${I18n.t(text)}</h2>${label ? button(label, action, "primary") : ""}</div>`;
const hosts = [
  {
    id: 1,
    name: "web-1",
    address: "203.0.113.10",
    user: "deploy",
    port: 22,
    group: "myapp-prod",
    online: true,
    cpu: 42,
    memory: 61,
    disk: 38,
    containers: 5,
    arch: "amd64",
  },
  {
    id: 2,
    name: "web-2",
    address: "203.0.113.11",
    user: "deploy",
    port: 22,
    group: "myapp-prod",
    online: true,
    cpu: 28,
    memory: 48,
    disk: 32,
    containers: 4,
    arch: "amd64",
  },
  {
    id: 3,
    name: "web-3",
    address: "203.0.113.12",
    user: "deploy",
    port: 22,
    group: "myapp-prod",
    online: false,
    cpu: 0,
    memory: 0,
    disk: 0,
    containers: 0,
    arch: "arm64",
  },
];
const hostGroups = [...new Set(hosts.map((host) => host.group).filter((group) => group && group !== "未分组"))];
function resetHostGroups() {
  hostGroups.splice(0, hostGroups.length, ...new Set(hosts.map((host) => host.group).filter((group) => group && group !== "未分组")));
}
const projects = [
  {
    id: 1,
    name: "myapp",
    path: "D:\\code\\myapp",
    dockerfile: "Dockerfile",
    image: "myapp",
    tag: "{git-sha}",
    platform: "auto",
    service: "web",
    hostIds: [1, 2, 3],
    files: ["docker-compose.yml", "docker-compose.prod.yml"],
    args: [["NODE_ENV", "production"]],
    env: [],
    ready: true,
  },
];
const history = [
  {
    id: 1,
    time: "2026-10-24 14:32",
    projectId: 1,
    project: "myapp",
    tag: "20261024-143200",
    result: "成功",
    tone: "success",
    duration: "1m 24s",
    targets: [1, 2],
    logs: ["[演示] 本地 Docker build 完成", "[演示] web-1 镜像传输完成", "[演示] web-1 Compose 启动成功", "[演示] web-1 HEALTHCHECK healthy", "[演示] web-2 镜像传输完成", "[演示] web-2 Compose 启动成功", "[演示] web-2 HEALTHCHECK healthy"],
  },
  {
    id: 2,
    time: "2026-10-24 11:08",
    projectId: 1,
    project: "myapp",
    tag: "20261024-110800",
    result: "失败",
    tone: "destructive",
    duration: "2m 12s",
    targets: [1, 2],
    logs: ["[演示] 本地 Docker build 完成", "[演示] web-1 镜像传输完成", "[演示] web-1 Compose 启动成功", "[演示] web-1 健康检查超过 120s 仍未就绪", "[演示] web-2 镜像传输完成", "[演示] web-2 Compose 启动成功", "[演示] web-2 健康检查超过 120s 仍未就绪", "[演示] 具体根因未记录，需查看远端容器日志"],
  },
  {
    id: 3,
    time: "2026-10-23 18:40",
    projectId: 1,
    project: "myapp",
    tag: "20261023-184000",
    result: "已启动，未验证就绪",
    tone: "warning",
    duration: "48s",
    targets: [1],
    logs: ["[演示] 本地 Docker build 完成", "[演示] web-1 镜像传输完成", "[演示] web-1 Compose 启动成功", "[演示] web-1 容器运行中，但没有有效的 HEALTHCHECK，无法确认服务就绪"],
  },
];
const containers = hosts.flatMap((h) => [
  {
    id: `${h.id}-1`,
    host: h.id,
    name: "myapp-web-1",
    image: "myapp:20261024-143200-" + h.arch,
    state: "running",
    ports: "0.0.0.0:8080 → 3000",
    cpu: "2%",
    memory: "120 MB",
  },
  {
    id: `${h.id}-2`,
    host: h.id,
    name: "myapp-redis-1",
    image: "redis:7-alpine",
    state: "running",
    ports: "127.0.0.1:6379 → 6379",
    cpu: "0.2%",
    memory: "18 MB",
  },
  {
    id: `${h.id}-3`,
    host: h.id,
    name: "old-worker",
    image: "worker:old",
    state: "exited",
    ports: "—",
    cpu: "—",
    memory: "—",
  },
]);
function containerActions(c) {
  return I18n.html`<div class="actions">${button("日志", `logs:${c.id}`, "ghost")}${menu([button("启动", `container-start:${c.id}`, "ghost", c.state === "running" ? "disabled" : ""), button("停止", `container-stop:${c.id}`, "ghost", c.state !== "running" ? "disabled" : ""), button("重启", `container-restart:${c.id}`, "ghost"), button("删除", `container-delete:${c.id}`, "ghost destructive")])}</div>`;
}
// History snapshots deliberately do not depend on later edits to host/project settings.
history.forEach((record) => {
  record.rows = record.targets.map((id) => {
    const h = hosts.find((h) => h.id === id);
    return {
      id,
      name: h.name,
      arch: h.arch,
      image: `myapp:${record.tag}-${h.arch}`,
      status: record.result,
      tone: record.tone,
      phase: record.tone === "destructive" ? "健康检查超时" : "完成",
      elapsed: "—",
      reason: record.tone === "destructive" ? "健康检查超过 120s 仍未就绪；具体根因未记录，需查看远端容器日志。" : "",
      materials: record.tone !== "destructive",
    };
  });
});
hosts.forEach((h) => {
  h.trustedAddress = h.address;
  h.trustedPort = h.port;
});
const state = {
  route: "dashboard",
  id: 0,
  tab: "",
  view: "normal",
  stage: "p1",
  group: "all",
  containerHost: 1,
  theme: "system",
  keep: 3,
  autoCleanup: false,
  hostCollectionIntervalSeconds: 60,
  hostCollectionCustom: false,
  alerts: { cpu: 90, memory: 90, disk: 90, duration: "5 分钟" },
  historyPage: 0,
  backupTab: "local",
  settingsSection: "retention",
  s3: {
    endpoint: "",
    region: "us-east-1",
    bucket: "",
    prefix: "devingpanel/",
    pathStyle: false,
  },
  docker: true,
  previewDockerMissing: false,
  previewMirrors: false,
  previewYamlProject: false,
  previewYamlError: false,
  deploy: null,
  selectedProject: 1,
  selectedHosts: [1, 2],
  parallel: 1,
  scenario: "success",
  registry: false,
  registryAddress: "",
  registryUser: "",
  registryDraftAddress: "",
  registryDraftUser: "",
  registryConfigured: false,
  imagesRemoved: [],
  follow: true,
};
let logTimer;
try {
  state.theme = localStorage.getItem("devingpanel-theme") || "system";
} catch {}
const navItems = [
  ["dashboard", "总览", "grid"],
  ["hosts", "主机", "server"],
  ["projects", "项目", "folder"],
  ["deploy", "发布", "rocket"],
  ["containers", "容器", "box"],
  ["settings", "设置", "settings"],
];
function settingsSections() {
  return [
    ["retention", "发布保留策略", "box"],
    ["appearance", "外观", "sun"],
    ["registry", "镜像仓库", "server"],
    ["backup", "备份与恢复", "cloud"],
    ...(state.stage === "p2" ? [["alerts", "告警", "alert"]] : []),
    ["environment", "本地环境", "terminal"],
    ["security", "安全", "shield"],
  ];
}
function theme(value) {
  state.theme = value;
  document.documentElement.classList.toggle(
    "dark",
    value === "dark" ||
      (value === "system" &&
        matchMedia("(prefers-color-scheme: dark)").matches),
  );
  try {
    localStorage.setItem("devingpanel-theme", value);
  } catch {}
}
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () =>
  theme(state.theme),
);
function toast(text, error = false) {
  const el = document.createElement("div");
  el.className = "toast" + (error ? " error" : "");
  el.textContent = I18n.t(text);
  $("#toasts").append(el);
  setTimeout(() => el.remove(), 5000);
}
function modal(
  title,
  content,
  footer = "",
  wide = false,
  sheet = false,
  localizeTitle = true,
) {
  BackupUI.cancelRead();
  const el = $(sheet ? "#sheet" : "#dialog");
  el.classList.toggle("wide", wide);
  el.classList.remove("host-dialog", "terminal-dialog");
  el.setAttribute("aria-labelledby", el.id + "-title");
  el.innerHTML = I18n.html`<div class="dialog-head"><h2 id="${el.id}-title">${localizeTitle ? I18n.t(title) : title}</h2>${ib("close", "关闭", "close")}</div>${content}${footer ? I18n.html`<div class="dialog-foot">${footer}</div>` : ""}`;
  if (!el.open) el.showModal();
}
function close() {
  // Invalidate the current operation synchronously; a delayed close event must
  // not cancel a newer file read started immediately after dismissal.
  if ($("#dialog").open) {
    BackupUI.onClose();
    ScreenLock.onDialogDismiss();
  }
  $$("#dialog[open],#sheet[open]").forEach((d) => d.close());
}
function confirmAction(title, message, action, label = "确认") {
  modal(
    title,
    I18n.html`<p>${I18n.t(message)}</p>`,
    button("取消", "close") + button(label, action, "danger"),
  );
}
function parseRoute() {
  close();
  BackupUI.cancelRead();
  const [route = "dashboard", id, tab] = location.hash
    .replace(/^#\/?/, "")
    .split("/");
  state.route = navItems.some((n) => n[0] === route) ? route : "dashboard";
  state.id = Number(id) || 0;
  if (state.route === "settings") state.settingsSection = id || "retention";
  state.tab = tab || "";
  state.view = "normal";
  state.historyPage = 0;
  render();
  $("#sidebar").classList.remove("open");
}
function dockerSetup() {
  return I18n.html`<section class="docker-setup"><div class="docker-setup-icon">${icon("box")}</div><h1>请先安装本机 Docker</h1><p>构建和发布依赖本机 Docker。安装并启动 Docker，检测可用后才能使用主机、项目、发布等功能。</p><div class="row wrap">${button("查看安装指引", "docker-setup", "primary")}${button("重新检测", "docker-detect")}${button("原型预览选项", "preview", "ghost")}</div><small>此网页原型不检测本机软件；可在“原型预览选项”中模拟 Docker 可用状态。</small></section>`;
}
function render() {
  $("#sidebar").setAttribute("aria-label", I18n.t("主导航"));
  let settingsLabel = "";
  if (state.route === "settings") {
    if (!settingsSections().some(([key]) => key === state.settingsSection))
      state.settingsSection = "retention";
    const canonical = `#/settings/${state.settingsSection}`;
    if (location.hash !== canonical)
      window.history.replaceState(null, "", canonical);
    settingsLabel = settingsSections().find(
      ([key]) => key === state.settingsSection,
    )[1];
  }
  const oldLog = $("#build-log");
  const oldScroll = oldLog?.scrollTop || 0;
  const followLog =
    !oldLog || oldLog.scrollHeight - oldScroll - oldLog.clientHeight < 16;
  const logOpen = oldLog?.closest("details").open;
  theme(state.theme);
  const isDark = document.documentElement.classList.contains("dark");
  const title = state.docker ? navItems.find((n) => n[0] === state.route)[1] : "安装 Docker";
  const entity = ["hosts", "containers"].includes(state.route)
    ? hosts.find((h) => h.id === state.id)
    : state.route === "projects"
      ? projects.find((p) => p.id === state.id)
      : null;
  const detailLabel = state.docker ? settingsLabel || (state.id && entity ? entity.name : "") : "";
  $("#sidebar").innerHTML =
    I18n.html`<a class="brand" href="#/dashboard"><svg class="brand-mark" viewBox="0 0 36 36" aria-hidden="true"><rect x="5" y="9" width="11" height="18" rx="2" fill="none" stroke="currentColor" stroke-width="2"/><path d="M8 14h5M8 18h5M8 22h4" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><path class="brand-transfer" d="M17.5 18h5m-2.5-2.5L22.5 18 20 20.5" fill="none" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/><rect x="24" y="8" width="8" height="20" rx="2" fill="none" stroke="currentColor" stroke-width="2"/><path d="M27 13h2" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle class="brand-transfer-dot" cx="28" cy="23" r="1.25"/></svg>devingPanel</a><nav>${navItems
      .filter((n) => state.stage !== "p0" || n[0] !== "containers")
      .map(
        ([id, label, symbol]) =>
          state.docker
            ? I18n.html`<a href="#/${id}" class="${id === state.route ? "active" : ""}" ${id === state.route ? 'aria-current="page"' : ""}>${icon(symbol)}${I18n.t(label)}${id === "deploy" && state.deploy?.running ? '<span class="dot info"></span>' : ""}</a>`
            : I18n.html`<span class="nav-disabled" aria-disabled="true">${icon(symbol)}${I18n.t(label)}</span>`,
      )
      .join(
        "",
      )}</nav><div class="sidebar-foot"><span class="row">${icon("shield")}仅 SSH · 无远端 Agent</span><p>交互原型 · 所有数值均为演示</p>${button("原型预览选项", "preview", "ghost")}<p>v0.1 · 本地构建，远端运行</p></div>`;
  $("#topbar").innerHTML =
    I18n.html`<div class="row">${ib("menu", "打开导航", "menu")}${state.docker ? I18n.html`<a class="muted" href="#/${state.route}">${I18n.t(title)}</a>` : I18n.html`<span class="muted">${I18n.t(title)}</span>`}${detailLabel ? I18n.html`${icon("chevron")}<span>${esc(settingsLabel ? I18n.t(detailLabel) : detailLabel)}</span>` : ""}</div><div class="top-actions"><span class="row"><span class="dot ${state.docker ? "success" : "destructive"}"></span><span class="docker-label">${state.docker ? I18n.t("本地 Docker 可用") : I18n.t("Docker 不可用")}</span></span>${ScreenLock.isEnabled() ? ib("lock", "立即锁屏", "screen-lock") : ""}${button(icon(isDark ? "sun" : "moon"), "theme", "ghost icon-btn", `aria-label="${I18n.t(isDark ? "切换至浅色" : "切换至深色")}" title="${I18n.t(isDark ? "切换至浅色" : "切换至深色")}"`)}</div>${state.deploy?.running ? I18n.t('<div class="global-progress" aria-label="发布进行中"></div>') : ""}`;
  $('#topbar [data-action="menu"]').classList.add("mobile-menu");
  if (!state.docker) {
    $("#content").innerHTML = dockerSetup();
    document.title = `${I18n.t(title)} · devingPanel`;
    return;
  }
  const pages = {
    dashboard,
    hosts: hostsPage,
    projects: projectsPage,
    deploy: deployPage,
    containers: containersPage,
    settings: settingsPage,
  };
  $("#content").innerHTML = pages[state.route]();
  if (state.view === "loading")
    $("#content").innerHTML =
      head(title) +
      I18n.html`<div class="grid hosts-grid">${Array.from({ length: 3 }, () => '<div class="card pad stack"><div class="skeleton"></div><div class="skeleton"></div></div>').join("")}</div><div class="card">${Array.from({ length: 5 }, () => '<div class="skeleton-row"><div class="skeleton"></div></div>').join("")}</div>`;
  if (state.view === "error")
    $("#content").innerHTML =
      head(title) +
      I18n.html`<div class="card">${empty("加载失败：SSH 连接超时，请检查网络与主机地址。", "重试", "retry", "alert")}</div>`;
  document.title = ScreenLock.isLocked
    ? `${I18n.t("已锁定")} · devingPanel`
    : `${settingsLabel ? I18n.t(settingsLabel) + " · " : ""}${I18n.t(title)} · devingPanel`;
  const newLog = $("#build-log");
  if (newLog) {
    if (oldLog && state.deploy?.running)
      newLog.closest("details").open = logOpen;
    newLog.scrollTop = followLog ? newLog.scrollHeight : oldScroll;
  }
}
function historyTable(records, compact = false, projectScoped = false) {
  if (!records.length)
    return I18n.t('<div class="pad muted">暂无发布记录</div>');
  return table(
    compact
      ? ["时间", "项目", "发布标识", "结果", "耗时"]
      : [
          "时间",
          ...(!projectScoped ? ["项目"] : []),
          "发布标识",
          projectScoped ? "目标主机数" : "目标主机",
          "结果",
          "耗时",
          "操作",
        ],
    records.map((r) =>
      row([
        esc(r.time),
        ...(!projectScoped ? [esc(r.project)] : []),
        I18n.html`<span class="mono">${esc(r.tag)}</span>`,
        ...(!compact ? [I18n.msg`${r.targets.length} 台`] : []),
        badge(r.result, r.tone),
        r.duration,
        ...(!compact
          ? [
              I18n.html`<div class="actions">${button("详情", `history:${r.id}`, "ghost")}${!projectScoped ? button("回滚", `rollback:${r.id}`, "", state.keep === 1 ? I18n.t('disabled title="仅保留当前版本，无法回滚"') : "") : ""}</div>`,
            ]
          : []),
      ]),
    ),
  );
}
function dashboard() {
  const items = state.view === "empty" ? [] : hosts;
  return (
    head("总览", go("去发布", "deploy", "primary")) +
    (items.length
      ? I18n.html`<div class="grid hosts-grid ${items.length > 8 ? "scroll-limited" : ""}">${items.map((h) => I18n.html`<a href="#/hosts/${h.id}" class="card compact host-card"><div class="row between"><div class="row"><span class="dot ${h.online ? "success" : "destructive"}"></span><strong>${esc(h.name)}</strong></div><span class="mono muted">${esc(h.address)}</span></div>${state.stage !== "p0" ? I18n.html`<div class="grid two" style="margin-top:16px">${["cpu", "memory"].map((key, i) => I18n.html`<div><div class="row between"><small>${i ? I18n.t("内存") : "CPU"}</small><span>${h.online ? h[key] + "%" : "--"}</span></div>${h.online ? progress(h[key]) : ""}</div>`).join("")}</div><p class="row between muted"><span>容器 ${h.online ? h.containers + I18n.t(" 运行") : "--"}</span><span>磁盘 ${h.online ? h.disk + "%" : "--"}</span></p>` : I18n.html`<p class="muted">Docker ${h.online ? I18n.t("已连接") : I18n.t("不可达")}</p>`}</a>`).join("")}</div>`
      : I18n.html`<section class="card">${empty("添加你的第一台 VPS", "添加主机", "host-new", "server")}</section>`) +
    I18n.html`<section class="card"><div class="card-head"><h2>最近发布</h2><p class="muted">最近 5 条发布记录</p></div>${historyTable(state.view === "empty" ? [] : history.slice(0, 5), true)}<div class="table-foot">${go("查看全部", "deploy/0/history", "ghost")}</div></section>`
  );
}
function hostsPage() {
  if (state.id) return hostDetail();
  const items =
    state.view === "empty"
      ? []
      : hosts.filter((h) => state.group === "all" || h.group === state.group);
  return (
    head("主机", button(icon("plus") + "添加主机", "host-new", "primary")) +
    I18n.html`<div class="row wrap">${select("host-group", [["all", "全部分组"], ...hostGroups, ...(hosts.some((host) => host.group === "未分组") ? ["未分组"] : [])], state.group, 'class="inline-select" aria-label="主机分组"')}${button(icon("settings") + I18n.t("管理分组"), "host-groups", "", "", false)}</div><section class="card">${
      items.length
        ? table(
            [
              "状态",
              "别名",
              "地址",
              "用户",
              "分组",
              ...(state.stage !== "p0" ? ["容器数"] : []),
              "CPU",
              "内存",
              "磁盘",
              "最近连接",
              "操作",
            ],
            items.map((h) => {
              const missing = state.previewDockerMissing && h.id === 1;
              return row([
                badge(
                  missing ? I18n.t("Docker 未安装") : h.online ? I18n.t("在线") : I18n.t("离线"),
                  missing ? "warning" : h.online ? "success" : "destructive",
                ),
                I18n.html`<a href="#/hosts/${h.id}"><strong>${esc(h.name)}</strong></a>`,
                I18n.html`<span class="mono">${esc(h.address)}:${h.port}</span>`,
                esc(h.user),
                esc(h.group),
                ...(state.stage !== "p0"
                  ? [h.online && !missing ? h.containers : "--"]
                  : []),
                ...["cpu", "memory", "disk"].map((key) =>
                  h.online && !missing ? `${h[key]}%` : "--",
                ),
                missing ? I18n.t("SSH 已连接") : h.online ? I18n.t("2 分钟前") : I18n.t("连接超时"),
                I18n.html`<div class="actions">${ib("refresh", "测试连接", `host-test:${h.id}`)}${menu([go("详情", `hosts/${h.id}`, "ghost", "eye"), button(icon("terminal") + I18n.t("连接"), `host-connect:${h.id}`, "ghost", "", false), button(icon("pencil") + I18n.t("编辑"), `host-edit:${h.id}`, "ghost", "", false), button(icon("copy") + I18n.t("复制"), `host-copy:${h.id}`, "ghost", "", false), button(icon("trash") + I18n.t("删除"), `host-delete:${h.id}`, "ghost destructive", "", false)])}</div>`,
              ]);
            }),
          )
        : empty("还没有主机", "添加主机", "host-new", "server")
    }</section>`
  );
}
function hostDetail() {
  const h = hosts.find((h) => h.id === state.id);
  if (!h) return empty("主机不存在", "返回主机列表", "nav:hosts", "server");
  const tab = state.stage === "p0" ? "info" : state.tab || "monitor";
  const missingDocker = state.previewDockerMissing && h.id === 1;
  let body;
  if (tab === "info")
    body = card(
      "基本信息",
      "SSH 连接配置与远端环境",
      I18n.html`<dl class="info-list">${[
        ["地址", h.address],
        ["SSH 端口", h.port],
        ["登录用户", h.user],
        ["认证方式", I18n.t("私钥（已安全保存 · 演示）")],
        ["分组", h.group],
        ["系统 / 内核", "Ubuntu 24.04 / Linux 6.8"],
        ["Docker 版本", "27.3.1 / Compose v2.29"],
        ["架构", "linux/" + h.arch],
        ["Agent", I18n.t("无")],
      ]
        .map(
          ([k, v]) =>
            I18n.html`<div><dt>${I18n.t(k)}</dt><dd>${esc(v)}</dd></div>`,
        )
        .join("")}</dl>${missingDocker ? I18n.html`<div class="notice warning stack"><strong>SSH 已连接 · Docker 未安装</strong><p>国内推荐腾讯云 Docker CE apt 镜像；安装前需确认仓库签名，完成后重新测试。</p>${button("一键安装 Docker", "host-docker-install", "primary")}</div>` : ""}${state.previewMirrors && h.id === 1 && !missingDocker ? I18n.html`<div class="notice info stack"><strong>Docker Hub 镜像源</strong><p>已实测候选：1Panel、DaoCloud。配置时保留其他 daemon 设置；会重启 Docker，运行容器不为空则阻断。</p>${button("配置镜像源", "host-mirror-config")}</div>` : ""}`,
    );
  else if (missingDocker)
    body = I18n.html`<div class="card">${empty("Docker 未安装", "查看安装引导", "tab:info", "alert")}</div>`;
  else if (!h.online)
    body = I18n.html`<div class="card">${empty("连接已断开", "重连", `host-test:${h.id}`, "alert")}</div>`;
  else
    body = I18n.html`<div class="grid metrics">${[
      ["CPU", h.cpu + "%", h.cpu],
      ["内存", "2.1 / 3.7 GB", h.memory],
      ["磁盘", h.disk + "%", h.disk],
      ["网络", "↑ 1.2 MB/s", null],
    ]
      .map(
        ([label, value, p]) =>
          I18n.html`<div class="card compact"><h3 class="muted">${I18n.t(label)}</h3><div class="stat ${p > 90 ? "destructive" : p > 80 ? "warning" : ""}">${value}</div>${p !== null ? progress(p) : "<small>↓ 300 KB/s</small>"}</div>`,
      )
      .join(
        "",
      )}</div>${state.stage === "p2" ? card("资源趋势", "近 10 分钟 · 演示数据，仅客户端在线期间采样", I18n.html`<svg class="chart" viewBox="0 0 600 180" role="img" aria-label="CPU 使用率趋势"><line x1="0" y1="40" x2="600" y2="40"/><line x1="0" y1="90" x2="600" y2="90"/><line x1="0" y1="140" x2="600" y2="140"/><polyline points="0,125 50,115 100,128 150,75 200,90 250,65 300,104 350,83 400,95 450,48 500,70 550,60 600,78"/></svg><div class="row between muted"><small>10 分钟前</small><small>CPU · 现在</small></div>`) : ""}<section class="card"><div class="card-head row between"><h2>运行的容器</h2><small>每 5s 刷新 · 原型不实际采样</small></div>${table(
      ["名称", "镜像", "状态", "CPU", "内存", "操作"],
      containers
        .filter((c) => c.host === h.id && c.state === "running")
        .map((c) =>
          row([
            I18n.html`<a href="#/containers/${h.id}">${c.name}</a>`,
            I18n.html`<span class="mono">${c.image}</span>`,
            badge("running", "success"),
            c.cpu,
            c.memory,
            containerActions(c),
          ]),
        ),
    )}</section>`;
  return (
    dataHead(
      esc(h.name),
      button(icon("terminal") + I18n.t("连接"), `host-connect:${h.id}`, "", "", false) + button("编辑", `host-edit:${h.id}`) +
        (state.stage !== "p0" ? go("打开容器管理", `containers/${h.id}`) + go("镜像管理", `containers/${h.id}/images`) : ""),
    ) +
    tabs(
      [
        ...(state.stage !== "p0" ? [["monitor", "资源监控"]] : []),
        ["info", "基本信息"],
      ],
      tab,
    ) +
    body
  );
}
function project() {
  return (
    projects.find((p) => p.id === state.id) ||
    projects.find((p) => p.id === state.selectedProject)
  );
}
function projectsPage() {
  if (state.id) return projectDetail();
  return (
    head("项目", button("导入已配置项目", "project-import") + button(icon("plus") + "新建项目", "project-new", "primary")) +
    I18n.html`<section class="card">${
      state.view === "empty" || !projects.length
        ? empty("还没有项目", "新建项目", "project-new")
        : table(
            ["项目名", "本地路径", "绑定主机数", "最近发布", "状态", "操作"],
            projects.map((p) =>
              row([
                I18n.html`<a href="#/projects/${p.id}"><strong>${esc(p.name)}</strong></a>`,
                I18n.html`<span class="mono truncate" title="${esc(p.path)}">${esc(p.path)}</span>`,
                p.hostIds.length,
                p.ready ? I18n.t("2 小时前 · 20261024-143200") : "—",
                badge(
                  p.ready ? I18n.t("就绪") : p.manifest && p.manifest.targets.some((t) => hosts.filter((h) => h.group === t.group && h.name === t.host).length !== 1) ? I18n.t("需管理员处理") : I18n.t("待完善"),
                  p.ready ? "success" : "warning",
                ),
                I18n.html`<div class="actions">${button("发布", `project-deploy:${p.id}`, "primary", !projectReady(p) ? I18n.t('disabled title="请完善 Compose、部署服务和目标主机"') : "")}${menu([go("详情", `projects/${p.id}`, "ghost"), ...(p.manifest ? [] : [button("编辑", `project-edit:${p.id}`, "ghost")]), button("删除", `project-delete:${p.id}`, "ghost destructive")])}</div>`,
              ]),
            ),
          )
    }</section>`
  );
}
function kvRows(values = [], env = false) {
  return I18n.html`<div class="kv-list">${values.map(([k, v]) => I18n.html`<div class="kv-row">${input("key", k, 'placeholder="KEY" aria-label="变量名" class="mono"')}${input("value", v, I18n.msg`placeholder="VALUE" aria-label="变量值" ${env ? 'type="password"' : ""}`)}${env ? I18n.html`${ib("eye", "切换掩码", "reveal")}<label><input type="checkbox" name="sensitive" checked>敏感</label>` : ""}${ib("trash", "删除行", "remove-row")}</div>`).join("")}</div>${button(icon("plus") + "添加变量", env ? "env-add" : "arg-add", "ghost")}`;
}
function manifestFor(p) {
  if (!p.manifest) p.manifest = {
    tag: p.tag,
    builds: [{ service: p.service || "web", dockerfile: p.dockerfile, image: p.image, platform: p.platform, args: structuredClone(p.args) }],
    files: [...p.files],
    targets: p.hostIds.map((id) => {
      const h = hosts.find((item) => item.id === id);
      return { group: h?.group || "", host: h?.name || "" };
    }),
  };
  if (!p.manifestDraft) p.manifestDraft = structuredClone(p.manifest);
  return p.manifestDraft;
}
function useManifest(p, loaded, directory) {
  p.directoryHandle = directory;
  p.manifestSource = loaded.source;
  p.manifestName = loaded.name;
  p.name = loaded.name;
  p.manifest = loaded.manifest;
  p.manifestDraft = structuredClone(loaded.manifest);
  p.hostIds = loaded.manifest.targets.flatMap((target) => {
    const match = hosts.filter((h) => h.group === target.group && h.name === target.host);
    return match.length === 1 ? [match[0].id] : [];
  });
  p.files = [...loaded.manifest.files];
  const first = loaded.manifest.builds[0];
  Object.assign(p, { tag: loaded.manifest.tag, image: first.image, service: first.service, dockerfile: first.dockerfile, platform: first.platform, args: structuredClone(first.args) });
  updateProjectReadiness(p);
}
function manifestEditor(p, tab) {
  const m = manifestFor(p);
  const save = button("保存到 devingpanel.yaml", "manifest-save", "primary", !p.directoryHandle ? 'disabled title="请先选择项目目录"' : "");
  if (tab === "build") return card("构建配置", "多个服务共用项目根目录作为构建上下文。", I18n.html`<div class="stack">${field("项目名", input("manifest-name", p.name, 'required'))}${field("Tag 规则", input("manifest-tag", m.tag, 'class="mono"'))}${m.builds.map((b, i) => I18n.html`<div class="manifest-item"><div class="row between"><h3>${I18n.t(I18n.msg`构建 ${i + 1}`)}</h3><div class="actions">${ib("up", "上移", `manifest-build-up:${i}`, i === 0 ? "disabled" : "")}${ib("down", "下移", `manifest-build-down:${i}`, i === m.builds.length - 1 ? "disabled" : "")}${ib("trash", "删除构建", `manifest-build-remove:${i}`, m.builds.length === 1 ? "disabled" : "")}</div></div><div class="form-grid">${field("构建服务", input(`manifest-service-${i}`, b.service, 'class="mono"'))}${field("Dockerfile", input(`manifest-dockerfile-${i}`, b.dockerfile, 'class="mono"'))}${field("镜像名", input(`manifest-image-${i}`, b.image, 'class="mono"'))}${field("目标平台", select(`manifest-platform-${i}`, [["auto", "按主机自动（默认）"], "linux/amd64", "linux/arm64"], b.platform))}</div><div><h3>Build args</h3>${(b.args || []).map(([key, value], j) => I18n.html`<div class="kv-row">${input(`manifest-arg-key-${i}-${j}`, key, 'placeholder="KEY" class="mono"')}${input(`manifest-arg-value-${i}-${j}`, value, 'placeholder="VALUE"')}${ib("trash", "删除行", `manifest-arg-remove:${i}:${j}`)}</div>`).join("")}${button(icon("plus") + "添加参数", `manifest-arg-add:${i}`)}</div></div>`).join("")}${button(icon("plus") + "添加构建", "manifest-build-add")}<div class="notice muted">Build args 以明文写入项目文件，不要填写密码或令牌。</div>${save}</div>`);
  if (tab === "compose") return card("Compose", "按列表顺序合并；首个文件必须位于项目根目录。", I18n.html`<div class="stack">${m.files.map((file, i) => I18n.html`<div class="file-row"><span>${i + 1}.</span>${input(`manifest-file-${i}`, file, 'class="mono grow"')}${ib("up", "上移", `manifest-file-up:${i}`, i === 0 ? "disabled" : "")}${ib("down", "下移", `manifest-file-down:${i}`, i === m.files.length - 1 ? "disabled" : "")}${ib("trash", "移除", `manifest-file-remove:${i}`)}</div>`).join("")}${button(icon("plus") + "添加 Compose 文件", "manifest-file-add")}<div class="notice muted">每个服务在 Compose 中使用自己的 DEVINGPANEL_IMAGE_服务名变量；保存前会读取并校验项目文件。</div>${save}</div>`);
  if (tab === "targets") return card("目标主机", "按 YAML 顺序发布；组名和别名须在本机唯一匹配。", I18n.html`<div class="stack">${m.targets.map((target, i) => I18n.html`<div class="manifest-target"><span>${i + 1}.</span>${field("主机组", input(`manifest-group-${i}`, target.group))}${field("主机别名", input(`manifest-host-${i}`, target.host))}${ib("up", "上移", `manifest-target-up:${i}`, i === 0 ? "disabled" : "")}${ib("down", "下移", `manifest-target-down:${i}`, i === m.targets.length - 1 ? "disabled" : "")}${ib("trash", "移除", `manifest-target-remove:${i}`)}</div>`).join("")}${button(icon("plus") + "添加目标", "manifest-target-add")}${m.targets.some((t) => hosts.filter((h) => h.group === t.group && h.name === t.host).length !== 1) || state.previewYamlError ? I18n.t('<div class="notice warning">目标主机缺失或名称不唯一；需管理员先配置主机和可信指纹，发布保持禁用。</div>') : ""}<div class="notice muted">变更目标绑定可能清理取消绑定主机的本地专属变量，保存前需确认。保存会写入项目文件，不修改主机配置。</div>${save}</div>`);
  return "";
}
function projectDetail() {
  const p = projects.find((p) => p.id === state.id);
  if (!p) return empty("项目不存在", "返回项目列表", "nav:projects");
  const tab = state.tab || "build";
  const yamlMode = !!p.manifest || (state.previewYamlProject && p.id === 1);
  let body = "";
  if (tab === "build")
    body = card(
      "构建配置",
      "镜像在开发机构建，不占用 VPS 编译资源。",
      I18n.html`<form id="build-form" class="stack">${p.restoreNeedsReview ? I18n.t('<div class="notice warning">该项目来自配置备份。请核对本地路径和 Compose 文件、补齐构建参数值，再保存确认；连接凭据和环境变量需重新配置。</div>') : ""}<div class="form-grid">${field("镜像名", input("image", p.image, 'required class="mono"'))}${field("Tag 规则", input("tag", "{git-sha}", 'readonly class="mono"'))}${field("目标平台", select("platform", [["auto", "按主机自动（默认）"], "linux/amd64", "linux/arm64"], p.platform))}${field("部署服务", select("service", [["", "请选择部署服务"], ...(p.files.length ? [["web", "web"]] : [])], p.service))}</div><div><h3>Build args</h3><p class="muted">构建参数不是秘密传输机制，请勿填写密码或令牌。</p>${kvRows(p.args)}</div>${button("保存", "build-save", "primary")}</form>`,
    );
  if (tab === "compose")
    body = card(
      "Compose",
      "支持多个 compose 文件，按顺序合并（后者覆盖前者）。",
      I18n.html`<div class="stack"><div>${p.files.map((f, i) => I18n.html`<div class="file-row"><span class="muted">${i + 1}.</span><span class="mono grow">${esc(f)}</span>${ib("up", "上移", `file-up:${i}`, i === 0 ? "disabled" : "")}${ib("down", "下移", `file-down:${i}`, i === p.files.length - 1 ? "disabled" : "")}${ib("trash", "移除", `file-remove:${i}`)}</div>`).join("")}</div>${button(icon("plus") + "添加 compose 文件", "file-add")}${composeIssue(p) ? I18n.html`<div class="notice warning">${I18n.t(composeIssue(p))}</div>` : ""}<details><summary>查看部署配置摘要</summary><div class="notice mono">服务 web · 镜像 \${DEVINGPANEL_IMAGE}<br>端口 8080:3000 · 卷 app-data:/data<br>环境变量：已脱敏（演示摘要，非文件解析结果）</div></details></div>`,
    );
  if (tab === "env")
    body = card(
      "环境变量",
      "主机专属值覆盖项目通用值；敏感值不写入 SQLite。",
      I18n.html`<form id="env-form" class="stack">${field("配置范围", select("env-scope", [["common", "项目通用"], ...p.hostIds.map((id) => [id, hosts.find((h) => h.id === id)?.name || id])], "common"))}<div id="env-rows">${kvRows([], true)}</div>${button("保存", "env-save", "primary")}</form>`,
    );
  if (tab === "targets")
    body = card(
      "目标主机",
      "只能选择同一主机组内的主机。",
      I18n.html`<div class="stack">${field("主机组", select("project-group", [...new Set(hosts.map((h) => h.group))], hosts.find((h) => p.hostIds.includes(h.id))?.group || hosts[0]?.group))}<div id="target-list">${targetTable(p.hostIds)}</div>${button("保存", "targets-save", "primary")}</div>`,
    );
  if (tab === "history") {
    const records = history.filter((h) => h.projectId === p.id);
    const lastPage = Math.max(0, Math.ceil(records.length / 20) - 1);
    state.historyPage = Math.min(state.historyPage, lastPage);
    body = I18n.html`<section class="card">${!records.length ? I18n.html`<div class="pad">${button("填充演示记录", "history-demo", "ghost")}</div>` : ""}${historyTable(
      records.slice(state.historyPage * 20, (state.historyPage + 1) * 20),
      false,
      true,
    )}<div class="table-foot"><small>每页 20 条 · 共 ${records.length} 条 · 第 ${state.historyPage + 1} / ${lastPage + 1} 页</small><div class="actions">${button("上一页", "history-prev", "", state.historyPage === 0 ? "disabled" : "")}${button("下一页", "history-next", "", state.historyPage === lastPage ? "disabled" : "")}</div></div></section>`;
  }
  if (yamlMode && ["build", "compose", "targets"].includes(tab)) body = manifestEditor(p, tab);
  return (
    dataHead(
      esc(p.name),
      button(
        "发布",
        `project-deploy:${p.id}`,
        "primary",
        !projectReady(p) ? 'disabled title="请保存并核对项目配置"' : "",
      ),
    ) +
    (yamlMode ? card(
      "devingpanel.yaml",
      "开发者维护的非秘密发布方案",
      I18n.html`<div class="stack"><p>${p.directoryHandle ? I18n.t("已读取项目目录中的 devingpanel.yaml") : I18n.t("尚未授权项目目录")}</p>${button("选择项目目录", "manifest-connect")}${button("重新读取配置", "manifest-reload", "", !p.directoryHandle ? "disabled" : "")}${manifestFor(p).builds.length > 1 ? I18n.t('<div class="notice warning">多构建页面仅供流程评审；当前原型发布动画只模拟单构建，开始发布已禁用。</div>') : ""}<small class="muted">保存会写入所选项目目录的 devingpanel.yaml；授权仅在当前页面会话有效。请勿在未核对文件内容时确认覆盖。</small></div>`,
    ) : "") +
    tabs(
      [
        ["build", "构建配置"],
        ["compose", "Compose"],
        ["env", "环境变量"],
        ["targets", "目标主机"],
        ["history", "发布历史"],
      ],
      tab,
    ) +
    body
  );
}
function targetTable(ids, deploy = false) {
  const p = project();
  const group =
    hosts.find((h) => p?.hostIds.includes(h.id))?.group || hosts[0]?.group;
  const source = deploy
    ? hosts.filter((h) => p?.hostIds.includes(h.id))
    : hosts.filter((h) => h.group === group);
  const sorted = [...source].sort(
    (a, b) =>
      (ids.includes(a.id) ? ids.indexOf(a.id) : 999) -
      (ids.includes(b.id) ? ids.indexOf(b.id) : 999),
  );
  return table(
    ["顺序", "选择", "状态", "主机名", "地址", "分组"],
    sorted.map((h, i) =>
      row([
        I18n.html`<span>${i + 1}</span>`,
        I18n.html`<input type="checkbox" name="target" value="${h.id}" ${ids.includes(h.id) ? "checked" : ""} aria-label="选择 ${esc(h.name)}">`,
        badge(h.online ? "在线" : "离线", h.online ? "success" : "destructive"),
        esc(h.name),
        I18n.html`<span class="mono">${esc(h.address)}</span>`,
        esc(h.group),
      ]),
    ),
  );
}
function deployPage() {
  const tab = state.tab || "execute";
  if (tab === "history")
    return (
      head("发布") +
      tabs(
        [
          ["execute", "执行"],
          ["history", "历史"],
        ],
        tab,
      ) +
      I18n.html`<section class="card">${historyTable(state.view === "empty" ? [] : history)}</section>`
    );
  if (!projects.length || state.view === "empty")
    return (
      head("发布") +
      I18n.html`<div class="card">${empty("先创建项目", "新建项目", "project-new")}</div>`
    );
  const d = state.deploy;
  const locked = deployLocked();
  const p = projects.find((p) => p.id === state.selectedProject) || projects[0];
  const configIds = locked ? d.ids : selectedDeployHosts(p);
  if (!locked) state.parallel = Math.min(state.parallel, configIds.length || 1);
  const ids = d ? d.ids : configIds;
  return (
    head("发布") +
    tabs(
      [
        ["execute", "执行"],
        ["history", "历史"],
      ],
      tab,
    ) +
    I18n.html`<section class="card pad"><div class="config-bar">${field(
      "项目",
      select(
        "deploy-project",
        projects.map((p) => [p.id, p.name]),
        p.id,
        locked ? "disabled" : "",
      ),
    )}${field("发布标识", I18n.html`<output id="release-tag" class="release-tag-value mono">${esc(locked ? d.tag : releaseTag(p))}</output>`)}<div><p class="muted">目标主机</p><div class="row"><span>已选 ${configIds.length} 台</span>${button("更改", "deploy-targets", "link", locked ? "disabled" : "")}</div></div>${field(
      "并行度",
      select(
        "parallel",
        Array.from({ length: Math.max(1, configIds.length) }, (_, i) =>
          String(i + 1),
        ),
        state.parallel,
        locked ? "disabled" : "",
      ),
    )}${button(locked ? I18n.t("终止") : I18n.t("开始发布"), locked ? "deploy-stop" : "deploy-start", locked ? "danger" : "primary", !locked && (!projectReady(p) || !configIds.length || !state.docker) ? I18n.t('disabled title="需要项目配置完整、至少一台目标主机且 Docker 可用"') : "")}</div></section><section class="card"><div class="card-head row between"><h2>目标主机</h2>${d ? badge(d.running ? I18n.t("进行中") : d.paused ? I18n.t("已暂停") : d.result, d.running ? "info" : d.tone || "muted") : ""}</div>${d?.message ? I18n.html`<p class="release-message ${d.tone || ""}">${esc(I18n.t(String(d.message)))}</p>` : ""}${table(
      ["顺序", "主机", "状态", "阶段", "进度", "耗时", "操作"],
      ids.map((id, i) => {
        const h = hosts.find((h) => h.id === id);
        const r = d?.rows[i] || {
          status: "等待",
          tone: "muted",
          phase: "—",
          percent: 0,
        };
        return row([
          i + 1,
          esc(r.name || h?.name || "已删除"),
          badge(r.status, r.tone),
          I18n.t(r.phase),
          r.status === "传输中"
            ? I18n.html`${progress(r.percent, "info")}<small>${r.percent}%</small>`
            : "—",
          r.elapsed || "—",
          I18n.html`<div class="actions">${r.status === "失败" ? button("查看原因", `deploy-reason:${i}`, "ghost") + (!d.stopping ? button("重试该主机", `deploy-retry:${i}`) + button("跳过此台继续", `deploy-skip:${i}`) + button("终止", "deploy-stop", "ghost destructive") : "") : r.status === "待核实" ? button("重新核实状态", `deploy-verify:${i}`) : "—"}</div>`,
        ]);
      }),
    )}</section><section class="card pad"><details ${!d || d.running || d.paused ? "open" : ""}><summary>构建日志</summary><div class="row between"><small>演示日志 · 不包含环境变量值</small>${button(icon("copy") + "复制日志", "copy-log", "ghost")}</div><pre class="log" id="build-log" translate="no">${esc(d?.logs.join("\n") || I18n.t("等待开始发布…"))}</pre></details></section>`
  );
}
function containersPage() {
  const id = state.id || state.containerHost;
  state.containerHost = id;
  const h = hosts.find((h) => h.id === id);
  const tab = state.tab || "containers";
  const prefix =
    head("容器", button("清理磁盘", "prune")) +
    I18n.html`<div class="container-toolbar">${field(
      "主机",
      select(
        "container-host",
        hosts.map((h) => [h.id, h.name]),
        id,
      ),
    )}<small>每 10s 刷新 · 原型使用演示数据</small></div>` +
    tabs(
      [
        ["containers", "容器"],
        ["images", "镜像"],
      ],
      tab,
    );
  if (!h || state.view === "empty")
    return (
      prefix +
      I18n.html`<div class="card">${empty("暂无容器或镜像", "", "", "box")}</div>`
    );
  if (!h.online)
    return (
      prefix +
      I18n.html`<div class="card">${empty("主机 SSH 不可达", "重试", `host-test:${h.id}`, "alert")}</div>`
    );
  if (tab === "images")
    return (
      prefix +
      I18n.html`<section class="card"><div class="card-head row between"><h2>镜像</h2>${button("清理悬空镜像", "image-prune", "ghost", 'disabled title="需先接入发布历史保护，避免删除回滚镜像"')}</div><div class="notice muted">清理悬空镜像需先接入发布历史保护；无标签镜像仍可能用于回滚。</div>${table(
        ["仓库名:tag", "大小", "创建时间", "未使用标记", "操作"],
        [
          [
            "myapp:20261024-143200-" + h.arch,
            "182 MB",
            "2 小时前",
            "当前运行，受保护",
          ],
          [
            "myapp:20261023-184000-" + h.arch,
            "180 MB",
            "1 天前",
            "保留版本，受保护",
          ],
          ["worker:old", "96 MB", "7 天前", "未使用"],
        ]
          .filter((v) => !state.imagesRemoved.includes(id + v[0]))
          .map(([name, size, time, used]) =>
            row([
              I18n.html`<span class="mono">${name}</span>`,
              size,
              I18n.t(time),
              badge(used, used === "未使用" ? "warning" : "muted"),
              I18n.html`<div class="actions">${button("删除", `image-delete:${name}`, "ghost destructive", used === "未使用" ? "" : I18n.t('disabled title="受发布保留策略保护"'))}</div>`,
            ]),
          ),
      )}</section>`
    );
  return (
    prefix +
    I18n.html`<section class="card">${table(
      ["状态", "名称", "镜像", "端口", "CPU", "内存", "操作"],
      containers
        .filter((c) => c.host === id)
        .map((c) =>
          row([
            badge(
              c.state === "running" ? "Up 3d" : "exited",
              c.state === "running" ? "success" : "destructive",
            ),
            button(esc(c.name), `logs:${c.id}`, "link mono", "", false),
            I18n.html`<span class="mono">${c.image}</span>`,
            I18n.html`<span class="mono">${c.ports}</span>`,
            c.cpu,
            c.memory,
            containerActions(c),
          ]),
        ),
    )}</section>`
  );
}
function settingsPage() {
  const views = {
    retention: () =>
      card(
        "发布保留策略",
        "按每项目 / 每主机计算已完成版本，保护当前运行与可回滚版本。",
        I18n.html`<div class="stack">${field("已完成版本保留数", select("retention", ["1", "2", "3", "5", "10", ["custom", "自定义"]], ["1", "2", "3", "5", "10"].includes(String(state.keep)) ? state.keep : "custom"))}${!["1", "2", "3", "5", "10"].includes(String(state.keep)) ? field("自定义数量", input("retention-custom", state.keep, 'type="number" min="1" required')) : ""}${state.keep === 1 ? I18n.t('<div class="notice warning">仅保留当前版本，该主机无法回滚到上一版本。</div>') : ""}<label class="row"><input type="checkbox" name="auto-cleanup" ${state.autoCleanup ? "checked" : ""}>自动清理策略（演示设置）</label><p class="muted">${state.autoCleanup ? I18n.t("本原型不运行定时清理；桌面客户端在线时每日检查。") : I18n.t("自动清理已关闭；可预览会话内历史材料。")}</p>${button("立即清理托管的历史镜像", "managed-clean")}</div>`,
      ),
    appearance: () =>
      card(
        "外观",
        "与顶部栏主题切换同步。",
        I18n.html`<div class="stack"><div class="row wrap" role="radiogroup" aria-label="主题">${[
          ["light", "浅色"],
          ["dark", "深色"],
          ["system", "跟随系统"],
        ]
          .map(
            ([v, label]) =>
              I18n.html`<label class="row"><input type="radio" name="theme" value="${v}" ${state.theme === v ? "checked" : ""}>${I18n.t(label)}</label>`,
          )
          .join("")}</div>${field(
          "语言",
          select(
            "language",
            [
              ["zh-CN", "简体中文"],
              ["en", "English"],
            ],
            I18n.locale,
          ),
          "语言仅影响界面；项目数据和日志保持原文。",
        )}</div>`,
      ),
    registry: () =>
      card(
        "镜像仓库（可选）",
        "未启用时演示 SSH 直连；启用后演示 push + 远端 pull。",
        I18n.html`<div class="stack">${field("Registry 地址", input("registry", state.registryDraftAddress, 'placeholder="registry.example.com" class="mono"'))}<div class="form-grid">${field("用户名", input("registry-user", state.registryDraftUser, 'autocomplete="off"'))}${field("访问令牌", input("registry-token", "", 'type="password" autocomplete="off" placeholder="原型中请勿输入真实凭据"'))}</div><small>实际客户端使用系统凭据管理器；原型不保存认证信息或验证登录。</small><div class="actions">${button("测试登录（演示）", "registry-test")}${button("保存配置（演示）", "registry-save", "primary", deployLocked() ? "disabled" : "")}${state.registryConfigured ? button("移除配置", "registry-remove", "ghost destructive", deployLocked() ? "disabled" : "") : ""}</div><label class="row"><input type="checkbox" name="registry-enabled" ${state.registry ? "checked" : ""} ${!state.registryConfigured || deployLocked() ? "disabled" : ""}>发布使用镜像仓库</label><small class="muted">${state.registryConfigured ? I18n.t("已配置（会话演示）") : I18n.t("尚未配置")}</small></div>`,
      ),
    alerts: () =>
      card(
        "告警",
        "P2 配置预览；浏览器原型不采样或发送通知。",
        I18n.html`<div class="form-grid">${[
          ["cpu", "CPU"],
          ["memory", "内存"],
          ["disk", "磁盘"],
        ]
          .map(([key, label]) =>
            field(
              label + " 阈值（%）",
              input(
                "threshold-" + key,
                state.alerts[key],
                'type="number" min="1" max="100" required',
              ),
            ),
          )
          .join(
            "",
          )}${field("持续时长", select("alert-duration", ["1 分钟", "5 分钟", "10 分钟"], state.alerts.duration))}</div>`,
      ),
    backup: () => BackupUI.render(),
    environment: () =>
      card(
        "本地环境",
        "以下路径及检测结果均为演示。",
        I18n.html`<div class="stack"><div class="row between environment-row"><span class="environment-details"><span>Docker：</span><span class="mono environment-path" title="/usr/local/bin/docker">/usr/local/bin/docker</span>${badge(state.docker ? I18n.t("可用") : I18n.t("不可用"), state.docker ? "success" : "destructive")}</span>${button("重新检测", "docker-detect")}</div><div class="row between environment-row"><span class="mono environment-path" title="~/devingPanel/devingpanel.sqlite">~/devingPanel/devingpanel.sqlite</span>${button("打开所在文件夹", "native-folder")}</div>${field("主机数据采集间隔", select("host-collection-interval", [["10", "10 秒"], ["30", "30 秒"], ["60", "1 分钟"], ["300", "5 分钟"], ["custom", "自定义"]], state.hostCollectionCustom || ![10, 30, 60, 300].includes(state.hostCollectionIntervalSeconds) ? "custom" : state.hostCollectionIntervalSeconds), "桌面客户端在线时按此间隔采集主机数据并写入 SQLite；原型不实际采集或写入。")}${state.hostCollectionCustom || ![10, 30, 60, 300].includes(state.hostCollectionIntervalSeconds) ? field("自定义间隔（秒）", input("host-collection-custom", state.hostCollectionIntervalSeconds, 'type="number" min="10" max="3600" step="1" required')) : ""}</div>`,
      ),
    security: () =>
      card(
        "安全",
        "",
        ScreenLock.settings(),
      ),
  };
  const sections = settingsSections();
  return (
    head("设置") +
    I18n.html`<div class="settings-layout"><nav class="settings-nav" aria-label="设置分类">${sections.map(([key, label, symbol]) => I18n.html`<a href="#/settings/${key}" class="${state.settingsSection === key ? "active" : ""}" ${state.settingsSection === key ? 'aria-current="page"' : ""}>${icon(symbol)}${I18n.t(label)}</a>`).join("")}</nav><label class="field settings-mobile">设置分类${select(
      "settings-section",
      sections.map(([key, label]) => [key, label]),
      state.settingsSection,
    )}</label><div class="settings-content">${views[state.settingsSection]()}</div></div>`
  );
}
function timestamp() {
  return new Date().toISOString().replace(/[-:T]/g, "").slice(0, 14);
}
function deployLocked() {
  const d = state.deploy;
  return !!(
    d &&
    (d.running || d.paused || d.rows.some((r) => r.status === "待核实"))
  );
}
function managedCleanupPreview() {
  const candidates = [];
  let protectedCount = 0;
  for (const project of projects) {
    for (const host of hosts) {
      const versions = history
        .filter((record) => record.projectId === project.id)
        .flatMap((record) => (record.rows || []).filter((entry) => entry.id === host.id && entry.materials && entry.status === "成功").map((entry) => ({ record, entry })))
        .sort((a, b) => b.record.time.localeCompare(a.record.time));
      versions.forEach(({ record, entry }, index) => {
        if (index < state.keep || containers.some((container) => container.host === host.id && container.state === "running" && container.image === entry.image)) {
          protectedCount++;
        } else {
          candidates.push({ record, entry, host, project, eligible: host.online });
        }
      });
    }
  }
  return { candidates, protectedCount };
}
function releaseTag(p) {
  return (p.manifest ? p.tag : "{git-sha}")
    .replaceAll("{timestamp}", timestamp())
    .replaceAll("{git-sha}", "a1b2c3d");
}
function composeIssue(p) {
  if (!p.files.length) return "尚未配置 Compose 文件。";
  if (p.files[0].includes("/")) return "首个 Compose 文件必须位于项目根目录。";
  return "";
}
function groupReference(name) {
  return projects.some((project) => [project.manifest, project.manifestDraft].some((manifest) => manifest?.targets?.some((target) => target.group === name)));
}
function groupNameIssue(name, current = "") {
  if (!name) return "请输入分组名称。";
  if (name === "未分组") return "未分组是保留名称。";
  if (name !== current && hostGroups.includes(name)) return "分组名称已存在。";
  return "";
}
function manageHostGroups(editIndex = -1) {
  modal("管理分组", I18n.html`<div class="stack"><form id="group-create-form" class="group-entry"><input name="group-name" required maxlength="64" placeholder="${I18n.t("新分组名称")}" aria-label="${I18n.t("新分组名称")}">${button(icon("plus") + I18n.t("创建分组"), "group-create", "primary", "", false)}</form><div class="group-list">${hostGroups.map((name, index) => {
    const count = hosts.filter((host) => host.group === name).length;
    return I18n.html`<div class="group-row">${editIndex === index ? I18n.html`<div class="group-entry"><input name="rename-group" value="${esc(name)}" required maxlength="64" aria-label="${I18n.t("分组名称")}">${ib("check", "保存", `group-rename-save:${index}`)}${ib("close", "取消", "host-groups")}</div>` : I18n.html`<span class="grow">${esc(name)} <small>${count} ${I18n.t("台主机")}</small></span>${ib("pencil", "重命名分组", `group-rename:${index}`)}${ib("trash", "删除分组", `group-delete:${index}`)}`}</div>`;
  }).join("")}</div></div>`, button("关闭", "close"));
  if (editIndex >= 0) $('[name="rename-group"]').focus();
}
function hostForm(id = 0, copyId = 0) {
  const h = hosts.find((host) => host.id === (copyId || id));
  const editing = !!id;
  const segment = (name, options, selected) =>
    I18n.html`<div class="host-segments">${options.map(([value, label]) => I18n.html`<label><input type="radio" name="${name}" value="${value}" ${value === selected ? "checked" : ""}><span>${I18n.t(label)}</span></label>`).join("")}</div>`;
  const jumpOptions = (selected = "") => select("jump-host", [["", "选择跳板主机"], ...hosts.filter((host) => host.id !== id).map((host) => [host.id, `${host.name} (${host.user}@${host.address}:${host.port})`])], selected, "required");
  modal(
    editing ? "编辑主机" : copyId ? "复制主机" : "新建主机",
    I18n.html`<form id="host-form" class="host-form stack" data-id="${id}">
      ${field("名称", input("name", copyId ? `${h.name} 副本` : h?.name, 'placeholder="可选，默认用地址"'))}
      ${field("协议", I18n.html`<div class="host-protocol">SSH</div>`)}
      <div class="host-address-row">${field("地址 *", input("address", h?.address, 'required placeholder="主机名或 IP 地址"'))}${field("端口", input("port", h?.port || 22, 'type="number" min="1" max="65535" required'))}</div>
      ${field("认证方式", segment("auth", [["password", "密码"], ["key", "使用凭据"], ["none", "无密码"]], h?.auth || "password"))}
      ${field("用户名", input("user", h?.user || "root", "required"))}
      <div id="auth-input"></div>
      <div class="field"><label for="host-group-select">${I18n.t("分组")}</label><div class="host-group-control">${select("group", [["", "(无分组)"], ...hostGroups.map((group) => [group, group])], h?.group === "未分组" ? "" : h?.group || "", 'id="host-group-select"')}${ib("plus", "新建分组", "host-group-new")}</div><div id="new-host-group" class="group-entry" hidden><input name="new-host-group" maxlength="64" placeholder="${I18n.t("新分组名称")}" aria-label="${I18n.t("新分组名称")}">${ib("check", "创建分组", "host-group-create")}${ib("close", "取消", "host-group-new")}</div></div>
      ${field("连接方式", segment("connection-mode", [["direct", "直接连接"], ["jump", "SSH 跳板"], ["proxy", "代理连接"]], h?.connectionMode || "direct"))}
      <div id="jump-settings" class="host-connection-panel" hidden><p class="muted">依次经过跳板主机连接到当前主机，可添加多台。</p><div id="jump-hosts">${(h?.jumpHostIds?.length ? h.jumpHostIds : [""]).map((hostId) => I18n.html`<div class="host-jump-row">${jumpOptions(hostId)}${ib("trash", "移除跳板主机", "host-jump-remove")}</div>`).join("")}</div>${button(icon("plus") + I18n.t("添加跳板主机"), "host-jump-add", "", "", false)}</div>
      <div id="proxy-settings" class="host-connection-panel stack" hidden>${field("代理类型", select("proxy-type", [["http", "HTTP 代理"], ["socks5", "SOCKS5 代理"]], h?.proxyType || "http"))}<div class="host-address-row">${field("代理地址 *", input("proxy-address", h?.proxyAddress || "", 'placeholder="例如 127.0.0.1" required'))}${field("端口 *", input("proxy-port", h?.proxyPort || "", 'type="number" min="1" max="65535" required'))}</div><div class="host-address-row equal">${field("用户名", input("proxy-user", h?.proxyUser || "", 'placeholder="可选"'))}${field("密码", I18n.html`<span class="host-password">${input("proxy-password", "", 'type="password" autocomplete="off" placeholder="可选"')}${ib("eye", "显示或隐藏密码", "host-password-toggle")}</span>`)}</div><small>目标地址由代理服务器解析；代理不需要认证时，用户名和密码留空。</small></div>
      ${field("备注", I18n.html`<textarea name="note">${esc(h?.note || "")}</textarea>`)}
      <small>密码只用于当前表单演示，不会保存或导出；浏览器原型不会连接主机。</small><div id="test-result" role="status"></div>
    </form>`,
    button("测试连接", "host-form-test") +
      button("取消", "close") + button(editing ? "保存" : "创建", "host-save", "primary"),
  );
  $("#dialog").classList.add("host-dialog");
  updateHostForm();
}
function hostRoute(form) {
  const data = new FormData(form);
  const mode = data.get("connection-mode");
  return JSON.stringify(mode === "jump" ? [mode, data.getAll("jump-host")] : mode === "proxy" ? [mode, data.get("proxy-type"), data.get("proxy-address")?.trim(), data.get("proxy-port"), data.get("proxy-user")?.trim()] : ["direct"]);
}
function savedHostRoute(host) {
  return JSON.stringify(host.connectionMode === "jump" ? ["jump", (host.jumpHostIds || []).map(String)] : host.connectionMode === "proxy" ? ["proxy", host.proxyType, host.proxyAddress, String(host.proxyPort), host.proxyUser] : ["direct"]);
}
function openHostTerminal(id) {
  const host = hosts.find((item) => item.id === id);
  if (!host) return;
  $$("details.menu[open]").forEach((menu) => (menu.open = false));
  const prompt = `${host.user}@${host.name}:~${host.user === "root" ? "#" : "$"} `;
  const welcome = `Linux demo-debian 6.12.63+deb13-amd64 #1 SMP PREEMPT_DYNAMIC Debian 6.12.63-1 x86_64\n\nThe programs included with the Debian GNU/Linux system are free software;\nthe exact distribution terms for each program are described in the\nindividual files in /usr/share/doc/*/copyright.\n\nDebian GNU/Linux comes with ABSOLUTELY NO WARRANTY, to the extent\npermitted by applicable law.\nLast login: ${new Date().toLocaleString(I18n.locale === "en" ? "en-US" : "zh-CN")}\n${I18n.t("[演示] 未建立远程 SSH 连接，命令不会执行。")}\n`;
  modal("连接", I18n.html`<div class="ssh-terminal"><pre id="ssh-terminal-output" aria-live="polite">${esc(welcome)}</pre><form id="ssh-terminal-form" class="ssh-terminal-prompt" data-prompt="${esc(prompt)}"><span aria-hidden="true">${esc(prompt)}</span><span class="terminal-entry"><span class="terminal-cursor" aria-hidden="true"></span><input name="terminal-command" autocomplete="off" spellcheck="false" aria-label="${I18n.t("终端命令")}"></span></form></div>`);
  $("#dialog").classList.add("terminal-dialog");
  $('[name="terminal-command"]').focus();
}
function submitTerminalCommand() {
  const input = $('[name="terminal-command"]');
  const command = input.value.trim();
  if (!command) return;
  const output = $("#ssh-terminal-output");
  output.textContent += `${$("#ssh-terminal-form").dataset.prompt}${command}\n${I18n.t("演示终端不会执行远程命令。")}\n`;
  input.value = "";
  input.parentElement.classList.remove("has-value");
  input.scrollIntoView({ block: "nearest" });
}
function updateHostForm() {
  const form = $("#host-form");
  if (!form) return;
  const auth = $('[name="auth"]:checked', form).value;
  const authInput = $("#auth-input");
  if (authInput.dataset.auth !== auth) {
    authInput.dataset.auth = auth;
    authInput.innerHTML = auth === "none" ? "" : field(auth === "key" ? "凭据" : "密码", auth === "key" ? I18n.html`<textarea name="credential" class="mono" placeholder="仅用于演示，请勿粘贴真实私钥"></textarea>` : I18n.html`<span class="host-password">${input("credential", "", 'type="password" autocomplete="off" placeholder="留空则每次连接都询问"')}${ib("eye", "显示或隐藏密码", "host-password-toggle")}</span>`);
  }
  for (const [mode, id] of [["jump", "jump-settings"], ["proxy", "proxy-settings"]]) {
    const panel = $(`#${id}`);
    panel.hidden = $('[name="connection-mode"]:checked', form).value !== mode;
    $$('input,select,textarea', panel).forEach((control) => control.disabled = panel.hidden);
  }
}
function testHost(id) {
  const h = hosts.find((h) => h.id === id);
  if (!h) return;
  if (h.trustedAddress !== h.address || h.trustedPort !== h.port || (h.trustedRoute || '["direct"]') !== savedHostRoute(h)) {
    const changed = !!h.trustedAddress;
    modal(
      changed ? "主机指纹变化：连接已阻断" : "首次连接：确认主机指纹",
      I18n.html`<div class="stack"><div class="notice warning">请通过独立渠道核对主机指纹，不要自动接受。以下指纹仅用于演示。</div>${changed ? I18n.html`<p>旧地址：<code>${esc(h.trustedAddress)}</code></p><p class="mono">旧指纹 SHA256:DEMO_OLD</p>` : ""}<p>地址：<code>${esc(h.address)}</code></p><p class="mono">新指纹 SHA256:DEMO_${h.id}</p></div>`,
      button("取消", "close") +
        button(
          changed ? "更新可信指纹" : "确认演示指纹",
          `host-trust:${id}`,
          "primary",
        ),
    );
    return;
  }
  h.online = true;
  render();
  toast("连接成功，Docker 已就绪（演示）。");
}
function projectForm(id = 0) {
  const p = projects.find((p) => p.id === id);
  modal(
    p ? "编辑项目" : "新建项目",
    I18n.html`<form id="project-form" class="stack" data-id="${id}">${field("项目名", input("name", p?.name, "required"))}${field("本地代码目录", I18n.html`<div class="row">${input("path", p?.path, 'required class="mono" placeholder="D:\\code\\myapp"')}${button("浏览", "browse-directory")}</div>`)}${field("Dockerfile 路径", input("dockerfile", p?.dockerfile || "Dockerfile", 'required class="mono"'), "相对于本地代码目录")}</form>`,
    button("取消", "close") +
      button(p ? "保存" : "创建项目", "project-save", "primary"),
    true,
  );
}
function historyDetail(id) {
  const r = history.find((r) => r.id === id);
  if (!r) return;
  const healthy = r.rows.filter((h) => h.status === "成功").length;
  modal(
    "发布详情",
    I18n.html`<div class="history-detail"><div class="history-summary"><strong>${esc(r.project)}</strong><span class="mono muted">${esc(r.tag)}</span><div class="row wrap">${badge(r.result, r.tone)}<span>${healthy}/${r.rows.length} 台通过健康检查</span></div><small>发布于 ${esc(r.time)} · 耗时 ${esc(r.duration)}</small>${r.result !== "成功" && r.message ? I18n.html`<p>${esc(I18n.t(r.message))}</p>` : ""}</div><details class="history-targets" ${r.rows.length <= 4 ? "open" : ""}><summary>目标主机（${r.rows.length} 台）</summary><div class="history-host-list ${r.rows.length > 4 ? "scroll-limited" : ""}">${r.rows
      .map(
        (h) => {
          const outcome = h.status === "已跳过" && h.reason
            ? `失败后跳过：${h.reason}`
            : h.reason || ({
            "成功": "健康检查通过",
            "已启动，未验证就绪": "容器已启动，尚未确认服务就绪",
            "待核实": "远端结果不明，需要重新连接核实",
            "已跳过": "已跳过此主机",
            "已终止": "发布终止，未部署",
            "等待": "尚未开始部署",
          }[h.status] || h.phase);
          return I18n.html`<div class="history-host"><div class="row between"><strong>${esc(h.name)}</strong>${badge(h.status, h.tone)}</div>${h.status !== "成功" ? I18n.html`<p>${esc(I18n.t(outcome))}</p>` : ""}<details><summary>技术信息</summary><div class="history-tech"><div>镜像 <code>${esc(h.image)}</code></div><div>平台 <code>linux/${esc(h.arch)}</code></div>${
            h.timings
              ? I18n.html`<div>${Object.entries(h.timings)
                  .map(([k, v]) => `${esc(I18n.t(k))} ${v}s`)
                  .join(" · ")}</div>`
              : ""
          }</div></details></div>`;
        },
      )
      .join("")}</div></details><section class="history-log"><div class="row between"><h3>构建日志</h3>${r.logs?.length ? button(icon("copy") + "复制日志", `history-copy-log:${r.id}`, "ghost") : ""}</div><pre class="log" translate="no">${esc(r.logs?.join("\n") || "这条历史记录未保存构建日志。")}</pre></section></div>`,
    "",
    true,
    true,
  );
}
function rollbackCandidates(record) {
  return record.rows.map((h) => {
    const completed = history
      .flatMap((r) =>
        r.projectId === record.projectId
          ? r.rows
              .filter((row) => row.id === h.id && row.materials)
              .map((row) => ({ record: r, row }))
          : [],
      )
      .slice(0, state.keep);
    const current = completed[0],
      previous = completed[1];
    const host = hosts.find((x) => x.id === h.id);
    const reason = !projects.some((p) => p.id === record.projectId)
      ? "项目已删除，无法回滚"
      : deployLocked() && state.deploy.ids.includes(h.id)
        ? "该主机发布中或待核实，不能回滚"
        : !host
          ? "主机已删除"
          : !host.online
            ? "主机离线，无法核实"
            : state.keep === 1
              ? "仅保留当前版本"
              : !previous
                ? "无上一已完成版本"
                : !h.materials
                  ? "本次未更新此主机"
                  : current?.record.id !== record.id
                    ? "所选记录不是当前版本"
                    : "";
    return { id: h.id, name: h.name, current, previous, reason };
  });
}
function rollbackDialog(id) {
  const r = history.find((r) => r.id === id);
  if (!r) return;
  const candidates = rollbackCandidates(r);
  modal(
    "回滚已更新主机",
    I18n.html`<div class="stack"><p class="muted">逐台恢复镜像、Compose 与环境配置，再检查健康状态。每台主机独立选择上一已完成版本。</p>${table(
      ["选择", "主机", "当前版本 → 上一版本", "可回滚状态"],
      candidates.map((c) =>
        row([
          I18n.html`<input type="checkbox" name="rollback-host" value="${c.id}" ${c.reason ? "disabled" : "checked"} aria-label="回滚 ${esc(c.name)}">`,
          esc(c.name),
          I18n.html`<span class="mono">${esc(c.current?.record.tag || "—")}<br>→ ${esc(c.previous?.record.tag || "—")}</span>`,
          badge(
            c.reason || "材料完整（演示）",
            c.reason ? "warning" : "success",
          ),
        ]),
      ),
    )}<div class="notice warning">历史版本若无 HEALTHCHECK，恢复后仍标记“已启动，未验证就绪”。原型不会执行真实回滚。</div></div>`,
    button("取消", "close") +
      button(
        "确认回滚",
        `rollback-confirm:${id}`,
        "primary",
        candidates.every((c) => c.reason) ? "disabled" : "",
      ),
    true,
  );
}
function showLogs(id) {
  const c = containers.find((c) => c.id === id);
  if (!c) return;
  state.follow = true;
  modal(
    esc(c.name),
    I18n.html`<div class="row wrap between"><label class="row"><input type="checkbox" name="follow" checked>跟随</label><div class="actions">${button("停止", `container-stop:${id}`)}${button("重启", `container-restart:${id}`)}</div></div><pre class="log" id="container-log" translate="no" tabindex="0">${esc(Array.from({ length: 35 }, (_, i) => `2026-10-24 14:${String(i).padStart(2, "0")}:00 INFO [demo] ${i % 4 ? "GET /health 200 OK" : "服务已就绪，监听 0.0.0.0:3000"}`).join("\n"))}</pre><div class="row between"><small>仅显示最近 1000 行 (docker logs) · 演示日志</small>${button("回到底部", "log-bottom", "ghost")}</div>`,
    "",
    true,
    true,
    false,
  );
  const log = $("#container-log");
  log.scrollTop = log.scrollHeight;
  log.addEventListener("scroll", () => {
    if (log.scrollTop + log.clientHeight < log.scrollHeight - 12) {
      state.follow = false;
      $('[name="follow"]').checked = false;
    }
  });
  clearInterval(logTimer);
  logTimer = setInterval(() => {
    if (!$("#sheet").open || !$("#container-log"))
      return clearInterval(logTimer);
    if (c.state !== "running") return;
    const lines = log.textContent.split("\n");
    lines.push(
      `${new Date().toLocaleTimeString(I18n.locale)} INFO [demo] GET /health 200 OK`,
    );
    log.textContent = lines.slice(-1000).join("\n");
    if (state.follow) log.scrollTop = log.scrollHeight;
  }, 1500);
}
function startDeploy() {
  if (deployLocked()) return;
  const p = projects.find((p) => p.id === state.selectedProject) || projects[0];
  const ids = selectedDeployHosts(p);
  if (!projectReady(p) || !ids.length || !state.docker)
    return toast("请完善项目配置、选择目标主机并启用本地 Docker。", true);
  if (composeIssue(p)) return toast(composeIssue(p), true);
  if (
    p.platform !== "auto" &&
    ids.some(
      (id) => "linux/" + hosts.find((h) => h.id === id).arch !== p.platform,
    )
  )
    return toast("目标平台与所选主机架构不匹配，未开始构建或传输。", true);
  if (ids.some((id) => !hosts.find((h) => h.id === id)?.online))
    return toast("有目标主机离线，请先测试连接或取消选择。", true);
  const tag = $("#release-tag")?.textContent.trim() || releaseTag(p);
  if (!/^[\w][\w.-]{0,110}$/.test(tag))
    return toast(
      "发布标识只能包含字母、数字、下划线、点和连字符，长度不超过 111。",
      true,
    );
  if (history.some((r) => r.projectId === p.id && r.tag === tag))
    return toast("发布标识已存在，不允许覆盖历史版本。", true);
  if (state.stage === "p0" && ids.length > 1)
    return toast("P0 仅支持单主机发布，请选择一台。", true);
  const d = (state.deploy = {
    ids,
    tag,
    project: p.name,
    projectId: p.id,
    registry: state.registry,
    parallel: state.parallel,
    scenario: state.scenario,
    push: 0,
    running: true,
    paused: false,
    build: "构建中",
    result: "",
    tone: "info",
    message: "在开发机校验并构建镜像…",
    logs: [
      `[演示] 传输方式：${state.registry ? "Registry push / pull" : "SSH save / SFTP / load"}`,
      `[演示] 目标平台：${[...new Set(ids.map((id) => `linux/${hosts.find((h) => h.id === id).arch}`))].join("、")}`,
      "[演示] 校验 Compose 路径、镜像变量与目标平台",
      "[演示] 校验远端禁用 build、主机架构及本地构建能力",
      "[演示] 本地 Docker build 启动",
    ],
    rows: ids.map((id) => {
      const h = hosts.find((h) => h.id === id);
      return {
        id,
        name: h.name,
        arch: h.arch,
        image: `${state.registry ? state.registryAddress + "/" : ""}${p.image}:${tag}-${h.arch}`,
        status: "等待",
        tone: "muted",
        phase: "—",
        percent: 0,
        elapsed: "—",
        timings: {},
      };
    }),
    tick: 0,
    failed: false,
    stopping: false,
  });
  render();
  d.timer = setInterval(() => deployTick(d), 900);
}
function deployTick(d) {
  if (state.deploy !== d || !d.running) return clearInterval(d.timer);
  d.tick++;
  if (d.tick === 2) {
    d.build = "完成";
    d.logs.push("[演示] 本地构建完成；校验有效 HEALTHCHECK 与最终镜像 tag");
    d.message = "构建后预检通过，准备分发镜像。";
    if (d.scenario === "unverified" && d.ids.length > 1) {
      d.running = false;
      d.result = "构建后预检失败";
      d.tone = "destructive";
      d.message =
        "多主机发布必须提供有效 HEALTHCHECK，所有主机保持等待，未开始传输。";
      d.logs.push("[演示] " + d.message);
      clearInterval(d.timer);
      saveDeployRecord(d);
      render();
      toast(d.message, true);
      return;
    }
  }
  if (d.tick < 3) {
    if (state.route === "deploy") render();
    return;
  }
  if (d.registry && d.push < 100 && !d.stopping) {
    d.push = Math.min(100, d.push + 34);
    d.message = I18n.msg`按架构推送到 Registry：${d.push}%`;
    d.logs.push(I18n.msg`[演示] 本地各架构 push ${d.push}%`);
    if (state.route === "deploy") render();
    return;
  }
  const active = d.rows.filter((r) =>
    ["传输中", "部署中", "健康检查中"].includes(r.status),
  );
  if (!active.length && !d.paused && !d.stopping) {
    const pending = d.rows
      .filter((r) => r.status === "等待")
      .slice(0, d.parallel);
    pending.forEach((r) => {
      r.status = "传输中";
      r.tone = "info";
      r.phase = d.registry ? "拉取镜像" : "SFTP 传输";
      r.percent = 0;
      d.logs.push(`[演示] ${r.name}：${d.registry ? "开始拉取镜像" : "开始传输镜像"}`);
    });
  }
  d.rows.forEach((r, i) => {
    if (["传输中", "部署中", "健康检查中"].includes(r.status))
      r.timings[r.status] = (r.timings[r.status] || 0) + 1;
    if (r.status === "传输中") {
      r.percent += 25;
      r.elapsed = d.tick - 2 + "s";
      if (r.percent >= 100) {
        r.status = "部署中";
        r.phase = "docker compose up -d";
        d.logs.push(
          I18n.msg`[演示] ${hosts.find((h) => h.id === d.ids[i])?.name} 镜像传输完成`,
        );
      }
    } else if (r.status === "部署中") {
      if (!d.failed && i === 0 && ["failure", "unknown"].includes(d.scenario)) {
        r.status = d.scenario === "unknown" ? "待核实" : "失败";
        r.tone = d.scenario === "unknown" ? "warning" : "destructive";
        r.phase =
          d.scenario === "unknown" ? "SSH 断开，远端结果未知" : "容器启动失败";
        r.reason = d.scenario === "unknown"
          ? "SSH 连接中断，无法确认远端 Compose 是否已启动。"
          : "端口 8080 已被占用，docker compose up -d 未完成。";
        d.logs.push(`[演示] ${r.name}：${r.reason}`);
        d.failed = true;
        d.paused = true;
        d.message =
          d.scenario === "unknown"
            ? "结果待核实；核实前不可重试或跳过。"
            : "发布暂停；已启动任务继续至可确认状态。";
      } else {
        r.status = "健康检查中";
        r.tone = "warning";
        r.phase = "等待 Docker HEALTHCHECK";
        d.logs.push(`[演示] ${r.name}：Compose 已启动，等待健康检查`);
      }
    } else if (r.status === "健康检查中") {
      r.status = d.scenario === "unverified" ? "已启动，未验证就绪" : "成功";
      r.tone = d.scenario === "unverified" ? "warning" : "success";
      r.phase = d.scenario === "unverified" ? "容器 running" : "healthy";
      r.elapsed = d.tick - 2 + "s";
      r.materials = true;
      r.reason = "";
      d.logs.push(`[演示] ${r.name}：${d.scenario === "unverified" ? "容器运行中，但没有有效的健康检查" : "健康检查通过，服务已就绪"}`);
    }
  });
  const hasActive = d.rows.some((r) =>
    ["传输中", "部署中", "健康检查中"].includes(r.status),
  );
  if (d.stopping && !hasActive) {
    d.rows
      .filter((r) => r.status === "等待")
      .forEach((r) => {
        r.status = "已终止";
        r.phase = "未执行";
        d.logs.push(`[演示] ${r.name}：发布已终止，此主机未开始`);
      });
    finishDeploy(d);
  } else if (
    !hasActive &&
    !d.rows.some((r) => ["等待", "失败", "待核实"].includes(r.status))
  )
    finishDeploy(d);
  else if (d.paused && !hasActive) {
    d.running = false;
    clearInterval(d.timer);
    saveDeployRecord(d);
  }
  if (state.route === "deploy") render();
  else renderShellProgress();
}
function renderShellProgress() {
  const marker = $(".global-progress");
  if (!state.deploy?.running) {
    marker?.remove();
    $("nav .dot")?.remove();
  }
}
function finishDeploy(d) {
  // A verified host must not end another host's in-flight deployment.
  if (d.rows.some((r) => ["传输中", "部署中", "健康检查中"].includes(r.status)))
    return;
  clearInterval(d.timer);
  d.running = false;
  d.paused = false;
  if (d.stopping)
    d.rows
      .filter((r) => r.status === "等待")
      .forEach((r) => {
        r.status = "已终止";
        r.phase = "未执行";
        d.logs.push(`[演示] ${r.name}：发布已终止，此主机未开始`);
      });
  const unknown = d.rows.some((r) => r.status === "待核实");
  const good = d.rows.filter((r) => r.status === "成功").length;
  d.result = unknown
    ? "结果待核实"
    : d.stopping
      ? "已终止"
      : good === d.rows.length
        ? "成功"
        : d.rows.some((r) => r.status === "已启动，未验证就绪")
          ? "已启动，未验证就绪"
          : "部分完成";
  d.tone =
    unknown || d.result.includes("未验证")
      ? "warning"
      : good === d.rows.length
        ? "success"
        : "destructive";
  d.message = I18n.msg`${d.result} · ${good}/${d.rows.length} 健康成功 · 耗时 ${d.tick}s`;
  d.logs.push("[演示] " + d.message);
  saveDeployRecord(d);
  toast("演示发布：" + d.result, d.tone === "destructive");
}
function saveDeployRecord(d) {
  const record = {
    id: d.recordId || Date.now(),
    time: new Date().toLocaleString(I18n.locale, { hour12: false }),
    project: d.project,
    projectId: d.projectId,
    tag: d.tag,
    result: d.result || (d.paused ? "已暂停" : "进行中"),
    tone: d.tone,
    duration: d.tick + "s",
    message: d.message,
    logs: [...d.logs],
    targets: [...d.ids],
    rows: structuredClone(d.rows),
  };
  const index = history.findIndex((r) => r.id === d.recordId);
  if (index < 0) history.unshift(record);
  else history[index] = record;
  d.recordId = record.id;
}
function resumeDeploy(index, skip = false) {
  const d = state.deploy;
  if (!d || d.stopping || d.rows[index]?.status !== "失败") return;
  Object.assign(
    d.rows[index],
    skip
      ? { status: "已跳过", tone: "muted", phase: "用户跳过" }
      : { status: "传输中", tone: "info", phase: "重试传输", percent: 0 },
  );
  d.logs.push(`[演示] ${d.rows[index].name}：${skip ? "失败后跳过此主机" : "用户重试镜像传输"}`);
  d.paused = d.rows.some((r) => ["失败", "待核实"].includes(r.status));
  d.running = true;
  d.message = "继续发布，不重新构建镜像。";
  clearInterval(d.timer);
  d.timer = setInterval(() => deployTick(d), 900);
  render();
}
function updateProjectReadiness(p) {
  p.ready = !!(
    !p.restoreNeedsReview &&
    p.service &&
    !composeIssue(p) &&
    p.hostIds.length
  );
}
function projectReady(p) {
  if (!p.ready) return false;
  if (!(p.manifest || (state.previewYamlProject && p.id === 1))) return true;
  const m = manifestFor(p);
  return !!p.directoryHandle && p.manifestName === p.name && m.builds.length === 1 && !state.previewYamlError && JSON.stringify(m) === JSON.stringify(p.manifest) &&
    m.targets.every((t) => hosts.filter((h) => h.group === t.group && h.name === t.host).length === 1);
}
function selectedDeployHosts(p) {
  return state.selectedHosts.filter(
    (id) => p.hostIds.includes(id) && hosts.some((h) => h.id === id),
  );
}
document.addEventListener("submit", (e) => {
  e.preventDefault();
  if (e.target.id === "ssh-terminal-form") submitTerminalCommand();
  if (e.target.id === "group-create-form") $('[data-action="group-create"]', e.target).click();
});
document.addEventListener("input", (e) => {
  const name = e.target.name || "";
  if (name === "terminal-command") {
    e.target.parentElement.classList.toggle("has-value", !!e.target.value);
    return;
  }
  if (!name.startsWith("manifest-")) return;
  const p = project();
  if (!p) return;
  const m = manifestFor(p);
  const parts = name.split("-");
  if (name === "manifest-name") p.name = e.target.value;
  else if (name === "manifest-tag") m.tag = e.target.value;
  else if (parts[1] === "file") m.files[Number(parts[2])] = e.target.value;
  else if (parts[1] === "group" || parts[1] === "host") m.targets[Number(parts[2])][parts[1]] = e.target.value;
  else if (["service", "dockerfile", "image", "platform"].includes(parts[1])) m.builds[Number(parts[2])][parts[1]] = e.target.value;
  else if (parts[1] === "arg") m.builds[Number(parts[3])].args[Number(parts[4])][parts[2] === "key" ? 0 : 1] = e.target.value;
});
document.addEventListener("change", (e) => {
  if (e.target.name?.startsWith("manifest-")) e.target.dispatchEvent(new Event("input", { bubbles: true }));
});
document.addEventListener("click", async (e) => {
  const el = e.target.closest("[data-action]");
  if (!el || el.disabled) return;
  if (ScreenLock.isLocked) return;
  const [action, ...parts] = el.dataset.action.split(":");
  if (action.startsWith("screen-")) return ScreenLock.handleAction(action);
  if (!state.docker && !["preview", "preview-apply", "theme", "menu", "close", "docker-setup", "docker-detect"].includes(action)) return;
  const arg = parts.join(":");
  const id = Number(arg);
  if (action.startsWith("backup-")) return BackupUI.handleAction(action, arg);
  if (action === "noop") return;
  if (action === "history-prev" || action === "history-next") {
    state.historyPage = Math.max(
      0,
      state.historyPage + (action === "history-next" ? 1 : -1),
    );
    render();
    return;
  }
  if (action === "close") return close();
  if (action === "menu") return $("#sidebar").classList.toggle("open");
  if (action === "nav") {
    location.hash = "#/" + arg;
    return;
  }
  if (action === "tab") {
    location.hash = `#/${state.route}/${state.id}/${arg}`;
    return;
  }
  if (action === "theme") {
    theme(document.documentElement.classList.contains("dark") ? "light" : "dark");
    render();
    return;
  }
  if (action === "docker-setup")
    return modal(
      "安装本机 Docker",
      I18n.html`<div class="stack"><p>选择本机操作系统，按 Docker 官方指引安装并启动 Docker。</p><div class="docker-install-links"><a href="https://docs.docker.com/desktop/setup/install/windows-install/" target="_blank" rel="noopener noreferrer">Windows · Docker Desktop</a><a href="https://docs.docker.com/desktop/setup/install/mac-install/" target="_blank" rel="noopener noreferrer">macOS · Docker Desktop</a><a href="https://docs.docker.com/engine/install/" target="_blank" rel="noopener noreferrer">Linux · Docker Engine</a></div><p class="muted">网页原型不会执行安装或检测；安装完成后可在预览选项中模拟检测结果。</p></div>`,
      button("关闭", "close"),
    );
  if (action === "preview")
    return modal(
      "原型预览选项",
      I18n.html`<div class="stack"><p class="muted">仅改变演示状态，不连接任何真实服务。</p>${field(
        "迭代阶段",
        select(
          "preview-stage",
          [
            ["p0", "P0 · 单机发布 MVP"],
            ["p1", "P1 · 多机 / 监控 / 容器"],
            ["p2", "P2 · 趋势 / 告警"],
          ],
          state.stage,
        ),
      )}${field(
        "页面状态",
        select(
          "preview-view",
          [
            ["normal", "正常"],
            ["loading", "加载"],
            ["empty", "空状态"],
            ["error", "错误"],
          ],
          state.view,
        ),
      )}${field(
        "发布情景",
        select(
          "preview-scenario",
          [
            ["success", "健康成功"],
            ["failure", "部署失败"],
            ["unknown", "SSH 断开 / 待核实"],
            ["unverified", "单主机无 HEALTHCHECK"],
          ],
          state.scenario,
        ),
      )}<label class="row"><input name="preview-docker" type="checkbox" ${state.docker ? "checked" : ""}>本地 Docker 可用</label><label class="row"><input name="preview-host-docker-missing" type="checkbox" ${state.previewDockerMissing ? "checked" : ""}>web-1 的 Docker 未安装</label><label class="row"><input name="preview-mirrors" type="checkbox" ${state.previewMirrors ? "checked" : ""}>主机镜像源配置</label><label class="row"><input name="preview-yaml-project" type="checkbox" ${state.previewYamlProject ? "checked" : ""}>开发者配置项目</label><label class="row"><input name="preview-yaml-error" type="checkbox" ${state.previewYamlError ? "checked" : ""}>YAML 目标缺失</label><small>所有表格、进度和日志均为演示数据；刷新页面重置业务数据。</small></div>`,
      button("应用", "preview-apply", "primary"),
    );
  if (action === "preview-apply") {
    if (deployLocked())
      return toast("发布尚未结束，暂不能切换演示情景。", true);
    state.stage = $('[name="preview-stage"]').value;
    state.view = $('[name="preview-view"]').value;
    state.scenario = $('[name="preview-scenario"]').value;
    state.docker = $('[name="preview-docker"]').checked;
    state.previewDockerMissing = $('[name="preview-host-docker-missing"]').checked;
    state.previewMirrors = $('[name="preview-mirrors"]').checked;
    state.previewYamlProject = $('[name="preview-yaml-project"]').checked;
    state.previewYamlError = $('[name="preview-yaml-error"]').checked;
    if (state.stage === "p0") {
      state.selectedHosts = state.selectedHosts.slice(0, 1);
      state.parallel = 1;
      if (state.route === "containers") state.route = "dashboard";
    }
    close();
    render();
    return;
  }
  if (action === "retry") {
    state.view = "normal";
    render();
    return;
  }
  if (action === "host-groups") return manageHostGroups();
  if (action === "group-create" || action === "host-group-create") {
    const inHostForm = action === "host-group-create";
    const input = $(`[name="${inHostForm ? "new-host-group" : "group-name"}"]`);
    const name = input.value.trim();
    const issue = groupNameIssue(name);
    if (issue) return toast(issue, true);
    hostGroups.push(name);
    if (inHostForm) {
      const option = document.createElement("option");
      option.value = name;
      option.textContent = name;
      $('[name="group"]', $("#host-form")).append(option);
      $('[name="group"]', $("#host-form")).value = name;
      $("#new-host-group").hidden = true;
      input.value = "";
    } else {
      render();
      manageHostGroups();
    }
    return;
  }
  if (action === "host-group-new") {
    const entry = $("#new-host-group");
    entry.hidden = !entry.hidden;
    if (!entry.hidden) $('[name="new-host-group"]', entry).focus();
    return;
  }
  if (action === "group-rename" || action === "group-rename-save" || action === "group-delete") {
    const name = hostGroups[id];
    if (!name) return;
    if (groupReference(name)) return toast("该分组已被项目 YAML 目标引用，请先调整项目目标。", true);
    if (action === "group-rename") return manageHostGroups(id);
    if (action === "group-delete") {
      if (hosts.some((host) => host.group === name)) return toast("请先将组内主机移至其他分组。", true);
      hostGroups.splice(id, 1);
      if (state.group === name) state.group = "all";
    } else {
      const next = $('[name="rename-group"]').value.trim();
      const issue = groupNameIssue(next, name);
      if (issue) return toast(issue, true);
      hostGroups[id] = next;
      hosts.filter((host) => host.group === name).forEach((host) => (host.group = next));
      if (state.group === name) state.group = next;
    }
    render();
    manageHostGroups();
    return;
  }
  if (action === "host-new" || action === "host-edit") return hostForm(id);
  if (action === "host-copy") return hostForm(0, id);
  if (action === "host-connect") return openHostTerminal(id);
  if (action === "host-password-toggle") {
    const input = $("input", el.closest(".host-password"));
    input.type = input.type === "password" ? "text" : "password";
    return;
  }
  if (action === "host-jump-add") {
    const row = $("#jump-hosts .host-jump-row").cloneNode(true);
    $("select", row).value = "";
    $("#jump-hosts").append(row);
    return;
  }
  if (action === "host-jump-remove") {
    const rows = $$("#jump-hosts .host-jump-row");
    if (rows.length > 1) el.closest(".host-jump-row").remove();
    else $("select", rows[0]).value = "";
    return;
  }
  if (action === "host-form-test") {
    if (!$("#host-form").reportValidity()) return;
    const f = $("#host-form"),
      h = hosts.find((h) => h.id === Number(f.dataset.id));
    f.dataset.testAddress = $('[name="address"]', f).value.trim();
    f.dataset.testPort = $('[name="port"]', f).value;
    f.dataset.testRoute = hostRoute(f);
    const changed =
      h?.trustedAddress &&
      (h.trustedAddress !== f.dataset.testAddress ||
        h.trustedPort !== Number(f.dataset.testPort));
    $("#test-result").innerHTML =
      I18n.html`<div class="notice warning"><h3>${changed ? I18n.t("主机指纹变化：连接已阻断") : I18n.t("首次连接：核对 SSH 主机指纹")}</h3>${changed ? I18n.t('<p class="mono">旧指纹 SHA256:DEMO_OLD</p>') : ""}<p class="mono">新指纹 SHA256:DEMO_ONLY_NOT_A_REAL_FINGERPRINT</p><p>请通过独立渠道核对指纹。这里只演示确认流程。</p>${button(changed ? I18n.t("更新可信指纹") : I18n.t("确认演示指纹"), "fingerprint-confirm")}</div>`;
    return;
  }
  if (action === "fingerprint-confirm") {
    const f = $("#host-form");
    if (
      f.dataset.testAddress !== $('[name="address"]', f).value.trim() ||
      f.dataset.testPort !== $('[name="port"]', f).value ||
      f.dataset.testRoute !== hostRoute(f)
    ) {
      $("#test-result").innerHTML = I18n.t(
        '<p class="warning">连接目标已变化，请重新测试连接并核对指纹。</p>',
      );
      return;
    }
    f.dataset.trustedAddress = f.dataset.testAddress;
    f.dataset.trustedPort = f.dataset.testPort;
    f.dataset.trustedRoute = f.dataset.testRoute;
    $("#test-result").innerHTML = I18n.t(
      '<p class="success">连接成功，Docker 已就绪（模拟结果）。</p>',
    );
    return;
  }
  if (action === "host-save") {
    const f = $("#host-form");
    if (!f.reportValidity()) return;
    const data = new FormData(f);
    let h = hosts.find((h) => h.id === Number(f.dataset.id));
    const changes = {
      name: data.get("name").trim() || data.get("address").trim(),
      address: data.get("address").trim(),
      port: Number(data.get("port")),
      user: data.get("user").trim(),
      group: data.get("group").trim() || "未分组",
      auth: data.get("auth"),
      connectionMode: data.get("connection-mode"),
      jumpHostIds: data.getAll("jump-host").map(Number),
      proxyType: data.get("proxy-type") || "http",
      proxyAddress: data.get("proxy-address")?.trim() || "",
      proxyPort: Number(data.get("proxy-port") || 0),
      proxyUser: data.get("proxy-user")?.trim() || "",
      note: data.get("note").trim(),
    };
    if (!changes.address || !changes.user)
      return toast("地址和用户名不能为空。", true);
    const trustedAddress = f.dataset.trustedAddress || h?.trustedAddress || "";
    const trustedPort = Number(f.dataset.trustedPort || h?.trustedPort || 0);
    const trustedRoute = f.dataset.trustedRoute || h?.trustedRoute || '["direct"]';
    const route = hostRoute(f);
    if (h)
      Object.assign(h, changes, {
        trustedAddress,
        trustedPort,
        trustedRoute,
        online:
          trustedAddress === changes.address && trustedPort === changes.port && trustedRoute === route,
      });
    else
      hosts.push({
        ...changes,
        id: Date.now(),
        trustedAddress,
        trustedPort,
        trustedRoute,
        online:
          trustedAddress === changes.address && trustedPort === changes.port && trustedRoute === route,
        cpu: 0,
        memory: 0,
        disk: 0,
        containers: 0,
        arch: "amd64",
      });
    close();
    state.view = "normal";
    render();
    toast("主机配置已更新（仅本次演示；未保存凭据）。");
    return;
  }
  if (action === "host-test") return testHost(id);
  if (action === "host-docker-install")
    return modal(
      "一键安装 Docker",
      I18n.html`<div class="stack"><p>目标主机：web-1 · Debian 13 / amd64 · root</p><p>将添加 Docker CE apt 源与签名密钥，安装 Engine、CLI、containerd、Buildx、Compose，并启动 Docker 服务。</p><p>安装源：mirrors.cloud.tencent.com（腾讯云）或 download.docker.com（官方）。</p><p>仓库密钥须匹配 Docker 官方指纹 9DC8 5822 9FC7 DD38 854A E2D8 8D81 803C 0EBF CD88；不执行 curl | sh，也不设置未知镜像加速器。</p><p class="muted">原型仅展示确认流程，不连接 VPS 或安装软件。</p></div>`,
      button("取消", "close") + button("使用官方源安装", "host-docker-install-demo") + button("使用腾讯云镜像安装", "host-docker-install-demo", "primary"),
    );
  if (action === "host-docker-install-demo") {
    close();
    return toast("原型不执行安装；桌面客户端需通过确认后才会操作 VPS。");
  }
  if (action === "host-mirror-config")
    return modal(
      "配置 Docker Hub 镜像源",
      I18n.html`<div class="stack"><p>将保留 daemon.json 中其他设置，仅将 registry-mirrors 设置为：</p><p class="mono">https://docker.1panel.live<br>https://docker.m.daocloud.io</p><p>操作会重启 Docker；如果有运行中的容器，将阻断而不修改。公共镜像源可能记录镜像名称，生产镜像请优先使用受控 Registry 并固定 digest。</p><p class="muted">原型仅展示确认流程，不连接 VPS 或修改配置。</p></div>`,
      button("取消", "close") + button("确认配置", "host-mirror-demo", "primary"),
    );
  if (action === "host-mirror-demo") {
    close();
    return toast("原型不修改 VPS；桌面客户端需通过确认后才会配置镜像源。");
  }
  if (action === "host-trust") {
    const h = hosts.find((h) => h.id === id);
    h.trustedAddress = h.address;
    h.trustedPort = h.port;
    h.trustedRoute = savedHostRoute(h);
    close();
    return testHost(id);
  }
  if (action === "host-delete")
    return confirmAction(
      "删除主机？",
      "只删除本地演示配置，不影响 VPS；将解除相关项目绑定。",
      `host-delete-confirm:${id}`,
      "删除",
    );
  if (action === "host-delete-confirm") {
    if (state.deploy && deployLocked() && state.deploy.ids.includes(id))
      return toast("存在运行中或待核实的发布，不能删除主机。", true);
    hosts.splice(
      hosts.findIndex((h) => h.id === id),
      1,
    );
    projects.forEach((p) => {
      p.hostIds = p.hostIds.filter((x) => x !== id);
      updateProjectReadiness(p);
    });
    state.selectedHosts = state.selectedHosts.filter((x) => x !== id);
    close();
    render();
    toast("主机演示配置已删除。");
    return;
  }
  if (action === "project-import" || action === "manifest-connect") {
    try {
      const directory = await ManifestIO.pick();
      const loaded = await ManifestIO.read(directory);
      await ManifestIO.verifyFiles(directory, loaded.manifest);
      if (projects.some((item) => item.name === loaded.name && (action === "project-import" || item !== project()))) throw new Error("项目名已存在。");
      const p = action === "project-import" ? { id: Date.now(), name: loaded.name, path: directory.name, dockerfile: "", image: "", tag: "", platform: "auto", service: "", hostIds: [], files: [], args: [], env: [], ready: false } : project();
      if (action === "project-import") projects.push(p);
      useManifest(p, loaded, directory);
      close();
      location.hash = `#/projects/${p.id}`;
      render();
      toast("已读取项目目录中的 devingpanel.yaml。");
    } catch (error) {
      if (error.name !== "AbortError") toast(error.message || "读取项目配置失败。", true);
    }
    return;
  }
  if (action === "manifest-reload") {
    const p = project();
    try {
      const loaded = await ManifestIO.read(p.directoryHandle);
      await ManifestIO.verifyFiles(p.directoryHandle, loaded.manifest);
      useManifest(p, loaded, p.directoryHandle);
      render();
      toast("已重新读取项目配置。");
    } catch (error) { toast(error.message || "重新读取配置失败。", true); }
    return;
  }
  if (action === "manifest-target-demo")
    return toast("请由管理员预先配置并确认本机目标主机与凭据。");
  if (action.startsWith("manifest-build-") || action.startsWith("manifest-file-") || action.startsWith("manifest-target-") || action.startsWith("manifest-arg-")) {
    const m = manifestFor(project());
    const [, operation, op, indexText, rowText] = action.split("-");
    const list = operation === "build" ? m.builds : operation === "file" ? m.files : m.targets;
    const index = Number(indexText);
    if (operation === "arg") {
      const args = m.builds[index]?.args;
      if (op === "add") args.push(["", ""]);
      else args.splice(Number(rowText), 1);
    } else if (op === "add") {
      list.push(operation === "build" ? { service: "", dockerfile: "Dockerfile", image: "", platform: "auto", args: [] } : operation === "target" ? { group: "", host: "" } : "");
    } else if (op === "remove") {
      if (operation === "build" && list.length === 1) return;
      list.splice(index, 1);
    } else {
      const other = index + (op === "up" ? -1 : 1);
      [list[index], list[other]] = [list[other], list[index]];
    }
    render();
    return;
  }
  if (action === "manifest-save") {
    const p = project(), m = manifestFor(p);
    if (!p.name.trim() || projects.some((item) => item !== p && item.name === p.name.trim())) return toast("项目名为空或已存在。", true);
    const services = m.builds.map((b) => b.service.trim());
    if (!m.tag.trim() || !m.builds.length || services.some((name) => !/^[A-Za-z][A-Za-z0-9_-]*$/.test(name)) || new Set(services).size !== services.length || m.builds.some((b) => !b.image.trim() || !b.dockerfile.trim() || b.dockerfile.startsWith("/") || b.dockerfile.includes("..") || (b.args || []).some(([key]) => !/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)))) return toast("请检查 Tag、唯一服务、镜像、Dockerfile 和构建参数。", true);
    if (!m.files.length || m.files.some((file, i) => !/\.ya?ml$/i.test(file) || file.startsWith("/") || file.includes(":") || file.split(/[\\/]/).includes("..") || (i === 0 && /[\\/]/.test(file))) || new Set(m.files).size !== m.files.length) return toast("Compose 文件路径无效；首个文件须位于项目根目录。", true);
    const keys = m.targets.map((t) => `${t.group}/${t.host}`);
    if (!keys.length || new Set(keys).size !== keys.length || m.targets.some((t) => hosts.filter((h) => h.group === t.group && h.name === t.host).length !== 1) || state.previewYamlError) return toast("目标主机缺失或名称不唯一；请先由管理员处理。", true);
    if (!p.directoryHandle) return toast("请先选择项目目录。", true);
    if (JSON.stringify(m.targets) !== JSON.stringify(p.manifest.targets)) return confirmAction("确认目标绑定变更", "取消绑定的主机专属环境变量可能被清理；请核对目标顺序与主机身份。", "manifest-commit", "确认保存");
    el.dataset.action = "manifest-commit";
    return el.click();
  }
  if (action === "manifest-commit") {
    const p = project(), m = manifestFor(p);
    if (p.manifestSaving) return;
    p.manifestSaving = true;
    try {
      const source = await ManifestIO.save(p.directoryHandle, p.manifestSource, p.name, m);
      useManifest(p, { name: p.name, manifest: structuredClone(m), source }, p.directoryHandle);
      close();
      render();
      toast("已写入项目目录中的 devingpanel.yaml。");
    } catch (error) { toast(error.message || "保存项目配置失败。", true); }
    finally { p.manifestSaving = false; }
    return;
  }
  if (action === "project-new" || action === "project-edit")
    return projectForm(id);
  if (action === "browse-directory")
    return toast(
      "静态浏览器不能读取完整本地目录路径，请手动填写；桌面版将调用系统选择器。",
    );
  if (action === "project-save") {
    const f = $("#project-form");
    if (!f.reportValidity()) return;
    const data = new FormData(f);
    const name = data.get("name").trim();
    if (!name) return toast("项目名不能为空。", true);
    let p = projects.find((p) => p.id === Number(f.dataset.id));
    if (projects.some((x) => x !== p && x.name === name))
      return toast("项目名已存在。", true);
    if (p)
      Object.assign(p, {
        name,
        path: data.get("path"),
        dockerfile: data.get("dockerfile"),
      });
    else {
      p = {
        id: Date.now(),
        name,
        path: data.get("path"),
        dockerfile: data.get("dockerfile"),
        image: name.toLowerCase(),
        tag: "{git-sha}",
        platform: "auto",
        service: "",
        hostIds: [],
        files: [],
        args: [],
        env: [],
        ready: false,
      };
      projects.push(p);
    }
    close();
    location.hash = `#/projects/${p.id}`;
    render();
    toast("项目已保存，请完善部署配置。");
    return;
  }
  if (action === "project-delete")
    return confirmAction(
      "删除项目？",
      "只删除本地演示项目，不删除远端服务。",
      `project-delete-confirm:${id}`,
      "删除",
    );
  if (action === "project-delete-confirm") {
    if (state.deploy && deployLocked() && state.deploy.projectId === id)
      return toast("发布尚未结束，不能删除关联项目。", true);
    projects.splice(
      projects.findIndex((p) => p.id === id),
      1,
    );
    close();
    render();
    toast("项目已删除（演示）。");
    return;
  }
  if (action === "project-deploy") {
    const p = projects.find((p) => p.id === id);
    if (deployLocked()) {
      location.hash = "#/deploy";
      return toast("请先完成或终止当前发布。", true);
    }
    state.selectedProject = id;
    state.selectedHosts = [...p.hostIds].slice(
      0,
      state.stage === "p0" ? 1 : undefined,
    );
    state.deploy = null;
    location.hash = "#/deploy";
    return;
  }
  if (action === "build-save") {
    const f = $("#build-form");
    if (!f.reportValidity()) return;
    const data = new FormData(f),
      p = project();
    const args = $$(".kv-row", f).map((r) => [
      $('[name="key"]', r).value.trim(),
      $('[name="value"]', r).value,
    ]);
    if (p.restoreNeedsReview && args.some(([key, value]) => key && !value))
      return toast(
        "请补齐导入后清空的构建参数值，或明确删除不需要的参数。",
        true,
      );
    Object.assign(p, {
      image: data.get("image"),
      tag: "{git-sha}",
      platform: data.get("platform"),
      service: data.get("service"),
      args,
      restoreNeedsReview: false,
    });
    updateProjectReadiness(p);
    render();
    toast("构建配置已保存（演示）。");
    return;
  }
  if (action === "arg-add" || action === "env-add") {
    const temp = document.createElement("div");
    temp.innerHTML = kvRows([["", ""]], action === "env-add");
    el.previousElementSibling.append(temp.querySelector(".kv-row"));
    return;
  }
  if (action === "remove-row") {
    el.closest(".kv-row").remove();
    return;
  }
  if (action === "reveal") {
    const value = $('[name="value"]', el.closest(".kv-row"));
    value.type = value.type === "password" ? "text" : "password";
    return;
  }
  if (action === "env-save") {
    const keys = $$('[name="key"]', $("#env-form")).map((i) => i.value.trim());
    if (
      keys.some((k) => !k || !/^[A-Za-z_][A-Za-z0-9_]*$/.test(k)) ||
      keys.includes("DEVINGPANEL_IMAGE") ||
      new Set(keys).size !== keys.length
    )
      return toast(
        "变量名为空、格式不正确、重复或使用保留键 DEVINGPANEL_IMAGE。",
        true,
      );
    $$('[name="value"]', $("#env-form")).forEach((i) => (i.value = ""));
    toast("已验证变量格式；原型不保存敏感值，输入已清空。");
    return;
  }
  if (action === "file-add")
    return modal(
      "添加 Compose 文件",
      I18n.html`<form id="file-form" class="stack">${field("相对于项目根目录的路径", input("file", "", 'required placeholder="docker-compose.yml" class="mono"'))}<p class="muted">原型只维护文件列表，不读取磁盘或执行 Compose 校验。</p></form>`,
      button("取消", "close") + button("添加", "file-save", "primary"),
    );
  if (action === "file-save") {
    const f = $("#file-form");
    if (!f.reportValidity()) return;
    const name = $('[name="file"]').value.trim().replaceAll("\\", "/");
    const p = project();
    if (
      !/\.ya?ml$/i.test(name) ||
      name.startsWith("/") ||
      name.includes(":") ||
      name.split("/").includes("..") ||
      (!p.files.length && name.includes("/"))
    )
      return toast(
        "必须是项目内 YAML 文件，且首个文件必须位于项目根目录。",
        true,
      );
    if (p.files.includes(name)) return toast("文件已存在。", true);
    p.files.push(name);
    close();
    render();
    toast("文件列表已更新（尚未检查文件内容）。");
    return;
  }
  if (
    action === "file-up" ||
    action === "file-down" ||
    action === "file-remove"
  ) {
    const p = project();
    if (action === "file-remove") p.files.splice(id, 1);
    else {
      const to = id + (action === "file-up" ? -1 : 1);
      [p.files[id], p.files[to]] = [p.files[to], p.files[id]];
    }
    updateProjectReadiness(p);
    render();
    return;
  }
  if (action === "targets-save" || action === "deploy-targets-save") {
    if (action === "deploy-targets-save" && deployLocked())
      return toast("发布配置已锁定。", true);
    const ids = $$(
      '[name="target"]:checked',
      action === "targets-save" ? $("#content") : $("#dialog"),
    ).map((i) => Number(i.value));
    if (action === "targets-save") {
      project().hostIds = ids;
      updateProjectReadiness(project());
      toast("目标主机已保存。");
    } else {
      if (state.stage === "p0" && ids.length > 1)
        return toast("P0 仅支持单台 VPS。", true);
      state.selectedHosts = ids;
      state.parallel = Math.min(state.parallel, ids.length || 1);
      state.deploy = null;
      close();
    }
    render();
    return;
  }
  if (action === "deploy-targets" && deployLocked())
    return toast("发布配置已锁定。", true);
  if (action === "deploy-targets")
    return modal(
      "选择目标主机",
      I18n.html`<p class="muted">项目绑定的同组主机。</p>${targetTable(state.selectedHosts, true)}`,
      button("取消", "close") +
        button("确认", "deploy-targets-save", "primary"),
      true,
    );
  if (action === "deploy-start") return startDeploy();
  if (action === "deploy-stop")
    return confirmAction(
      "终止发布？",
      "不再启动新任务；已在运行的任务完成状态核实后展示。已成功主机不会自动回滚。",
      "deploy-stop-confirm",
      "终止发布",
    );
  if (action === "deploy-stop-confirm") {
    const d = state.deploy;
    close();
    if (!d) return;
    d.stopping = true;
    d.paused = false;
    if (!d.running) {
      finishDeploy(d);
      render();
    } else {
      d.message = "终止请求已接受，等待正在执行的任务达到可确认状态。";
      render();
    }
    return;
  }
  if (action === "deploy-reason")
    return modal(
      "失败原因",
      I18n.html`<div class="notice destructive">${esc(I18n.t(state.deploy?.rows[id]?.reason || "原因未记录，请核查远端容器与 Compose 状态。"))}</div>`,
      button("关闭", "close"),
    );
  if (action === "deploy-retry" || action === "deploy-skip")
    return resumeDeploy(id, action === "deploy-skip");
  if (action === "deploy-verify") {
    const d = state.deploy;
    if (!d) return;
    Object.assign(d.rows[id], {
      status: "失败",
      tone: "destructive",
      phase: "已核实：容器未就绪",
      reason: "核实后确认容器未就绪",
    });
    d.logs.push(`[演示] ${d.rows[id].name}：重新连接后确认容器未就绪`);
    d.message = d.stopping
      ? "远端状态已核实（演示），等待其余已启动任务完成。"
      : "远端状态已核实（演示），可重试或跳过。";
    d.paused = !d.stopping;
    if (d.stopping) finishDeploy(d);
    else saveDeployRecord(d);
    render();
    return;
  }
  if (action === "copy-log") {
    try {
      await navigator.clipboard.writeText(
        state.deploy?.logs.join("\n") || I18n.t("等待开始发布…"),
      );
      toast("日志已复制。");
    } catch {
      toast("浏览器不允许访问剪贴板，请选中日志手动复制。", true);
    }
    return;
  }
  if (action === "history-copy-log") {
    const record = history.find((item) => item.id === id);
    if (!record?.logs?.length) return;
    try {
      await navigator.clipboard.writeText(record.logs.join("\n"));
      toast("日志已复制。");
    } catch {
      toast("浏览器不允许访问剪贴板，请选中日志手动复制。", true);
    }
    return;
  }
  if (action === "history") return historyDetail(id);
  if (action === "history-demo") {
    const p = projects.find((item) => item.id === state.id);
    if (!p || history.some((record) => record.projectId === p.id)) return;
    const sample = history.filter((record) => record.projectId === 1).slice(0, 3);
    history.unshift(...sample.map((record, index) => ({
      ...structuredClone(record),
      id: Date.now() + index,
      projectId: p.id,
      project: p.name,
      rows: record.rows.map((host) => ({
        ...structuredClone(host),
        image: `${p.image}:${record.tag}-${host.arch}`,
      })),
    })));
    render();
    return;
  }
  if (action === "rollback") return rollbackDialog(id);
  if (action === "rollback-confirm") {
    const r = history.find((r) => r.id === id);
    const ids = $$('[name="rollback-host"]:checked').map((el) =>
      Number(el.value),
    );
    const selected = rollbackCandidates(r).filter(
      (c) => ids.includes(c.id) && !c.reason,
    );
    if (!selected.length)
      return toast("请选择具有完整历史材料的可回滚主机。", true);
    const results = selected.map((c) => ({
      ...structuredClone(c.previous.row),
      phase: "演示回滚完成",
      elapsed: "3s",
    }));
    history.unshift({
      id: Date.now(),
      time: new Date().toLocaleString(I18n.locale, { hour12: false }),
      project: r.project,
      projectId: r.projectId,
      tag: "rollback-" + timestamp(),
      result: results.some((h) => h.tone === "warning")
        ? "已启动，未验证就绪"
        : "成功",
      tone: results.some((h) => h.tone === "warning") ? "warning" : "success",
      duration: "3s",
      targets: ids,
      rows: results,
    });
    close();
    render();
    toast(
      I18n.msg`演示回滚完成：${selected.map((c) => c.name).join("、")}，真实 VPS 未发生变化。`,
    );
    return;
  }
  if (action === "logs") return showLogs(arg);
  if (action === "log-bottom") {
    const log = $("#container-log");
    log.scrollTop = log.scrollHeight;
    state.follow = true;
    $('[name="follow"]').checked = true;
    return;
  }
  if (
    ["container-stop", "container-restart", "container-delete"].includes(action)
  )
    return confirmAction(
      "确认容器操作",
      I18n.msg`确认${action === "container-stop" ? I18n.t("停止") : action === "container-delete" ? I18n.t("删除") : I18n.t("重启")}此容器？可能中断服务。`,
      `container-confirm:${action}:${arg}`,
    );
  if (action === "container-start" || action === "container-confirm") {
    const [op, cid] =
      action === "container-start"
        ? ["container-start", arg]
        : [parts[0], parts.slice(1).join(":")];
    const c = containers.find((c) => c.id === cid);
    if (!c) return;
    if (op === "container-delete") containers.splice(containers.indexOf(c), 1);
    else c.state = op === "container-stop" ? "exited" : "running";
    close();
    render();
    toast("容器操作完成（仅演示）。");
    return;
  }
  if (action === "image-delete")
    return confirmAction(
      "删除镜像？",
      I18n.msg`将删除 ${esc(arg)}。当前运行与保留版本受到保护。`,
      `image-delete-confirm:${arg}`,
      "删除",
    );
  if (action === "image-delete-confirm") {
    state.imagesRemoved.push(state.containerHost + arg);
    close();
    render();
    toast("镜像已删除（演示）。");
    return;
  }
  if (action === "image-prune")
    return toast("需先接入发布历史保护；无标签镜像仍可能用于回滚。", true);
  if (action === "prune")
    return modal(
      "清理磁盘",
      I18n.html`<div class="stack"><p class="muted">docker system df · 演示预估</p>${table(
        ["资源", "预估可回收"],
        [
          ["悬空镜像", "120 MB"],
          ["停止容器", "32 MB"],
          ["未使用网络", "—"],
          ["构建缓存", "256 MB"],
        ].map((cells) => row(cells.map(I18n.t))),
      )}<div class="notice warning">这是全局 Docker 清理，可能删除其他项目的停止容器、未使用网络及缓存。估值不等于实际释放空间。不清理卷。</div></div>`,
      button("取消", "close") +
        button("继续", "prune-confirm-dialog", "danger"),
    );
  if (action === "prune-confirm-dialog")
    return confirmAction(
      "确认全局 Docker 清理",
      "此操作可能影响其他项目，也可能删除未被容器引用但仍用于回滚的无标签镜像。发布历史镜像清理请使用设置中的独立入口。",
      "prune-confirm",
      "确认清理",
    );
  if (action === "prune-confirm") {
    close();
    toast("演示清理完成：实际释放 308 MB（模拟数据）。");
    return;
  }
  if (action === "managed-clean") {
    const preview = managedCleanupPreview();
    return modal(
      "清理托管历史镜像",
      I18n.html`<div class="stack"><p class="muted">按当前会话发布历史和保留数计算；不执行 docker system prune 或远端清理。</p><p>受保护版本：${preview.protectedCount} · 待处理版本：${preview.candidates.length}</p>${preview.candidates.length ? table(
        ["主机", "项目", "待清理版本", "状态"],
        preview.candidates.map(({ host, project, record, eligible }) => row([esc(host.name), esc(project.name), esc(record.tag), eligible ? I18n.t("可清理（演示）") : I18n.t("离线，待重连")])),
      ) : I18n.t('<div class="notice muted">当前没有超出保留数的已完成版本。</div>')}<div class="notice warning">真实清理还需核对远端镜像、引用及版本目录；此页面仅更新演示历史状态。</div></div>`,
      button("取消", "close") + button("确认清理", "managed-clean-confirm", "primary", preview.candidates.some((item) => item.eligible) ? "" : "disabled"),
      true,
    );
  }
  if (action === "managed-clean-confirm") {
    const preview = managedCleanupPreview();
    const eligible = preview.candidates.filter((item) => item.eligible);
    eligible.forEach(({ entry }) => { entry.materials = false; entry.cleanup = "已清理（演示）"; });
    close();
    render();
    toast(I18n.msg`演示历史材料已标记清理 ${eligible.length} 项；未操作 VPS。`);
    return;
  }
  if (action === "registry-test") {
    const address = $('[name="registry"]').value.trim();
    if (!address) return toast("请填写 Registry 地址。", true);
    $('[name="registry-token"]').value = "";
    toast("仅演示登录检查；未验证凭据或更改传输方式。");
    return;
  }
  if (action === "registry-save") {
    if (deployLocked()) return toast("发布尚未结束，不能修改镜像仓库配置。", true);
    const address = $('[name="registry"]').value.trim();
    if (!/^[a-z0-9.-]+(?::[0-9]{1,5})?$/i.test(address)) return toast("请填写 HTTPS Registry 域名，可包含端口。", true);
    state.registryAddress = address;
    state.registryUser = $('[name="registry-user"]').value.trim();
    state.registryDraftAddress = state.registryAddress;
    state.registryDraftUser = state.registryUser;
    state.registryConfigured = true;
    $('[name="registry-token"]').value = "";
    render();
    toast("仓库配置已保存到会话演示；令牌未保存。");
    return;
  }
  if (action === "registry-remove") {
    if (deployLocked()) return toast("发布尚未结束，不能修改镜像仓库配置。", true);
    return confirmAction("移除镜像仓库配置？", "移除当前会话中的地址和用户名，发布方式切回 SSH 直连。原型未保存令牌。", "registry-remove-confirm", "移除配置");
  }
  if (action === "registry-remove-confirm") {
    if (deployLocked()) return toast("发布尚未结束，不能修改镜像仓库配置。", true);
    Object.assign(state, { registry: false, registryConfigured: false, registryAddress: "", registryUser: "", registryDraftAddress: "", registryDraftUser: "" });
    close();
    render();
    return toast("镜像仓库配置已移除；发布方式为 SSH 直连（演示）。");
  }
  if (action === "docker-detect") {
    toast(
      state.docker
        ? "检测完成：Docker 可用（演示）。"
        : "未检测到 Docker，请查看安装指引。",
      !state.docker,
    );
    return;
  }
  if (action === "native-folder")
    return toast(
      "浏览器原型不支持打开系统文件夹；桌面客户端将调用原生文件管理器。",
    );
});
document.addEventListener("change", (e) => {
  const el = e.target;
  const name = el.name;
  if (name === "screen-language") {
    ScreenLock.setLanguage(el.value);
    return;
  }
  if (ScreenLock.isLocked) return;
  if (name === "screen-interval") {
    ScreenLock.onIntervalChange(el);
    return;
  }
  if (name === "language") {
    I18n.setLocale(el.value);
    $("#toasts").replaceChildren();
    render();
    toast("语言已更新。");
    return;
  }
  if (!state.docker) return;
  if (name === "settings-section") {
    location.hash = `#/settings/${el.value}`;
    return;
  }
  if (
    name.startsWith("s3-") ||
    name.startsWith("backup-") ||
    el.id === "backup-file"
  ) {
    BackupUI.handleChange(el);
    return;
  }
  if (name === "host-group") {
    state.group = el.value;
    render();
  }
  if (name === "theme") {
    theme(el.value);
    render();
    toast("主题已保存。");
  }
  if (name === "auth" || name === "connection-mode") updateHostForm();
  if (name === "sensitive" && !el.checked) {
    toast(
      "关闭敏感标记后，实际客户端将以非秘密配置写入 SQLite。原型不会保存此值。",
    );
  }
  if (name === "deploy-project") {
    if (deployLocked()) return render();
    const p = projects.find((p) => p.id === Number(el.value));
    state.selectedProject = p.id;
    state.selectedHosts = [...p.hostIds].slice(
      0,
      state.stage === "p0" ? 1 : undefined,
    );
    state.deploy = null;
    state.parallel = 1;
    render();
  }
  if (name === "parallel") state.parallel = Number(el.value);
  if (name === "container-host")
    location.hash = `#/containers/${el.value}/${state.tab || "containers"}`;
  if (name === "retention") {
    if (el.value === "custom") state.keep = 4;
    else state.keep = Number(el.value);
    render();
    toast("保留策略已更新（演示）。");
  }
  if (name === "retention-custom") {
    if (!el.reportValidity()) return;
    state.keep = Number(el.value);
    render();
    toast("保留策略已更新（演示）。");
  }
  if (name === "host-collection-interval") {
    state.hostCollectionCustom = el.value === "custom";
    if (!state.hostCollectionCustom) state.hostCollectionIntervalSeconds = Number(el.value);
    render();
    if (!state.hostCollectionCustom) toast("主机数据采集间隔已更新（演示）。");
  }
  if (name === "host-collection-custom") {
    if (!el.reportValidity()) return;
    state.hostCollectionIntervalSeconds = Number(el.value);
    render();
    toast("主机数据采集间隔已更新（演示）。");
  }
  if (name === "registry-enabled") {
    if (deployLocked() || !state.registryConfigured) return;
    state.registry = el.checked;
    toast("发布传输方式已更新（演示）。");
  }
  if (name === "auto-cleanup") {
    state.autoCleanup = el.checked;
    render();
    toast("自动清理设置已更新（演示）。");
  }
  if (name === "registry") state.registryDraftAddress = el.value.trim();
  if (name === "registry-user") state.registryDraftUser = el.value;
  if (name === "env-scope") {
    $$('[name="value"]', $("#env-form")).forEach((i) => (i.value = ""));
    toast("已切换配置范围；原型不保存变量值。");
  }
  if (name.startsWith("threshold-") || name === "alert-duration") {
    if (!el.reportValidity()) return;
    if (name === "alert-duration") state.alerts.duration = el.value;
    else state.alerts[name.slice("threshold-".length)] = Number(el.value);
    toast("告警配置已更新（演示，不发送系统通知）。");
  }
  if (name === "follow") {
    state.follow = el.checked;
    if (state.follow) {
      const log = $("#container-log");
      log.scrollTop = log.scrollHeight;
    }
  }
  if (name === "project-group") {
    const p = project();
    const chosen = hosts.filter((h) => h.group === el.value);
    $("#target-list").innerHTML = table(
      ["顺序", "选择", "状态", "主机名", "地址", "分组"],
      chosen.map((h, i) =>
        row([
          I18n.html`<span>${i + 1}</span>`,
          I18n.html`<input type="checkbox" name="target" value="${h.id}" ${p.hostIds.includes(h.id) ? "checked" : ""} aria-label="选择 ${esc(h.name)}">`,
          badge(
            h.online ? "在线" : "离线",
            h.online ? "success" : "destructive",
          ),
          esc(h.name),
          esc(h.address),
          esc(h.group),
        ]),
      ),
    );
  }
});
document.addEventListener(
  "toggle",
  (e) => {
    const m = e.target;
    if (!m.matches("details.menu") || !m.open) return;
    const list = $(".menu-list", m),
      rect = $("summary", m).getBoundingClientRect();
    list.style.left =
      Math.max(
        8,
        Math.min(
          innerWidth - list.offsetWidth - 8,
          rect.right - list.offsetWidth,
        ),
      ) + "px";
    list.style.top =
      (rect.bottom + list.offsetHeight > innerHeight - 8
        ? Math.max(8, rect.top - list.offsetHeight)
        : rect.bottom) + "px";
  },
  true,
);
document.addEventListener("click", (e) => {
  $$("details.menu[open]").forEach((m) => {
    if (!m.contains(e.target)) m.open = false;
  });
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    $$("details.menu[open]").forEach((m) => (m.open = false));
    $("#sidebar").classList.remove("open");
  }
});
window.addEventListener("resize", () =>
  $$("details.menu[open]").forEach((m) => (m.open = false)),
);
$$("dialog").forEach((d) =>
  d.addEventListener("close", () => {
    if (!d.open) {
      d.innerHTML = "";
    }
    if (d.id === "sheet") clearInterval(logTimer);
  }),
);
$("#dialog").addEventListener("cancel", () => BackupUI.onClose());
$("#dialog").addEventListener("cancel", () => ScreenLock.onDialogDismiss());
window.addEventListener("hashchange", parseRoute);
parseRoute();
ScreenLock.init();
