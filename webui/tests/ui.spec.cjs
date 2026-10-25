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
    path.join(__dirname, "../../docs/design/00-overview.md"),
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
    await page.locator('[data-action="theme"]').click();
    await page.locator(`[data-action="set-theme:${theme}"]`).click();
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
  await page.locator('[name="registry"]').fill("registry.example.com/team");
  await page.locator('[name="registry"]').blur();
  await page.locator('nav a[href="#/deploy"]').click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(
    page.getByText("linux/amd64 push · 100%", { exact: true }),
  ).toBeVisible({ timeout: 10000 });
  await expect(page.locator("tbody")).toContainText("拉取镜像");
  await page.reload();
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-scenario"]').selectOption("unverified");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(
    page.getByText("linux/amd64 · 构建中", { exact: true }),
  ).toBeVisible();
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
    .getByRole("button", { name: "保存", exact: true })
    .click();
  const host = page.getByRole("row").filter({ hasText: "new-untrusted" });
  await expect(host).toContainText("离线");
  await host.getByRole("button", { name: "测试连接", exact: true }).click();
  await expect(page.getByText("首次连接：确认主机指纹")).toBeVisible();
  await page.getByRole("button", { name: "确认演示指纹" }).click();
  await expect(host).toContainText("在线");
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
test("theme tokens and shell match specification", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator("#sidebar")).toHaveCSS("width", "224px");
  await expect(page.locator("#topbar")).toHaveCSS("height", "56px");
  await page.locator('[data-action="theme"]').click();
  await page.locator('[data-action="set-theme:dark"]').click();
  await expect(page.locator("html")).toHaveClass("dark");
  await page.reload();
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
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.getByText("test-host", { exact: true })).toBeVisible();
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "新建项目", exact: true }).click();
  await page.locator('[name="name"]').fill("demo-app");
  await page.locator('[name="path"]').fill("D:\\demo-app");
  await page.getByRole("button", { name: "创建项目", exact: true }).click();
  await expect(page.locator("h1")).toHaveText("demo-app");
  await expect(page.locator('[data-action^="project-deploy:"]')).toBeDisabled();
});
test("deployment succeeds and records history", async ({ page }) => {
  await page.goto("/#/deploy");
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.locator('[name="deploy-project"]')).toBeDisabled();
  await expect(
    page.locator(".notice").filter({ hasText: /成功 · 2\/2 健康成功/ }),
  ).toBeVisible({ timeout: 25000 });
  await page.getByRole("tab", { name: "历史", exact: true }).click();
  await expect(page.locator("tbody tr")).toHaveCount(4);
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
  await page.getByRole("button", { name: "跳过此台继续" }).click();
  await expect(
    page.locator(".notice").filter({ hasText: "部分完成 · 1/2 健康成功" }),
  ).toBeVisible({ timeout: 12000 });
  await page.getByRole("tab", { name: "历史", exact: true }).click();
  await page.getByRole("button", { name: "详情", exact: true }).first().click();
  await expect(page.locator("#sheet .card").first()).toContainText("已跳过");
  await expect(page.locator("#sheet .card").nth(1)).toContainText("成功");
  await expect(page.locator("#sheet .card").nth(1)).toContainText(
    "custom-web:",
  );
  await expect(page.locator("#sheet .card").nth(1)).not.toContainText(
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
