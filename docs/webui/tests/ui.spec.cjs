const { test, expect } = require("@playwright/test");
test.beforeEach(async ({ page }) => {
  page.scriptErrors = [];
  page.on("pageerror", (e) => page.scriptErrors.push(e.message));
});
test.afterEach(async ({ page }) => {
  expect(page.scriptErrors).toEqual([]);
});
test("theme values match both documented base palettes", async ({ page }) => {
  const fs = require("node:fs");
  const path = require("node:path");
  const doc = fs.readFileSync(
    path.join(__dirname, "../../design/00-overview.md"),
    "utf8",
  );
  await page.goto("/");
  for (const [heading, theme] of [
    ["### 3.1", "light"],
    ["### 3.2", "dark"],
  ]) {
    const block = doc
      .slice(doc.indexOf(heading))
      .match(/```css\s*([\s\S]*?)```/)[1];
    if (
      Boolean((await page.locator("html").getAttribute("class"))?.includes("dark")) !==
      (theme === "dark")
    )
      await page.locator('[data-action="theme"]').click();
    for (const [, name, value] of block.matchAll(/(--[\w-]+):\s*([^;]+);/g)) {
      expect(
        await page.evaluate(
          (name) =>
            getComputedStyle(document.documentElement)
              .getPropertyValue(name)
              .trim(),
          name,
        ),
      ).toBe(value.trim());
    }
  }
});
test("Registry push has separate progress and multi-host health preflight runs after build", async ({
  page,
}) => {
  await page.goto("/#/settings/registry");
  await page.locator('[name="registry"]').fill("registry.example.com");
  await page.locator('[name="registry"]').blur();
  await page.getByRole("button", { name: "保存配置（演示）" }).click();
  await page.locator('[name="registry-enabled"]').check();
  await page.locator('nav a[href="#/deploy"]').click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.locator("#build-log")).toContainText("本地各架构 push 100%", { timeout: 10000 });
  await expect(page.locator("tbody")).toContainText("拉取镜像");
  await expect(page.getByRole("heading", { name: "发布进度总览" })).toHaveCount(0);
  await page.reload();
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-scenario"]').selectOption("unverified");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.locator("#build-log")).toContainText("目标平台：linux/amd64");
  await expect(page.getByText("构建后预检失败", { exact: true })).toBeVisible();
  await expect(page.locator("tbody")).not.toContainText("传输中");
  await expect(page.locator("tbody .badge")).toHaveText(["等待", "等待"]);
});
test("all specified pages render without errors and fit desktop/mobile", async ({
  page,
}) => {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  const routes = [
    "dashboard",
    "hosts",
    "hosts/1/monitor",
    "hosts/1/info",
    "projects",
    "projects/1/build",
    "projects/1/compose",
    "projects/1/env",
    "projects/1/targets",
    "projects/1/history",
    "deploy",
    "deploy/0/history",
    "containers/1/containers",
    "containers/1/images",
    "settings",
    "settings/appearance",
    "settings/registry",
    "settings/backup",
    "settings/environment",
    "settings/security",
  ];
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    for (const route of routes) {
      await page.goto("/#/" + route);
      await expect(page.locator("h1")).toBeVisible();
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
      await page.screenshot({
        path: `test-results/screens/${width}-${route.replaceAll("/", "-")}.png`,
        fullPage: true,
      });
    }
  }
  expect(errors).toEqual([]);
});
test("platform mismatch and invalid Compose ordering block deployment", async ({
  page,
}) => {
  await page.goto("/#/projects/1/build");
  await page.locator('[name="platform"]').selectOption("linux/arm64");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await page.locator('nav a[href="#/deploy"]').click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(
    page.getByText("目标平台与所选主机架构不匹配，未开始构建或传输。"),
  ).toBeVisible();
  await page.locator('nav a[href="#/projects"]').click();
  await page.getByRole("link", { name: "myapp", exact: true }).click();
  await page.getByRole("tab", { name: "Compose", exact: true }).click();
  await page.getByRole("button", { name: "添加 compose 文件" }).click();
  await page.locator('[name="file"]').fill("sub/prod.yml");
  await page.getByRole("button", { name: "添加", exact: true }).click();
  await page.locator('[data-action="file-up:2"]').click();
  await page.locator('[data-action="file-up:1"]').click();
  await expect(
    page.getByText("首个 Compose 文件必须位于项目根目录。", { exact: true }),
  ).toBeVisible();
  await expect(page.locator('[data-action="project-deploy:1"]')).toBeDisabled();
});
test("registry address survives navigation and rollback respects per-host availability", async ({
  page,
}) => {
  await page.goto("/#/settings/registry");
  await page.locator('[name="registry"]').fill("registry.example.com/team");
  await page.locator('[name="registry"]').blur();
  await page.locator('nav a[href="#/dashboard"]').click();
  await page.locator('nav a[href="#/settings"]').click();
  await page.locator('.settings-nav a[href="#/settings/registry"]').click();
  await expect(page.locator('[name="registry"]')).toHaveValue(
    "registry.example.com/team",
  );
  await page.locator('nav a[href="#/deploy"]').click();
  await page.getByRole("tab", { name: "历史", exact: true }).click();
  await page.locator('[data-action="rollback:1"]').click();
  await expect(page.locator('[name="rollback-host"][value="1"]')).toBeChecked();
  await expect(
    page.locator('[name="rollback-host"][value="2"]'),
  ).toBeDisabled();
  await expect(page.getByText("无上一已完成版本")).toBeVisible();
});
test("new untrusted host is offline until fingerprint confirmation", async ({
  page,
}) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机", exact: true }).click();
  await page.locator('[name="name"]').fill("new-untrusted");
  await page.locator('[name="address"]').fill("192.0.2.22");
  await page
    .locator("#dialog")
    .getByRole("button", { name: "创建", exact: true })
    .click();
  const host = page.getByRole("row").filter({ hasText: "new-untrusted" });
  await expect(host).toContainText("离线");
  await host.getByRole("button", { name: "测试连接", exact: true }).click();
  await expect(page.getByText("首次连接：确认主机指纹")).toBeVisible();
  await page.getByRole("button", { name: "确认演示指纹" }).click();
  await expect(host).toContainText("在线");
});
test("host list shows resource usage only for available hosts", async ({ page }) => {
  await page.goto("/#/hosts");
  const headers = page.locator("thead th");
  await expect(headers).toContainText(["状态", "别名", "地址", "用户", "分组", "容器数", "CPU", "内存", "磁盘", "最近连接", "操作"]);
  const firstHost = page.getByRole("row").filter({ hasText: "web-1" });
  await expect(firstHost).toContainText("42%");
  await expect(firstHost).toContainText("61%");
  await expect(firstHost).toContainText("38%");
  await expect(page.getByRole("row").filter({ hasText: "web-3" }).locator("td").nth(6)).toHaveText("--");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-host-docker-missing"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByRole("row").filter({ hasText: "web-1" }).locator("td").nth(6)).toHaveText("--");
});
test("fingerprint confirmation is bound to the tested endpoint", async ({
  page,
}) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机", exact: true }).click();
  await page.locator('[name="name"]').fill("changed-target");
  await page.locator('[name="address"]').fill("192.0.2.10");
  await page.locator('[data-action="host-form-test"]').click();
  await page.locator('[name="address"]').fill("192.0.2.11");
  await page.getByRole("button", { name: "确认演示指纹" }).click();
  await expect(
    page.getByText("连接目标已变化，请重新测试连接并核对指纹。"),
  ).toBeVisible();
  await page.locator('[data-action="host-save"]').click();
  await expect(
    page.getByRole("row").filter({ hasText: "changed-target" }),
  ).toContainText("离线");
});
test("preview loading empty error and P0/P2 views", async ({ page }) => {
  await page.goto("/");
  for (const mode of ["loading", "empty", "error", "normal"]) {
    await page.getByRole("button", { name: "原型预览选项" }).click();
    await page.locator('[name="preview-view"]').selectOption(mode);
    await page.getByRole("button", { name: "应用", exact: true }).click();
    if (mode === "loading")
      await expect(page.locator(".skeleton-row")).toHaveCount(5);
    if (mode === "empty")
      await expect(page.getByText("添加你的第一台 VPS")).toBeVisible();
    if (mode === "error")
      await expect(
        page.getByRole("button", { name: "重试", exact: true }),
      ).toBeVisible();
  }
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-stage"]').selectOption("p0");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.locator('nav a[href="#/containers"]')).toHaveCount(0);
  await expect(page.locator(".hosts-grid")).not.toContainText("CPU");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-stage"]').selectOption("p2");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.locator("a.host-card").first().click();
  await expect(page.getByText("资源趋势", { exact: true })).toBeVisible();
});

test("missing local Docker gates every feature until availability is restored", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("devingpanel-theme", "light"));
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-docker"]').uncheck();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByRole("heading", { name: "请先安装本机 Docker" })).toBeVisible();
  await expect(page.locator("#sidebar nav a")).toHaveCount(0);
  await expect(page.locator("#sidebar nav .nav-disabled")).toHaveCount(6);
  await page.screenshot({ path: "test-results/docker-setup-desktop.png" });
  await page.setViewportSize({ width: 390, height: 800 });
  await page.locator('[data-action="theme"]').click();
  await expect(page.locator("html")).toHaveClass("dark");
  await expect(page.getByRole("heading", { name: "请先安装本机 Docker" })).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: "test-results/docker-setup-mobile-dark.png" });
  await page.goto("/#/projects/1");
  await expect(page.getByRole("heading", { name: "请先安装本机 Docker" })).toBeVisible();
  await expect(page.locator('[data-action="project-edit"]')).toHaveCount(0);
  await page.getByRole("button", { name: "查看安装指引" }).click();
  await expect(page.getByRole("dialog")).toContainText("Windows · Docker Desktop");
  await expect(page.getByRole("dialog")).toContainText("Linux · Docker Engine");
  await page.locator('#dialog [data-action="close"]').first().click();
  await page.getByRole("button", { name: "重新检测" }).click();
  await expect(page.getByText("未检测到 Docker，请查看安装指引。")).toBeVisible();
  await page.locator('.docker-setup [data-action="preview"]').click();
  await page.locator('[name="preview-docker"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByRole("heading", { name: "请先安装本机 Docker" })).toHaveCount(0);
  await expect(page.locator("#sidebar nav a")).toHaveCount(6);
  await expect(page.locator("#content h1")).toHaveText("myapp");
});
test("Docker-missing host shows installation confirmation without running it in the prototype", async ({ page }) => {
  await page.goto("/#/hosts/1/info");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-host-docker-missing"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByText("SSH 已连接 · Docker 未安装")).toBeVisible();
  await page.getByRole("button", { name: "一键安装 Docker" }).click();
  await expect(page.getByRole("dialog")).toContainText("Docker 官方指纹");
  await page.getByRole("button", { name: "使用腾讯云镜像安装" }).click();
  await expect(page.getByText("原型不执行安装；桌面客户端需通过确认后才会操作 VPS。"))
    .toBeVisible();
});
test("host mirror configuration preview requires confirmation and never changes a VPS", async ({ page }) => {
  await page.goto("/#/hosts/1/info");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-mirrors"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "配置镜像源" }).click();
  await expect(page.getByRole("dialog")).toContainText("https://docker.1panel.live");
  await expect(page.getByRole("dialog")).toContainText("https://docker.m.daocloud.io");
  await page.getByRole("button", { name: "确认配置" }).click();
  await expect(page.getByText("原型不修改 VPS；桌面客户端需通过确认后才会配置镜像源。"))
    .toBeVisible();
});
test("YAML preview requires directory authorization before saving", async ({ page }) => {
  await page.goto("/#/projects/1/build");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-yaml-project"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByText("开发者维护的非秘密发布方案")).toBeVisible();
  await expect(page.locator("#build-form")).toHaveCount(0);
  await page.locator('[name="manifest-tag"]').fill("release-{timestamp}");
  await page.getByRole("tab", { name: "Compose" }).click();
  await page.getByRole("tab", { name: "构建配置" }).click();
  await expect(page.locator('[name="manifest-tag"]')).toHaveValue("release-{timestamp}");
  await expect(page.getByRole("button", { name: "保存到 devingpanel.yaml" })).toBeDisabled();
  await expect(page.getByRole("button", { name: "重新读取配置" })).toBeDisabled();
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-yaml-error"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByRole("button", { name: "选择项目目录" })).toBeVisible();
});
test("YAML editor exposes multiple builds but does not publish an unsaved draft", async ({ page }) => {
  await page.goto("/#/projects/1/build");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-yaml-project"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "添加构建" }).click();
  await page.locator('[name="manifest-service-1"]').fill("worker");
  await page.locator('[name="manifest-image-1"]').fill("worker");
  await expect(page.getByRole("button", { name: "发布", exact: true })).toBeDisabled();
  await page.getByRole("tab", { name: "目标主机" }).click();
  await page.getByRole("button", { name: "添加目标" }).click();
  await page.locator('[name="manifest-group-3"]').fill("myapp-prod");
  await page.locator('[name="manifest-host-3"]').fill("missing-host");
  await expect(page.getByText("目标主机缺失或名称不唯一", { exact: false })).toBeVisible();
});
test("theme tokens and shell match specification", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator("#sidebar")).toHaveCSS("width", "224px");
  await expect(page.locator("#topbar")).toHaveCSS("height", "56px");
  await page.locator('[data-action="theme"]').click();
  await expect(page.locator("html")).toHaveClass("dark");
  await expect(page.locator("#dialog[open]")).toHaveCount(0);
  await page.reload();
  await expect(page.locator("html")).toHaveClass("dark");
  await page.locator('[data-action="theme"]').click();
  await expect(page.locator("html")).not.toHaveClass("dark");
  await page.reload();
  await expect(page.locator("html")).not.toHaveClass("dark");
  await page.locator('[data-action="theme"]').click();
  await expect(page.locator("html")).toHaveClass("dark");
  await page.screenshot({
    path: "test-results/dashboard-dark.png",
    fullPage: true,
  });
});
test("host form and project creation are usable", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机", exact: true }).click();
  await page.locator('[name="name"]').fill("test-host");
  await page.locator('[name="address"]').fill("192.0.2.1");
  await page
    .locator("#dialog")
    .getByRole("button", { name: "测试连接", exact: true })
    .click();
  await expect(page.getByText("首次连接：核对 SSH 主机指纹")).toBeVisible();
  await page.getByRole("button", { name: "确认演示指纹" }).click();
  await page.getByRole("button", { name: "创建", exact: true }).click();
  await expect(page.getByText("test-host", { exact: true })).toBeVisible();
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "新建项目", exact: true }).click();
  await page.locator('[name="name"]').fill("demo-app");
  await page.locator('[name="path"]').fill("D:\\demo-app");
  await page.getByRole("button", { name: "创建项目", exact: true }).click();
  await expect(page.locator("h1")).toHaveText("demo-app");
  await expect(page.locator('[data-action^="project-deploy:"]')).toBeDisabled();
});
test("SSH host form switches connection settings and validates the active mode", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机", exact: true }).click();
  const form = page.locator("#host-form");
  await expect(form.locator(".host-protocol")).toHaveText("SSH");
  await expect(form.getByText("Telnet")).toHaveCount(0);
  await expect(form.getByText("串口")).toHaveCount(0);
  await expect(form.locator("#jump-settings")).toBeHidden();
  await page.screenshot({ path: "test-results/host-form-direct.png" });
  await form.locator('[name="credential"]').fill("demo-only");
  await form.locator('[data-action="host-password-toggle"]').first().click();
  await expect(form.locator('[name="credential"]')).toHaveAttribute("type", "text");
  await form.locator('[name="connection-mode"][value="jump"]').check();
  await expect(form.locator("#jump-settings")).toBeVisible();
  await form.locator('[data-action="host-jump-add"]').click();
  await expect(form.locator('[name="jump-host"]')).toHaveCount(2);
  await form.locator('[name="jump-host"]').first().selectOption("1");
  await form.locator('[name="jump-host"]').last().selectOption("2");
  await form.locator('[name="connection-mode"][value="proxy"]').check();
  await expect(form.locator("#proxy-settings")).toBeVisible();
  await page.screenshot({ path: "test-results/host-form-proxy.png" });
  await expect(form.locator('[name="jump-host"]').first()).toBeDisabled();
  await form.locator('[name="address"]').fill("192.0.2.50");
  await page.locator('[data-action="host-save"]').click();
  await expect(form).toBeVisible();
  await form.locator('[name="proxy-address"]').fill("127.0.0.1");
  await form.locator('[name="proxy-port"]').fill("8080");
  await form.locator('[name="proxy-type"]').selectOption("socks5");
  await page.setViewportSize({ width: 390, height: 800 });
  const fitsMobile = await form.evaluate((element) => element.scrollWidth <= element.clientWidth);
  expect(fitsMobile).toBe(true);
  await page.locator('[data-action="host-save"]').click();
  await expect(page.getByRole("row").filter({ hasText: "192.0.2.50" })).toBeVisible();
});
test("changing the SSH route after fingerprint confirmation requires a new test", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机", exact: true }).click();
  const form = page.locator("#host-form");
  await form.locator('[name="address"]').fill("192.0.2.51");
  await page.locator('[data-action="host-form-test"]').click();
  await page.getByRole("button", { name: "确认演示指纹" }).click();
  await form.locator('[name="connection-mode"][value="proxy"]').check();
  await form.locator('[name="proxy-address"]').fill("127.0.0.1");
  await form.locator('[name="proxy-port"]').fill("8080");
  await page.locator('[data-action="host-save"]').click();
  await expect(page.getByRole("row").filter({ hasText: "192.0.2.51" })).toContainText("离线");
});
test("host menu duplicates configuration without trust or credentials and opens the in-app terminal", async ({ page }) => {
  await page.goto("/#/hosts");
  const source = page.getByRole("row").filter({ hasText: "web-1" });
  await source.locator("summary").click();
  const menuItems = source.locator(".menu-list > *");
  await expect(menuItems).toHaveCount(5);
  for (const item of await menuItems.all()) await expect(item.locator("svg.icon")).toHaveCount(1);
  await page.screenshot({ path: "test-results/host-menu-icons.png" });
  await source.locator('[data-action="host-connect:1"]').click();
  await expect(page).toHaveURL(/#\/hosts$/);
  await expect(page.locator("#dialog-title")).toHaveText("连接");
  await expect(page.locator("#ssh-terminal-output")).toContainText("Linux demo-debian");
  await expect(page.locator("#ssh-terminal-output")).toContainText("Last login:");
  await expect(page.locator(".ssh-terminal-prompt > span:first-child")).toHaveText("deploy@web-1:~$ ");
  await expect(page.locator(".terminal-cursor")).toBeVisible();
  await page.screenshot({ path: "test-results/host-terminal.png" });
  await expect(source.locator("details.menu")).not.toHaveAttribute("open", "");
  await page.setViewportSize({ width: 390, height: 800 });
  const terminalFits = await page.locator("#dialog").evaluate((element) => {
    const rect = element.getBoundingClientRect();
    return rect.left > 0 && rect.top > 0 && rect.right < window.innerWidth && rect.bottom < window.innerHeight;
  });
  expect(terminalFits).toBe(true);
  await expect(page.locator('a[href^="ssh://"]')).toHaveCount(0);
  await page.locator('[name="terminal-command"]').fill("pwd");
  await expect(page.locator(".terminal-cursor")).toBeHidden();
  await page.locator('[name="terminal-command"]').press("Enter");
  await expect(page.locator("#ssh-terminal-output")).toContainText("deploy@web-1:~$ pwd\n演示终端不会执行远程命令。");
  await expect(page.locator(".ssh-terminal-prompt > span:first-child")).toHaveText("deploy@web-1:~$ ");
  await expect(page.locator(".terminal-cursor")).toBeVisible();
  await page.locator('#dialog .dialog-head [data-action="close"]').click();
  await source.locator("summary").click();
  await source.locator('[data-action="host-copy:1"]').click();
  const form = page.locator("#host-form");
  await expect(page.locator("#dialog-title")).toHaveText("复制主机");
  await expect(form.locator('[name="name"]')).toHaveValue("web-1 副本");
  await expect(form.locator('[name="address"]')).toHaveValue("203.0.113.10");
  await expect(form.locator('[name="credential"]')).toBeEmpty();
  await expect(form).toHaveAttribute("data-id", "0");
  await page.locator('[data-action="host-save"]').click();
  await expect(page.getByRole("row").filter({ hasText: "web-1 副本" })).toContainText("离线");
  await expect(source.first()).toContainText("在线");
  await page.goto("/#/hosts/1");
  await page.getByRole("button", { name: "连接", exact: true }).click();
  await expect(page.locator(".ssh-terminal-prompt > span:first-child")).toHaveText("deploy@web-1:~$ ");
});
test("host groups can be created, renamed, and removed when empty", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "管理分组" }).click();
  await expect(page.locator(".group-row")).toHaveCount(1);
  await expect(page.locator(".group-row")).toContainText("myapp-prod");
  await page.locator('[name="group-name"]').fill("staging");
  await page.locator('[name="group-name"]').press("Enter");
  await expect(page.locator(".group-row")).toHaveCount(2);
  await page.locator('#dialog .dialog-foot [data-action="close"]').click();
  await page.locator('[name="host-group"]').selectOption("staging");
  await expect(page.getByText("还没有主机")).toBeVisible();
  await page.locator('button[data-action="host-new"]').first().click();
  await page.locator('.host-group-control [data-action="host-group-new"]').click();
  await page.locator('[name="new-host-group"]').fill("qa");
  await page.locator('[data-action="host-group-create"]').click();
  await expect(page.locator('#host-form [name="group"]')).toHaveValue("qa");
  await page.locator('#host-form [name="address"]').fill("192.0.2.80");
  await page.locator('[data-action="host-save"]').click();
  await page.locator('[name="host-group"]').selectOption("qa");
  await expect(page.getByRole("row").filter({ hasText: "192.0.2.80" })).toContainText("qa");
  await page.getByRole("button", { name: "管理分组" }).click();
  await page.locator('[data-action="group-rename:2"]').click();
  await page.locator('[name="rename-group"]').fill("qa-new");
  await page.locator('[data-action="group-rename-save:2"]').click();
  await expect(page.locator('[name="host-group"]')).toHaveValue("qa-new");
  await page.locator('[data-action="group-delete:2"]').click();
  await expect(page.getByText("请先将组内主机移至其他分组。")).toBeVisible();
  await page.locator('[data-action="group-delete:1"]').click();
  await expect(page.locator(".group-row")).toHaveCount(2);
  await page.locator('#dialog .dialog-foot [data-action="close"]').click();
  await expect(page.getByRole("row").filter({ hasText: "192.0.2.80" })).toContainText("qa-new");
});
test("YAML project targets prevent host group renaming", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-yaml-project"]').check();
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.goto("/#/projects/1/targets");
  await expect(page.locator('[name="manifest-group-0"]')).toHaveValue("myapp-prod");
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "管理分组" }).click();
  await page.locator('[data-action="group-rename:0"]').click();
  await expect(page.getByText("该分组已被项目 YAML 目标引用，请先调整项目目标。")).toBeVisible();
  await expect(page.locator('[name="rename-group"]')).toHaveCount(0);
});

test("host group management fits mobile and dark theme", async ({ page }) => {
  for (const [theme, width, height] of [["light", 1440, 900], ["light", 390, 800], ["dark", 390, 800]]) {
    await page.setViewportSize({ width, height });
    await page.goto("/#/hosts");
    await page.evaluate((value) => localStorage.setItem("devingpanel-theme", value), theme);
    await page.reload();
    await page.getByRole("button", { name: "管理分组" }).click();
    const fits = await page.locator("#dialog").evaluate((element) => {
      const rect = element.getBoundingClientRect();
      return rect.left >= 0 && rect.right <= innerWidth && rect.top >= 0 && rect.bottom <= innerHeight && element.scrollWidth <= element.clientWidth;
    });
    expect(fits).toBe(true);
    await page.screenshot({ path: `test-results/groups-${theme}-${width}.png` });
    await page.locator('#dialog .dialog-foot [data-action="close"]').click();
    await page.locator('button[data-action="host-new"]').first().click();
    await page.locator('.host-group-control [data-action="host-group-new"]').click();
    await expect(page.locator("#new-host-group")).toBeVisible();
    await page.screenshot({ path: `test-results/group-form-${theme}-${width}.png` });
  }
});
test("SSH terminal follows the application theme", async ({ page }) => {
  await page.addInitScript(() => localStorage.setItem("devingpanel-theme", "light"));
  await page.goto("/#/hosts");
  const openTerminal = async () => {
    const host = page.getByRole("row").filter({ hasText: "web-1" });
    await host.locator("summary").click();
    await host.locator('[data-action="host-connect:1"]').click();
  };
  const colors = () => page.evaluate(() => {
    const style = (selector) => getComputedStyle(document.querySelector(selector));
    return {
      background: style(".ssh-terminal").backgroundColor,
      text: style(".ssh-terminal").color,
      dialog: style("#dialog").backgroundColor,
      close: style(".terminal-dialog .dialog-head button").color,
      input: style('[name="terminal-command"]').color,
      cursor: style(".terminal-cursor").backgroundColor,
      pageBackground: getComputedStyle(document.body).backgroundColor,
      pageText: getComputedStyle(document.body).color,
    };
  });
  await openTerminal();
  const light = await colors();
  await page.locator('#dialog .dialog-head [data-action="close"]').click();
  await page.locator('[data-action="theme"]').click();
  await openTerminal();
  const dark = await colors();
  await page.screenshot({ path: "test-results/host-terminal-dark.png" });
  expect(light.background).not.toBe(dark.background);
  expect(light.text).not.toBe(dark.text);
  for (const palette of [light, dark]) {
    expect(palette.background).toBe(palette.pageBackground);
    expect(palette.dialog).toBe(palette.background);
    expect(palette.text).toBe(palette.pageText);
    expect(palette.close).toBe(palette.text);
    expect(palette.input).toBe(palette.text);
    expect(palette.cursor).toBe(palette.text);
  }
});
test("deployment succeeds and records history", async ({ page }) => {
  await page.goto("/#/deploy");
  const tag = await page.locator("#release-tag").textContent();
  await expect(page.locator('[name="release-tag"]')).toHaveCount(0);
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.locator('[name="deploy-project"]')).toBeDisabled();
  await expect(
    page.locator(".release-message").filter({ hasText: /成功 · 2\/2 健康成功/ }),
  ).toBeVisible({ timeout: 25000 });
  await page.getByRole("tab", { name: "历史", exact: true }).click();
  await expect(page.locator("tbody tr")).toHaveCount(4);
  await expect(page.locator("tbody tr").first()).toContainText(tag);
});

test("history explains success and failure evidence", async ({ page }) => {
  await page.goto("/#/deploy/0/history");
  await page.getByRole("button", { name: "详情", exact: true }).first().click();
  await expect(page.locator("#sheet .history-log")).toContainText("web-1 HEALTHCHECK healthy");
  await page.locator('#sheet [data-action="close"]').click();
  await page.getByRole("button", { name: "详情", exact: true }).nth(1).click();
  await expect(page.locator("#sheet .history-host").first()).toContainText("健康检查超过 120s 仍未就绪；具体根因未记录");
  await expect(page.locator("#sheet .history-log")).toContainText("web-1 健康检查超过 120s 仍未就绪");
});

test("many history hosts keep the saved log accessible", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 800 });
  await page.goto("/#/deploy/0/history");
  await page.evaluate(() => {
    const record = history[0];
    record.rows = Array.from({ length: 24 }, (_, index) => ({
      ...record.rows[0],
      id: index + 1,
      name: `web-${index + 1}`,
    }));
    record.targets = record.rows.map((host) => host.id);
    historyDetail(record.id);
  });
  const targets = page.locator("#sheet .history-targets");
  await expect(targets).not.toHaveAttribute("open", "");
  await expect(page.locator("#sheet .history-log")).toBeVisible();
  await targets.locator("summary").first().click();
  await expect(page.locator("#sheet .history-host")).toHaveCount(24);
  expect(await page.locator("#sheet .history-host-list").evaluate((list) => list.scrollHeight > list.clientHeight)).toBe(true);
  expect(await page.evaluate(() => document.querySelector("#sheet").scrollWidth <= document.querySelector("#sheet").clientWidth)).toBe(true);
});
test("unknown outcome cannot be retried or skipped before verification", async ({
  page,
}) => {
  await page.goto("/#/deploy");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-scenario"]').selectOption("unknown");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.getByRole("button", { name: "重新核实状态" })).toBeVisible({
    timeout: 12000,
  });
  await expect(page.getByRole("button", { name: "重试该主机" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "跳过此台继续" })).toHaveCount(
    0,
  );
  await expect(page.locator('[name="deploy-project"]')).toBeDisabled();
  await page.getByRole("tab", { name: "历史", exact: true }).click();
  await page.locator('[data-action="rollback:1"]').click();
  await expect(
    page.getByRole("button", { name: "确认回滚", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByText("该主机发布中或待核实，不能回滚").first(),
  ).toBeVisible();
  await page
    .locator("#dialog")
    .getByRole("button", { name: "取消", exact: true })
    .click();
  await page.getByRole("tab", { name: "执行", exact: true }).click();
  await page.getByRole("button", { name: "重新核实状态" }).click();
  await expect(page.getByRole("button", { name: "重试该主机" })).toBeVisible();
});
test("terminating a paused release removes retry and cancels pending hosts", async ({
  page,
}) => {
  await page.goto("/#/deploy");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-scenario"]').selectOption("failure");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.getByRole("button", { name: "重试该主机" })).toBeVisible({
    timeout: 12000,
  });
  await page.locator('[data-action="deploy-stop"]').first().click();
  await page.getByRole("button", { name: "终止发布", exact: true }).click();
  await expect(page.getByRole("button", { name: "重试该主机" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "跳过此台继续" })).toHaveCount(
    0,
  );
  await expect(page.locator("tbody .badge")).toHaveText(["失败", "已终止"]);
});
test("partial release history preserves individual results and configured image", async ({
  page,
}) => {
  await page.goto("/#/projects/1/build");
  await page.locator('[name="image"]').fill("custom-web");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await page.locator('nav a[href="#/deploy"]').click();
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-scenario"]').selectOption("failure");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.getByRole("button", { name: "跳过此台继续" })).toBeVisible({
    timeout: 12000,
  });
  await page.getByRole("button", { name: "查看原因" }).click();
  await expect(page.locator("#dialog")).toContainText("端口 8080 已被占用");
  await page.locator('#dialog [data-action="close"]').last().click();
  await page.getByRole("button", { name: "跳过此台继续" }).click();
  await expect(
    page.locator(".release-message").filter({ hasText: "部分完成 · 1/2 健康成功" }),
  ).toBeVisible({ timeout: 12000 });
  await page.getByRole("tab", { name: "历史", exact: true }).click();
  await page.getByRole("button", { name: "详情", exact: true }).first().click();
  await expect(page.locator("#sheet .history-summary")).toContainText("1/2 台通过健康检查");
  await expect(page.locator("#sheet .history-host").first()).toContainText("失败后跳过：端口 8080 已被占用");
  await expect(page.locator("#sheet .history-log")).toContainText("docker compose up -d 未完成");
  await expect(page.locator("#sheet .history-host").nth(1).locator(".badge")).toHaveText("成功");
  await expect(page.locator("#sheet .history-log")).toContainText("web-2：健康检查通过，服务已就绪");
  await page.locator("#sheet .history-host").nth(1).locator("summary").click();
  await expect(page.locator("#sheet .history-host").nth(1).locator(".history-tech")).toContainText(
    "custom-web:",
  );
  await expect(page.locator("#sheet .history-host").nth(1)).not.toContainText(
    "容器启动失败",
  );
});
test("menus, sheet, retention warning, and mobile navigation", async ({
  page,
}) => {
  await page.goto("/#/containers/1");
  await page.locator("summary").first().click();
  await expect(
    page.getByRole("button", { name: "重启", exact: true }),
  ).toBeVisible();
  await page.getByRole("button", { name: "日志", exact: true }).first().click();
  await expect(page.locator("#sheet")).toBeVisible();
  await page.locator('#sheet [data-action="close"]').click();
  await page.goto("/#/settings");
  await page.locator('[name="retention"]').selectOption("1");
  await expect(
    page.getByText("仅保留当前版本，该主机无法回滚到上一版本。"),
  ).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator('[data-action="menu"]').click();
  await page.locator('nav a[href="#/hosts"]').click();
  await expect(page.locator("h1")).toHaveText("主机");
  await expect(page.locator("#sidebar")).not.toBeVisible();
  await page.screenshot({
    path: "test-results/hosts-mobile.png",
    fullPage: true,
  });
});
test("host detail exposes container actions and image management", async ({ page }) => {
  await page.goto("/#/hosts/1/monitor");
  const running = page.locator("tbody tr").filter({ hasText: "myapp-web-1" });
  await expect(running.getByRole("button", { name: "日志" })).toBeVisible();
  await running.locator("summary").click();
  await running.getByRole("button", { name: "停止" }).click();
  await page.locator('#dialog [data-action="container-confirm:container-stop:1-1"]').click();
  await expect(running).toHaveCount(0);

  await page.getByRole("link", { name: "镜像管理" }).click();
  await expect(page).toHaveURL(/#\/containers\/1\/images$/);
  await expect(page.locator('button[data-action="image-delete:myapp:20261024-143200-amd64"]')).toBeDisabled();
  await page.locator('button[data-action="image-delete:worker:old"]').click();
  await page.locator('button[data-action="image-delete-confirm:worker:old"]').click();
  await expect(page.locator('button[data-action="image-delete:myapp:20261023-184000-amd64"]')).toBeDisabled();
});
