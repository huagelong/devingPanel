const { test, expect } = require("@playwright/test");

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
  await page.goto("/#/deploy");
  await freeze(page);
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await page.clock.runFor(12600);
  await expect(page.locator("tbody .badge")).toHaveText(["成功", "成功"]);
  await page.goto("/#/projects/1/targets");
  await page.locator('[name="target"][value="2"]').uncheck();
  await page.getByRole("button", { name: "保存", exact: true }).click();
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
test("form project tag rule is fixed to git-sha", async ({ page }) => {
  await page.goto("/#/projects/1/build");
  const tag = page.locator('[name="tag"]');
  await expect(tag).toHaveValue("{git-sha}");
  await expect(tag).toHaveAttribute("readonly", "");
  await page.locator('[name="image"]').fill("updated-image");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(tag).toHaveValue("{git-sha}");
  await page.goto("/#/deploy");
  await expect(page.locator("#release-tag")).toHaveText("a1b2c3d");
});
test("renaming preserves history and a new same-name project inherits none", async ({
  page,
}) => {
  await page.goto("/#/projects");
  await page.locator("summary").click();
  await page.locator('[data-action="project-edit:1"]').click();
  await page.locator('[name="name"]').fill("renamed-app");
  await page.locator('[data-action="project-save"]').click();
  await page.getByRole("tab", { name: "发布历史", exact: true }).click();
  await expect(page.locator("tbody tr")).toHaveCount(3);
  await page.locator('nav a[href="#/projects"]').click();
  await page.getByRole("button", { name: "新建项目", exact: true }).click();
  await page.locator('[name="name"]').fill("myapp");
  await page.locator('[name="path"]').fill("D:\\new-project");
  await page.locator('[data-action="project-save"]').click();
  await page.getByRole("tab", { name: "发布历史", exact: true }).click();
  await expect(page.getByText("暂无发布记录")).toBeVisible();
});
test("sample history belongs to the new project and opens its details", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "新建项目", exact: true }).click();
  await page.locator('[name="name"]').fill("1111");
  await page.locator('[name="path"]').fill("D:\\code\\1111");
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
