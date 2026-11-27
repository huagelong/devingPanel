const { test, expect } = require("@playwright/test");
const { installProjectDirectory, connectProjectDirectory } = require("./project-directory.cjs");

test("dashboard attention uses each project's latest release and limits visible reminders", async ({ page }) => {
  await page.goto("/#/dashboard");
  await expect(page.locator(".attention-list")).toContainText("web-3");
  await expect(page.locator('.attention [data-action="history:2"]')).toHaveCount(0);
  await page.evaluate(() => {
    history.unshift({ ...history[1], id: 99, time: "2026-11-27 12:00:00" });
    render();
  });
  await page.locator('.attention [data-action="history:99"]').click();
  await expect(page.locator("#sheet .history-log")).toContainText("健康检查超过 120s");
  await page.locator('#sheet [data-action="close"]').click();
  await page.evaluate(() => {
    for (let i = 0; i < 12; i++) hosts.push({ ...hosts[2], id: i + 10, name: `offline-${i}` });
    render();
  });
  await expect(page.locator(".attention-item")).toHaveCount(4);
  await expect(page.locator(".attention-more")).toContainText("还有 10 项");
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

test("resource actions stay in view when desktop navigation reduces the table width", async ({ page }) => {
  await page.setViewportSize({ width: 1024, height: 900 });
  for (const route of ["hosts", "projects", "containers/1", "deploy/0/history"]) {
    await page.goto(`/#/${route}`);
    const actions = page.locator(".resource-table .action-col .actions").first();
    await expect(actions).toBeVisible();
    const box = await actions.boundingBox();
    expect(box.x).toBeGreaterThanOrEqual(224);
    expect(box.x + box.width).toBeLessThanOrEqual(1024);
    expect(await page.locator(".resource-table .table-wrap").first().evaluate((wrap) => wrap.scrollWidth <= wrap.clientWidth)).toBe(true);
  }
  await page.setViewportSize({ width: 1440, height: 900 });
  await expect(page.locator("thead")).toBeInViewport();
});

test("persistent project save controls stay usable below long drafts and notifications", async ({ page }) => {
  await installProjectDirectory(page);
  await page.goto("/#/projects/1/build");
  await connectProjectDirectory(page);
  await page.clock.install();
  await page.clock.pauseAt(new Date());
  await page.locator('[name="manifest-tag"]').fill("layout-{timestamp}");
  await expect(page.locator("#project-draft")).toBeVisible();
  await page.evaluate(() => toast("测试通知"));
  await page.locator('.project-save-bar [data-action="manifest-save"]').click();
  await expect.poll(() => page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("layout-{timestamp}");
  await page.getByRole("button", { name: "添加构建", exact: true }).click();
  await expect(page.locator("#project-build-count")).toHaveText("2");
  await page.setViewportSize({ width: 390, height: 844 });
  const lastInput = page.locator('[name="manifest-image-1"]');
  await lastInput.focus();
  await lastInput.scrollIntoViewIfNeeded();
  const inputBox = await lastInput.boundingBox();
  const footerBox = await page.locator(".project-save-bar").boundingBox();
  expect(inputBox.y + inputBox.height).toBeLessThanOrEqual(footerBox.y);
  await expect(page.locator('.project-save-bar [data-action="manifest-save"]')).toBeInViewport();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});


test("dashboard task clears on completion while the overview stays open", async ({ page }) => {
  await page.goto("/#/deploy");
  await page.clock.install();
  await page.clock.pauseAt(new Date());
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await page.locator('#sidebar a[href="#/dashboard"]').click();
  await expect(page.locator("#dashboard-task")).toContainText("进行中");
  await page.clock.runFor(12600);
  await expect(page.locator("#dashboard-task")).toHaveCount(0);
  await expect(page.locator(".activity-section tbody tr").first()).toContainText("a1b2c3d");
  await expect(page.locator(".activity-section tbody tr").first()).toContainText("成功");
});
