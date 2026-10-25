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
  await expect(
    page.getByRole("heading", { name: "上次发布结果" }),
  ).toBeVisible();
  await page.locator('[name="release-tag"]').fill("second-release");
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.locator("tbody tr")).toHaveCount(1);
  await expect(page.locator("tbody tr")).toContainText("web-1");
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
