const { test, expect } = require("@playwright/test");
const sections = [
  ["retention", "发布保留策略", '[name="retention"]'],
  ["appearance", "外观", '[name="theme"]'],
  ["registry", "镜像仓库（可选）", '[name="registry"]'],
  ["backup", "备份与恢复", '[data-action="backup-export"]'],
  ["environment", "本地环境", '[data-action="docker-detect"]'],
  ["security", "安全", '[name="screen-new"]'],
];
test("each settings section is a separate addressable page with only its own content", async ({
  page,
}) => {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  for (const [key, title, selector] of sections) {
    await page.goto("/#/settings/" + key);
    await expect(page.locator(".settings-content > .card")).toHaveCount(1);
    await expect(page.locator(".settings-content h2")).toHaveText(title);
    await expect(
      page.locator(".settings-content").locator(selector).first(),
    ).toBeVisible();
    await expect(
      page.locator(`.settings-nav a[href="#/settings/${key}"]`),
    ).toHaveAttribute("aria-current", "page");
    await expect(page.locator("#topbar")).toContainText(
      key === "registry" ? "镜像仓库" : title,
    );
    await page.reload();
    await expect(page.locator(".settings-content h2")).toHaveText(title);
    await page.screenshot({
      path: `test-results/settings-${key}-desktop.png`,
      fullPage: true,
    });
  }
  expect(errors).toEqual([]);
});
test("navigation and browser back/forward retain non-secret settings", async ({
  page,
}) => {
  await page.goto("/#/settings");
  await expect(page).toHaveURL(/#\/settings\/retention$/);
  await page.locator('[name="retention"]').selectOption("5");
  await page.locator('.settings-nav a[href="#/settings/registry"]').click();
  await page.locator('[name="registry"]').fill("registry.example.com/project");
  await page.locator('[name="registry"]').blur();
  await expect(page.locator('[name="retention"]')).toHaveCount(0);
  await page.goBack();
  await expect(page.locator('[name="retention"]')).toHaveValue("5");
  await expect(page.locator('[name="registry"]')).toHaveCount(0);
  await page.goForward();
  await expect(page.locator('[name="registry"]')).toHaveValue(
    "registry.example.com/project",
  );
});
test("browser back dismisses a dialog belonging to the previous settings page", async ({
  page,
}) => {
  await page.goto("/#/settings/retention");
  await page.locator('.settings-nav a[href="#/settings/backup"]').click();
  await page.locator('[data-action="backup-password"]').click();
  await page.locator('[name="backup-password"]').fill("not-yet-saved-password");
  await page.goBack();
  await expect(page.locator(".settings-content h2")).toHaveText("发布保留策略");
  await expect(page.locator("#dialog")).not.toBeVisible();
  await page.goForward();
  await expect(page.locator(".settings-content h2")).toHaveText("备份与恢复");
  await expect(
    page.getByText("未设置，无法创建备份", { exact: true }),
  ).toBeVisible();
});
test("mobile category selector replaces the secondary sidebar without page overflow", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/#/settings/retention");
  await expect(page.locator(".settings-nav")).not.toBeVisible();
  for (const [key, title] of sections) {
    await page.locator('[name="settings-section"]').selectOption(key);
    await expect(page.locator(".settings-content h2")).toHaveText(title);
    expect(
      await page.evaluate(
        () => document.documentElement.scrollWidth <= innerWidth,
      ),
    ).toBe(true);
  }
  await page.locator('[name="settings-section"]').selectOption("backup");
  await page.screenshot({
    path: "test-results/settings-backup-mobile.png",
    fullPage: true,
  });
});
test("long environment paths truncate without hiding actions", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/#/settings/environment");
  const paths = page.locator(".environment-path");
  const longPath = "/very/long/" + "nested/directory/".repeat(20) + "devingpanel.sqlite";
  await paths.evaluateAll((elements, value) => {
    for (const element of elements) {
      element.textContent = value;
      element.title = value;
    }
  }, longPath);
  for (const path of await paths.all()) {
    await expect(path).toHaveAttribute("title", longPath);
    expect(await path.evaluate((element) => element.scrollWidth > element.clientWidth)).toBe(true);
  }
  await expect(page.locator('[data-action="native-folder"]')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});
test("host collection interval supports presets, custom seconds and configuration backup", async ({ page }) => {
  await page.goto("/#/settings/environment");
  const interval = page.locator('[name="host-collection-interval"]');
  await expect(interval).toHaveValue("60");
  await interval.selectOption("30");
  expect(await page.evaluate(() => state.hostCollectionIntervalSeconds)).toBe(30);
  await interval.selectOption("custom");
  const custom = page.locator('[name="host-collection-custom"]');
  await expect(custom).toHaveValue("30");
  await custom.fill("9");
  await custom.blur();
  expect(await page.evaluate(() => state.hostCollectionIntervalSeconds)).toBe(30);
  await custom.fill("75");
  await custom.blur();
  expect(await page.evaluate(() => state.hostCollectionIntervalSeconds)).toBe(75);
  expect(await page.evaluate(() => PanelBackup.create(hosts, projects, state).settings.hostCollectionIntervalSeconds)).toBe(75);
  await page.locator('.settings-nav a[href="#/settings/appearance"]').click();
  await page.goBack();
  await expect(page.locator('[name="host-collection-interval"]')).toHaveValue("custom");
  await expect(page.locator('[name="host-collection-custom"]')).toHaveValue("75");
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.reload();
  await expect(page.locator('[name="host-collection-interval"]')).toHaveValue("60");
});
test("unavailable alert pages and unknown categories return to the default section", async ({
  page,
}) => {
  await page.goto("/#/settings/unknown");
  await expect(page).toHaveURL(/#\/settings\/retention$/);
  await page.goto("/#/settings/alerts");
  await expect(page).toHaveURL(/#\/settings\/retention$/);
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-stage"]').selectOption("p2");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await page.locator('.settings-nav a[href="#/settings/alerts"]').click();
  await expect(page.locator(".settings-content h2")).toHaveText("告警");
  await page.locator('[name="threshold-cpu"]').fill("72");
  await page.locator('[name="threshold-cpu"]').blur();
  await page.getByRole("button", { name: "原型预览选项" }).click();
  await page.locator('[name="preview-stage"]').selectOption("p1");
  await page.getByRole("button", { name: "应用", exact: true }).click();
  await expect(page).toHaveURL(/#\/settings\/retention$/);
  await expect(
    page.locator('.settings-nav a[href="#/settings/alerts"]'),
  ).toHaveCount(0);
});

test("registry configuration can be removed and returns the release mode to SSH", async ({ page }) => {
  await page.goto("/#/settings/registry");
  await page.locator('[name="registry"]').fill("registry.example.com");
  await page.locator('[name="registry-user"]').fill("deploy");
  await page.getByRole("button", { name: "保存配置（演示）" }).click();
  await page.locator('[name="registry-enabled"]').check();
  await page.getByRole("button", { name: "移除配置" }).click();
  await expect(page.getByRole("dialog")).toContainText("SSH 直连");
  await page.getByRole("dialog").getByRole("button", { name: "移除配置" }).click();
  await expect(page.locator('[name="registry"]')).toHaveValue("");
  await expect(page.locator('[name="registry-user"]')).toHaveValue("");
  await expect(page.locator('[name="registry-enabled"]')).toBeDisabled();
  await page.goto("/#/deploy");
  await page.getByRole("button", { name: "开始发布", exact: true }).click();
  await expect(page.locator("#build-log")).toContainText("SSH save / SFTP / load");
});

test("retention preview uses release history and only marks eligible demo materials", async ({ page }) => {
  await page.goto("/#/settings/retention");
  await page.locator('[name="retention"]').selectOption("1");
  await page.evaluate(() => history.push({
    id: 99, time: "2026-10-20 09:00", projectId: 1, project: "myapp", tag: "old-release", result: "成功", tone: "success", duration: "1m", targets: [1],
    rows: [{ id: 1, name: "web-1", arch: "amd64", image: "myapp:old-release-amd64", status: "成功", tone: "success", phase: "完成", elapsed: "1m", materials: true }],
  }));
  await page.getByRole("button", { name: "立即清理托管的历史镜像" }).click();
  await expect(page.getByRole("dialog")).toContainText("old-release");
  await expect(page.getByRole("dialog")).not.toContainText("20261020-093000");
  await page.getByRole("dialog").getByRole("button", { name: "确认清理" }).click();
  expect(await page.evaluate(() => history.find((record) => record.id === 99).rows[0].materials)).toBe(false);
  await page.getByRole("button", { name: "立即清理托管的历史镜像" }).click();
  await expect(page.getByRole("dialog")).toContainText("当前没有超出保留数的已完成版本");
});
