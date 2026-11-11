const { test, expect } = require("@playwright/test");
const fs = require("node:fs/promises");
async function english(page) {
  await page.goto("/#/settings/appearance");
  await page.locator('[name="language"]').selectOption("en");
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
}
async function noChineseUI(page) {
  const remaining = await page.evaluate(() => {
    const out = [];
    const walker = document.createTreeWalker(
      document.body,
      NodeFilter.SHOW_TEXT,
    );
    while (walker.nextNode()) {
      const node = walker.currentNode,
        el = node.parentElement;
      if (
        el.closest('script,style,option,pre[translate="no"]') ||
        !el.getClientRects().length
      )
        continue;
      if (/[\u3400-\u9fff]/.test(node.textContent))
        out.push(node.textContent.trim());
    }
    document
      .querySelectorAll("[aria-label],[title],[placeholder]")
      .forEach((el) => {
        if (!el.getClientRects().length) return;
        for (const key of ["aria-label", "title", "placeholder"])
          if (/[\u3400-\u9fff]/.test(el.getAttribute(key) || ""))
            out.push(key + ": " + el.getAttribute(key));
      });
    document.querySelectorAll('select:not([name="language"])').forEach((el) => {
      if (el.getClientRects().length)
        for (const option of el.options)
          if (/[\u3400-\u9fff]/.test(option.text))
            out.push("option: " + option.text);
    });
    return out;
  });
  expect(remaining).toEqual([]);
}
test.beforeEach(async ({ page }) => {
  page.errors = [];
  page.on("pageerror", (e) => page.errors.push(e.message));
});
test("Docker installation guidance remains bilingual in the prototype", async ({ page }) => {
  await english(page);
  await page.getByRole("button", { name: "Prototype options" }).click();
  await page.locator('[name="preview-host-docker-missing"]').check();
  await page.getByRole("button", { name: "Apply", exact: true }).click();
  await page.goto("/#/hosts/1/info");
  await expect(page.getByText("SSH connected · Docker not installed")).toBeVisible();
  await noChineseUI(page);
  await page.getByRole("button", { name: "Install Docker", exact: true }).click();
  await noChineseUI(page);
});
test("registry mirror confirmation remains bilingual in the prototype", async ({ page }) => {
  await english(page);
  await page.getByRole("button", { name: "Prototype options" }).click();
  await page.locator('[name="preview-mirrors"]').check();
  await page.getByRole("button", { name: "Apply", exact: true }).click();
  await page.goto("/#/hosts/1/info");
  await page.getByRole("button", { name: "Configure mirrors" }).click();
  await noChineseUI(page);
});
test("developer YAML project preview remains bilingual", async ({ page }) => {
  await english(page);
  await page.getByRole("button", { name: "Prototype options" }).click();
  await page.locator('[name="preview-yaml-project"]').check();
  await page.locator('[name="preview-yaml-error"]').check();
  await page.getByRole("button", { name: "Apply", exact: true }).click();
  await page.goto("/#/projects/1/build");
  await noChineseUI(page);
});
test.afterEach(async ({ page }) => expect(page.errors).toEqual([]));
test("language selection persists, updates document metadata and can return to Chinese", async ({
  page,
}) => {
  await page.goto("/#/settings/appearance");
  await expect(page.locator('[name="language"]')).toHaveValue("zh-CN");
  const before = await page.evaluate(() =>
    JSON.stringify([hosts, projects, history]),
  );
  await page.locator('[name="language"]').selectOption("en");
  await expect(page.locator("h1")).toHaveText("Settings");
  await expect(page).toHaveTitle("Appearance · Settings · devingPanel");
  await expect(page.locator("#sidebar")).toHaveAttribute(
    "aria-label",
    "Main navigation",
  );
  expect(
    await page.evaluate(() => JSON.stringify([hosts, projects, history])),
  ).toBe(before);
  await page.reload();
  await expect(page.locator("h1")).toHaveText("Settings");
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
  await page.locator('[name="language"]').selectOption("zh-CN");
  await expect(page.locator("h1")).toHaveText("设置");
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
});
test("all pages and settings categories have English UI and fit desktop and mobile", async ({
  page,
}) => {
  await english(page);
  await page.locator('[data-action="preview"]').click();
  await page.locator('[name="preview-stage"]').selectOption("p2");
  await page.locator('[data-action="preview-apply"]').click();
  const routes = [
    "dashboard",
    "hosts",
    "hosts/1/info",
    "hosts/1/monitor",
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
    ...[
      "retention",
      "appearance",
      "registry",
      "backup",
      "alerts",
      "environment",
      "security",
    ].map((s) => "settings/" + s),
  ];
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    for (const route of routes) {
      await page.goto("/#/" + route);
      await expect(page.locator("h1")).toBeVisible();
      await expect(page).toHaveURL(new RegExp("#/" + route + "$"));
      await page.waitForFunction((route) => {
        const [section, id, tab = ""] = route.split("/");
        return (
          state.route === section &&
          (section === "settings"
            ? state.settingsSection === id
            : state.id === (Number(id) || 0) && state.tab === tab)
        );
      }, route);
      await noChineseUI(page);
      expect(
        await page.evaluate(
          () => document.documentElement.scrollWidth <= innerWidth,
        ),
      ).toBe(true);
    }
    await page.goto("/#/settings/backup");
    await page.screenshot({
      path: `test-results/i18n-backup-${width}.png`,
      fullPage: true,
    });
  }
});
test("English dialogs, accessibility text, fingerprints and errors are localized", async ({
  page,
}) => {
  await english(page);
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "Add host", exact: true }).click();
  await noChineseUI(page);
  await page.locator('[name="name"]').fill("example");
  await page.locator('[name="address"]').fill("192.0.2.5");
  await page.locator('[data-action="host-form-test"]').click();
  await noChineseUI(page);
  await page.locator('[data-action="fingerprint-confirm"]').click();
  await noChineseUI(page);
  await page
    .locator("#dialog")
    .getByRole("button", { name: "Close", exact: true })
    .click();
  await page.goto("/#/projects");
  await page.locator('[data-action="project-new"]').click();
  await noChineseUI(page);
  await page
    .locator("#dialog")
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await page.goto("/#/containers/1");
  await page.locator('[data-action="prune"]').click();
  await noChineseUI(page);
  await page
    .locator("#dialog")
    .getByRole("button", { name: "Cancel", exact: true })
    .click();
  await page.goto("/#/settings/backup");
  await page.locator('[data-action="backup-password"]').click();
  await noChineseUI(page);
  await page.locator('[name="backup-password"]').fill("password-first");
  await page
    .locator('[name="backup-password-confirm"]')
    .fill("password-second");
  await page.locator('[data-action="backup-password-save"]').click();
  await expect(page.locator("#backup-password-error")).toHaveText(
    "The passwords do not match.",
  );
});
test("user names, paths, group labels and container names are never translated", async ({
  page,
}) => {
  await english(page);
  await page.evaluate(() => {
    hosts[0].name = "设置";
    hosts[0].group = "发布";
    projects[0].name = "成功";
    projects[0].path = "D:\\项目\\设置";
    containers[0].name = "停止";
  });
  await page.goto("/#/hosts");
  await expect(page.locator("tbody")).toContainText("设置");
  await expect(page.locator("tbody")).toContainText("发布");
  await page.getByRole("link", { name: "设置", exact: true }).click();
  await expect(page.locator("h1")).toHaveText("设置");
  await page.goto("/#/projects");
  await expect(page.locator("tbody")).toContainText("D:\\项目\\设置");
  await page.getByRole("link", { name: "成功", exact: true }).click();
  await expect(page.locator("h1")).toHaveText("成功");
  await page.goto("/#/deploy");
  await expect(page.locator('[name="deploy-project"] option')).toHaveText([
    "成功",
  ]);
  await page.goto("/#/containers/1");
  await expect(page.locator('[data-action="logs:1-1"]').first()).toHaveText(
    "停止",
  );
  await page.locator('[data-action="logs:1-1"]').first().click();
  await expect(page.locator("#sheet h2")).toHaveText("停止");
});
test("English deployment uses unchanged canonical state and can switch language mid-task", async ({
  page,
}) => {
  await english(page);
  await page.goto("/#/deploy");
  await page.clock.install({ time: new Date("2026-10-25T04:00:00Z") });
  await page.clock.pauseAt(new Date("2026-10-25T04:00:01Z"));
  await page
    .getByRole("button", { name: "Start deployment", exact: true })
    .click();
  await page.clock.runFor(2700);
  await expect(page.locator("tbody .badge").first()).toHaveText("Transferring");
  expect(await page.evaluate(() => state.deploy.rows[0].status)).toBe("传输中");
  await page.goto("/#/settings/appearance");
  await page.locator('[name="language"]').selectOption("zh-CN");
  await page.goto("/#/deploy");
  await expect(page.locator("tbody .badge").first()).toHaveText("传输中");
  await page.goto("/#/settings/appearance");
  await page.locator('[name="language"]').selectOption("en");
  await page.goto("/#/deploy");
  await page.clock.runFor(9900);
  await expect(page.locator("tbody .badge")).toHaveText([
    "Succeeded",
    "Succeeded",
  ]);
  await noChineseUI(page);
});
test("English backup errors and encrypted restore work without changing language or data keys", async ({
  page,
}) => {
  await english(page);
  await page.goto("/#/settings/backup");
  await page.locator('[data-action="backup-password"]').click();
  await page
    .locator('[name="backup-password"]')
    .fill("english-backup-password");
  await page
    .locator('[name="backup-password-confirm"]')
    .fill("english-backup-password");
  await page.locator('[data-action="backup-password-save"]').click();
  await page.locator('[data-action="backup-export"]').click();
  await noChineseUI(page);
  const downloaded = page.waitForEvent("download");
  await page.locator('[data-action="backup-export-confirm"]').click();
  const envelope = JSON.parse(
    await fs.readFile(await (await downloaded).path(), "utf8"),
  );
  await page
    .locator("#backup-file")
    .setInputFiles({
      name: "backup.json",
      mimeType: "application/json",
      buffer: Buffer.from(JSON.stringify(envelope)),
    });
  await noChineseUI(page);
  await page.locator('[name="backup-decrypt-password"]').fill("wrong-password");
  await page.locator('[data-action="backup-decrypt"]').click();
  await expect(page.locator("#backup-decrypt-error")).toHaveText(
    "Incorrect password or damaged backup. No data was restored.",
  );
  await page
    .locator('[name="backup-decrypt-password"]')
    .fill("english-backup-password");
  await page.locator('[data-action="backup-decrypt"]').click();
  await expect(
    page.getByRole("heading", { name: "Import preview" }),
  ).toBeVisible();
  await noChineseUI(page);
  await page.locator("#backup-overwrite").check();
  await page.locator('[data-action="backup-import-confirm"]').click();
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
  await noChineseUI(page);
  expect(await page.evaluate(() => state.alerts.duration)).toBe("5 分钟");
});
test("unsupported persisted language falls back to Chinese", async ({
  page,
}) => {
  await page.addInitScript(() =>
    localStorage.setItem("devingpanel-language", "invalid-language"),
  );
  await page.goto("/#/settings/appearance");
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page.locator('[name="language"]')).toHaveValue("zh-CN");
});
test("new project placeholders and invalid Compose states are localized", async ({
  page,
}) => {
  await english(page);
  await page.goto("/#/projects");
  await page.locator('[data-action="project-new"]').click();
  await page.locator('[name="name"]').fill("example");
  await page.locator('[name="path"]').fill("D:\\example");
  await page.locator('[data-action="project-save"]').click();
  await expect(page.locator('[name="service"] option')).toHaveText([
    "Select a service",
  ]);
  await page.getByRole("tab", { name: "Compose", exact: true }).click();
  await expect(
    page.getByText("No Compose files configured.", { exact: true }),
  ).toBeVisible();
  await noChineseUI(page);
  await page.goto("/#/projects/1/compose");
  await page.evaluate(() => {
    projects[0].files = ["sub/prod.yml"];
    render();
  });
  await expect(
    page.getByText("The first Compose file must be in the project root.", {
      exact: true,
    }),
  ).toBeVisible();
});
test("switching language does not rewrite raw container logs", async ({
  page,
}) => {
  await english(page);
  await page.goto("/#/containers/1");
  await page.clock.install({ time: new Date("2026-10-25T04:00:00Z") });
  await page.clock.pauseAt(new Date("2026-10-25T04:00:01Z"));
  await page.locator('[data-action="logs:1-1"]').first().click();
  const englishLog = await page.locator("#container-log").textContent();
  await expect(page.locator("#sheet")).toContainText("Follow");
  await page.locator('#sheet [data-action="close"]').click();
  await page.goto("/#/settings/appearance");
  await page.locator('[name="language"]').selectOption("zh-CN");
  await page.goto("/#/containers/1");
  await page.locator('[data-action="logs:1-1"]').first().click();
  expect(await page.locator("#container-log").textContent()).toBe(englishLog);
  await expect(page.locator("#sheet")).toContainText("跟随");
});
