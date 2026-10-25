const { test, expect } = require("@playwright/test");
const sections = [
  ["retention", "发布保留策略", '[name="retention"]'],
  ["appearance", "外观", '[name="theme"]'],
  ["registry", "镜像仓库（可选）", '[name="registry"]'],
  ["backup", "备份与恢复", '[data-action="backup-export"]'],
  ["environment", "本地环境", '[data-action="docker-detect"]'],
  ["security", "安全", '[data-action="native-folder"]'],
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
