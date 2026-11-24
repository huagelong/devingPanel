const { test, expect } = require("@playwright/test");
const { installProjectDirectory, connectProjectDirectory } = require("./project-directory.cjs");

test("host dialog and history empty states follow page specifications", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机" }).click();
  await expect(page.locator("#dialog.host-dialog")).toHaveCSS("max-width", "512px");
  await page.locator('#dialog [data-action="close"]').first().click();

  await page.goto("/#/dashboard");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-view"]').selectOption("empty");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.getByText("暂无发布记录")).toBeVisible();
  await expect(page.locator("main .empty")).toHaveCount(1);

  await page.goto("/#/deploy/0/history");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-view"]').selectOption("empty");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.locator("main .empty")).toContainText("暂无发布记录");
  await expect(page.locator("main .empty").getByRole("button", { name: "去发布" })).toBeVisible();
});

test("permission preview shows a return path without claiming a real permission check", async ({ page }) => {
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-view"]').selectOption("denied");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page.locator("main .empty")).toContainText("无权访问此页面");
  await page.getByRole("button", { name: "返回总览" }).click();
  await expect(page).toHaveURL(/#\/dashboard$/);
  await expect(page.locator(".page-head h1")).toHaveText("总览");

  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-view"]').selectOption("denied");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.getByRole("button", { name: "返回总览" }).click();
  await expect(page.locator("main .hosts-grid")).toBeVisible();
});

test("tabs retain focus after keyboard navigation and rerender", async ({ page }) => {
  await page.goto("/#/deploy");
  await page.getByRole("tab", { name: "执行" }).focus();
  await page.keyboard.press("ArrowRight");
  await expect(page.getByRole("tab", { name: "历史" })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tab", { name: "历史" })).toBeFocused();
  await expect(page.getByRole("tab", { name: "执行" })).toHaveAttribute("tabindex", "-1");
  await page.keyboard.press("Home");
  await expect(page.getByRole("tab", { name: "执行" })).toBeFocused();

  await page.goto("/#/settings/backup");
  await page.getByRole("tab", { name: "本地备份" }).focus();
  await page.keyboard.press("End");
  await expect(page.getByRole("tab", { name: "S3 备份" })).toBeFocused();
  await expect(page.getByRole("tab", { name: "S3 备份" })).toHaveAttribute("aria-selected", "true");
});

test("core pages fit measured widths and forced colors keep tab selection visible", async ({ page }) => {
  for (const width of [1440, 1024, 768, 700, 390]) {
    await page.setViewportSize({ width, height: 900 });
    for (const route of ["dashboard", "hosts", "projects/1/build", "deploy", "settings/appearance"]) {
      await page.goto("/#/" + route);
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(width);
    }
  }
  await page.emulateMedia({ forcedColors: "active" });
  await page.goto("/#/deploy");
  const selectedBorder = await page.getByRole("tab", { name: "执行" }).evaluate((tab) => getComputedStyle(tab).borderBottomColor);
  const otherBorder = await page.getByRole("tab", { name: "历史" }).evaluate((tab) => getComputedStyle(tab).borderBottomColor);
  expect(selectedBorder).not.toBe(otherBorder);
});

async function freeze(page) {
  await page.clock.install({ time: new Date("2026-10-24T04:00:00Z") });
  await page.clock.pauseAt(new Date("2026-10-24T04:00:01Z"));
}
test("verification during stop waits for other active batch members", async ({
  page,
}) => {
  await page.goto("/#/deploy");
  await freeze(page);
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-scenario"]').selectOption("unknown");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.locator('[name="parallel"]').selectOption("2");
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await page.clock.runFor(6300);
  await expect(page.locator("tbody .badge")).toHaveText([
    "待核实",
    "健康检查中",
  ]);
  await page.locator('[data-action="deploy-stop"]').first().click();
  await page.getByRole("button", { name: "终止发布", exact: true }).click();
  await page.getByRole("button", { name: "重新核实状态", exact: true }).click();
  await expect(page.locator('[name="deploy-project"]')).toBeDisabled();
  await expect(page.locator("tbody .badge")).toHaveText(["失败", "健康检查中"]);
  await page.clock.runFor(900);
  await expect(page.locator("tbody .badge")).toHaveText(["失败", "成功"]);
  await expect(page.locator('[name="deploy-project"]')).toBeEnabled();
});
test("next release target count matches execution after binding changes", async ({
  page,
}) => {
  await installProjectDirectory(page);
  await page.goto("/#/deploy");
  await freeze(page);
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await page.clock.runFor(12600);
  await expect(page.locator("tbody .badge")).toHaveText(["成功", "成功"]);
  await page.goto("/#/projects/1/targets");
  await connectProjectDirectory(page);
  await expect(page.locator('[name="manifest-host-2"]')).toHaveValue("web-3");
  await page.locator('[data-action="manifest-target-remove:2"]').click();
  await page.locator('[data-action="manifest-target-remove:1"]').click();
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await page.getByRole("button", { name: "确认保存" }).click();
  await page.locator('nav a[href="#/deploy"]').click();
  await expect(page.getByText("已选 1 台", { exact: true })).toBeVisible();
  await expect(page.locator('[name="parallel"] option')).toHaveCount(1);
  await expect(page.locator(".release-message")).toContainText("成功 · 2/2 健康成功");
  await expect(page.locator("#release-tag")).toHaveText("a1b2c3d");
  await expect(page.locator('[name="release-tag"]')).toHaveCount(0);
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.getByText("发布标识已存在，不允许覆盖历史版本。")).toBeVisible();
  await expect(page.locator("tbody tr")).toHaveCount(2);
});
test("project tag rule saves to YAML after directory authorization", async ({ page }) => {
  await installProjectDirectory(page);
  await page.goto("/#/projects/1/build");
  await connectProjectDirectory(page);
  const tag = page.locator('[name="manifest-tag"]');
  await expect(tag).toHaveValue("{git-sha}");
  await tag.fill("release-{timestamp}");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("release-{timestamp}");
});
test("renaming preserves history and a new same-name project inherits none", async ({
  page,
}) => {
  await installProjectDirectory(page);
  await page.goto("/#/projects");
  await page.locator("summary").click();
  await expect(page.getByRole("link", { name: "详情", exact: true })).toHaveCount(0);
  await page.getByRole("link", { name: "编辑" }).click();
  await expect(page).toHaveURL(/#\/projects\/1\/basic$/);
  await connectProjectDirectory(page);
  await page.locator('[name="manifest-name"]').fill("renamed-app");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator("h1")).toHaveText("renamed-app");
  await expect(page.locator('[name="path"]')).toHaveAttribute("readonly", "");
  await page.getByRole("tab", { name: "发布历史", exact: true }).click();
  await expect(page.locator("tbody tr")).toHaveCount(3);
  await page.locator('nav a[href="#/projects"]').click();
  await page.getByRole("button", { name: "新建项目", exact: true }).click();
  await page.locator('[name="name"]').fill("myapp");
  await page.locator('[data-action="project-save"]').click();
  await page.getByRole("tab", { name: "发布历史", exact: true }).click();
  await expect(page.getByText("暂无发布记录")).toBeVisible();
});
test("sample history belongs to the new project and opens its details", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "新建项目", exact: true }).click();
  await page.locator('[name="name"]').fill("1111");
  await page.locator('[data-action="project-save"]').click();
  await page.getByRole("tab", { name: "发布历史", exact: true }).click();
  await page.getByRole("button", { name: "填充演示记录" }).click();
  await expect(page.locator("main tbody tr")).toHaveCount(3);
  await expect(page.getByText("共 3 条")).toBeVisible();
  await page.getByRole("button", { name: "详情" }).first().click();
  await expect(page.locator("#sheet")).toContainText("1111");
  await page.goto("/#/projects/1/history");
  await expect(page.locator("main tbody tr")).toHaveCount(3);
});
test("project history actually paginates, including exact page boundaries", async ({
  page,
}) => {
  await page.goto("/#/projects/1/history");
  await page.evaluate(() => {
    const original = structuredClone(history[0]);
    history.splice(0);
    for (let i = 0; i < 41; i++)
      history.push({ ...original, id: 100 + i, tag: "release-" + i });
    render();
  });
  await expect(page.locator("tbody tr")).toHaveCount(20);
  await expect(page.getByRole("button", { name: "上一页" })).toBeDisabled();
  await page.getByRole("button", { name: "下一页" }).click();
  await expect(page.locator("tbody tr")).toHaveCount(20);
  await expect(page.locator("tbody tr").first()).toContainText("release-20");
  await page.getByRole("button", { name: "下一页" }).click();
  await expect(page.locator("tbody tr")).toHaveCount(1);
  await expect(page.getByRole("button", { name: "下一页" })).toBeDisabled();
  await page.getByRole("button", { name: "上一页" }).click();
  await expect(page.locator("tbody tr")).toHaveCount(20);
});
test("custom retention one immediately warns and alert settings survive navigation", async ({
  page,
}) => {
  await page.goto("/#/settings");
  await page.locator('[name="retention"]').selectOption("custom");
  await page.locator('[name="retention-custom"]').fill("1");
  await page.locator('[name="retention-custom"]').blur();
  await expect(
    page.getByText("仅保留当前版本，该主机无法回滚到上一版本。"),
  ).toBeVisible();
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-stage"]').selectOption("p2");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.locator('.settings-nav a[href="#/settings/alerts"]').click();
  await page.locator('[name="threshold-cpu"]').fill("75");
  await page.locator('[name="threshold-cpu"]').blur();
  await page.locator('[name="threshold-memory"]').fill("80");
  await page.locator('[name="threshold-memory"]').blur();
  await page.locator('[name="alert-duration"]').selectOption("10 分钟");
  await page.locator('nav a[href="#/dashboard"]').click();
  await page.locator('nav a[href="#/settings"]').click();
  await page.locator('.settings-nav a[href="#/settings/alerts"]').click();
  await expect(page.locator('[name="threshold-cpu"]')).toHaveValue("75");
  await expect(page.locator('[name="threshold-memory"]')).toHaveValue("80");
  await expect(page.locator('[name="alert-duration"]')).toHaveValue("10 分钟");
});
