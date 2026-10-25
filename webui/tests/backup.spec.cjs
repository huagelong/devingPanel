const { test, expect } = require("@playwright/test");
const fs = require("node:fs/promises");
const PASSWORD = "test backup password 2026!";
async function setPassword(page) {
  await page.locator('[data-action="backup-password"]').click();
  await page.locator('[name="backup-password"]').fill(PASSWORD);
  await page.locator('[name="backup-password-confirm"]').fill(PASSWORD);
  await page.locator('[data-action="backup-password-save"]').click();
}
async function unlock(page) {
  await page.locator('[name="backup-decrypt-password"]').fill(PASSWORD);
  await page.locator('[data-action="backup-decrypt"]').click();
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
}

test.beforeEach(async ({ page }) => {
  page.scriptErrors = [];
  page.on("pageerror", (e) => page.scriptErrors.push(e.message));
  await page.goto("/#/settings/backup");
  await setPassword(page);
});
test.afterEach(async ({ page }) => {
  expect(page.scriptErrors).toEqual([]);
});
const fixture = (page) =>
  page.evaluate(() => PanelBackup.create(hosts, projects, state));
async function upload(page, data, decrypt = true) {
  const inner =
    typeof data === "object" && data.format === "devingpanel-config";
  if (inner)
    data = await page.evaluate(
      ({ data, password }) => PanelBackup.encrypt(data, password),
      { data, password: PASSWORD },
    );
  await page.locator("#backup-file").setInputFiles({
    name: "backup.json",
    mimeType: "application/json",
    buffer: Buffer.from(typeof data === "string" ? data : JSON.stringify(data)),
  });
  if (inner && decrypt) await unlock(page);
}
async function confirmImport(page) {
  await page.locator("#backup-overwrite").check();
  await page.getByRole("button", { name: "确认导入", exact: true }).click();
}
async function configureS3(page) {
  await page.getByRole("tab", { name: "S3 备份", exact: true }).click();
  await page.locator('[name="s3-endpoint"]').fill("https://s3.example.com");
  await page.locator('[name="s3-region"]').fill("us-east-1");
  await page.locator('[name="s3-bucket"]').fill("demo-backups");
  await page.locator('[name="s3-bucket"]').blur();
  await page
    .getByRole("button", { name: "测试连接（演示）", exact: true })
    .click();
}
test("local export downloads a credential-free, versioned JSON file", async ({
  page,
}) => {
  await page.evaluate(() => {
    hosts[0].password = "NEVER_EXPORT_HOST_PASSWORD";
    projects[0].args = [["TOKEN", "NEVER_EXPORT_BUILD_VALUE"]];
    projects[0].env = [["SECRET", "NEVER_EXPORT_ENV_VALUE"]];
    state.s3.secretKey = "NEVER_EXPORT_S3_KEY";
    state.registryToken = "NEVER_EXPORT_REGISTRY_TOKEN";
  });
  await page.goto("/#/settings/registry");
  await page.locator('[name="registry-token"]').fill("NEVER_EXPORT_FORM_TOKEN");
  await page.goto("/#/settings/backup");
  await page.getByRole("button", { name: "导出配置", exact: true }).click();
  const downloaded = page.waitForEvent("download");
  await page.getByRole("button", { name: "确认导出", exact: true }).click();
  const file = await downloaded;
  const text = await fs.readFile(await file.path(), "utf8");
  expect(file.suggestedFilename()).toMatch(/^devingpanel-encrypted-.*\.json$/);
  expect(text).not.toContain("NEVER_EXPORT");
  const envelope = JSON.parse(text);
  expect(envelope.format).toBe("devingpanel-encrypted-backup");
  expect(text).not.toContain("203.0.113.10");
  expect(text).not.toContain(PASSWORD);
  const data = await page.evaluate(
    ({ envelope, password }) => PanelBackup.decrypt(envelope, password),
    { envelope, password: PASSWORD },
  );
  expect(JSON.stringify(data)).not.toContain("NEVER_EXPORT");
  expect(data.format).toBe("devingpanel-config");
  expect(data.version).toBe(1);
  expect(data.hosts).toHaveLength(3);
  expect(data.projects[0].buildArgKeys).toEqual(["TOKEN"]);
  expect(data.hosts[0]).not.toHaveProperty("trustedAddress");
  expect(data).not.toHaveProperty("history");
});
test("import previews, requires consent, applies config and resets trust and readiness", async ({
  page,
}) => {
  const data = await fixture(page);
  data.hosts[0].name = "restored-host";
  data.projects[0].name = "restored-project";
  data.settings.keep = 1;
  data.settings.alerts.cpu = 75;
  await upload(page, data);
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
  await expect(
    page.getByRole("button", { name: "确认导入", exact: true }),
  ).toBeDisabled();
  expect(await page.evaluate(() => hosts[0].name)).toBe("web-1");
  await confirmImport(page);
  await page.locator('.settings-nav a[href="#/settings/retention"]').click();
  await expect(
    page.getByText("仅保留当前版本，该主机无法回滚到上一版本。"),
  ).toBeVisible();
  const restored = await page.evaluate(() => ({
    hosts,
    projects,
    historyLength: history.length,
    containerCount: containers.length,
  }));
  expect(restored.hosts[0].name).toBe("restored-host");
  expect(restored.hosts[0].online).toBe(false);
  expect(restored.hosts[0].trustedAddress).toBe("");
  expect(restored.projects[0].ready).toBe(false);
  expect(restored.projects[0].args).toEqual([["NODE_ENV", ""]]);
  expect(restored.historyLength).toBe(0);
  expect(restored.containerCount).toBe(0);
  await page.locator('nav a[href="#/projects"]').click();
  await page
    .getByRole("link", { name: "restored-project", exact: true })
    .click();
  await expect(page.getByText(/该项目来自配置备份/)).toBeVisible();
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.getByText(/请补齐导入后清空的构建参数值/)).toBeVisible();
  await page.locator('[name="value"]').fill("production");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator('[data-action="project-deploy:1"]')).toBeEnabled();
});
test("cancelled import is non-destructive and repeat selection still opens preview", async ({
  page,
}) => {
  const data = await fixture(page);
  data.hosts[0].name = "cancelled";
  await upload(page, data);
  await page.getByRole("button", { name: "取消", exact: true }).click();
  expect(await page.evaluate(() => hosts[0].name)).toBe("web-1");
  await upload(page, data);
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
});
test("a delayed file read cannot replace a later dialog or tab", async ({
  page,
}) => {
  for (const next of ["project", "export", "tab"]) {
    await page.goto("/#/settings/backup");
    await page.reload();
    await setPassword(page);
    const data = await fixture(page);
    await page.evaluate(() => {
      const original = File.prototype.text;
      File.prototype.text = function () {
        return new Promise((resolve) => {
          window.finishBackupRead = async () =>
            resolve(await original.call(this));
        });
      };
    });
    await upload(page, data, false);
    await expect
      .poll(() => page.evaluate(() => typeof window.finishBackupRead))
      .toBe("function");
    if (next === "project") {
      await page.locator('nav a[href="#/projects"]').click();
      await page.getByRole("button", { name: "新建项目", exact: true }).click();
      await page.locator('[name="name"]').fill("unsaved-project");
    } else if (next === "export") {
      await page.getByRole("button", { name: "导出配置", exact: true }).click();
    } else {
      await page.getByRole("tab", { name: "S3 备份", exact: true }).click();
    }
    await page.evaluate(() => window.finishBackupRead());
    if (next === "project") {
      await expect(
        page.getByRole("heading", { name: "新建项目", exact: true }),
      ).toBeVisible();
      await expect(page.locator('[name="name"]')).toHaveValue(
        "unsaved-project",
      );
    } else if (next === "export") {
      await expect(
        page.getByRole("heading", { name: "导出本地配置", exact: true }),
      ).toBeVisible();
    } else {
      await expect(page.locator("#dialog")).not.toBeVisible();
      await expect(page.locator('[name="s3-endpoint"]')).toBeVisible();
    }
  }
});
test("invalid files, unknown versions, duplicate IDs and secret fields fail atomically", async ({
  page,
}) => {
  const original = await fixture(page);
  const cases = [
    "{invalid",
    { ...original, version: 99 },
    { ...original, hosts: [original.hosts[0], original.hosts[0]] },
    { ...original, credential: "should-not-be-imported" },
    { ...original, projects: [{ ...original.projects[0], hostIds: [999] }] },
    { ...original, settings: { ...original.settings, keep: 0 } },
    {
      ...original,
      hosts: [
        { ...original.hosts[0], password: "blocked" },
        ...original.hosts.slice(1),
      ],
    },
    {
      ...original,
      settings: {
        ...original.settings,
        s3: {
          ...original.settings.s3,
          endpoint: "https://user:secret@example.com",
        },
      },
    },
  ];
  for (const data of cases) {
    if (typeof data === "object")
      expect(
        await page.evaluate((data) => {
          try {
            PanelBackup.validate(data);
            return false;
          } catch {
            return true;
          }
        }, data),
      ).toBe(true);
    await upload(page, typeof data === "string" ? data : JSON.stringify(data));
    await expect(page.locator('main [role="status"]')).toContainText(
      /格式错误|有效的 JSON|旧版明文/,
    );
    expect(await page.evaluate(() => hosts.length)).toBe(3);
    expect(await page.evaluate(() => history.length)).toBe(3);
    await expect(page.locator("#dialog")).not.toBeVisible();
  }
  await page.locator("#backup-file").setInputFiles({
    name: "large.json",
    mimeType: "application/json",
    buffer: Buffer.alloc(5 * 1024 * 1024 + 1, 32),
  });
  await expect(page.locator('main [role="status"]')).toContainText(
    "不能超过 5 MB",
  );
  const exportError = await page.evaluate(() => {
    const many = Array.from({ length: 1000 }, (_, i) => ({
      ...projects[0],
      id: i + 1,
      name: "project-" + i,
      path: "p".repeat(4096),
      dockerfile: "f".repeat(4096),
    }));
    try {
      PanelBackup.create(hosts, many, state);
      return "";
    } catch (error) {
      return error.message;
    }
  });
  expect(exportError).toContain("不能超过 5 MB");
});
test("backup text is rendered as text, not HTML", async ({ page }) => {
  const data = await fixture(page);
  data.hosts[0].name = '<img src=x onerror="window.injected=true">';
  await upload(page, data);
  await page.locator("#dialog summary").click();
  await expect(page.locator("#dialog")).toContainText("<img src=x");
  await expect(page.locator("#dialog img")).toHaveCount(0);
  await confirmImport(page);
  await page.locator('nav a[href="#/hosts"]').click();
  await expect(page.locator("main img")).toHaveCount(0);
  expect(await page.evaluate(() => window.injected)).toBeUndefined();
});
test("running releases block import and confirmation rechecks the lock", async ({
  page,
}) => {
  const data = await fixture(page);
  await upload(page, data);
  await page.locator("#backup-overwrite").check();
  // Model a task starting after file validation but before the user confirms.
  await page.evaluate(() => {
    state.deploy = {
      running: true,
      paused: false,
      ids: [1],
      rows: [{ status: "传输中" }],
    };
  });
  await page.getByRole("button", { name: "确认导入", exact: true }).click();
  await expect(page.locator('main [role="status"]')).toContainText(
    "发布尚未结束",
  );
  expect(await page.evaluate(() => history.length)).toBe(3);
  await page
    .locator("#dialog")
    .getByRole("button", { name: "取消", exact: true })
    .click();
  await expect(
    page.getByRole("button", { name: "选择文件导入", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByRole("button", { name: "导出配置", exact: true }),
  ).toBeEnabled();
});
test("S3 demo validates config, clears credentials and supports download, restore and delete", async ({
  page,
}) => {
  const outbound = [];
  page.on("request", (r) => {
    if (r.url().startsWith("https://s3.example.com")) outbound.push(r.url());
  });
  await page.getByRole("tab", { name: "S3 备份", exact: true }).click();
  await page
    .getByRole("button", { name: "测试连接（演示）", exact: true })
    .click();
  await expect(page.locator('main [role="status"]')).toContainText(
    "HTTPS Endpoint",
  );
  await configureS3(page);
  await page.locator('[name="s3-access-key"]').fill("DEMO_ID");
  await page.locator('[name="s3-secret-key"]').fill("DEMO_SECRET");
  await page
    .getByRole("button", { name: "测试连接（演示）", exact: true })
    .click();
  await expect(page.locator('[name="s3-secret-key"]')).toHaveValue("");
  await page
    .getByRole("button", { name: "创建备份（演示）", exact: true })
    .click();
  await expect(page.locator("main tbody tr")).toHaveCount(1);
  await expect(page.locator("main tbody")).toContainText("仅本地模拟");
  const downloaded = page.waitForEvent("download");
  await page.getByRole("button", { name: "下载", exact: true }).click();
  const text = await fs.readFile(await (await downloaded).path(), "utf8");
  expect(text).not.toContain("DEMO_SECRET");
  await page.getByRole("button", { name: "预览恢复", exact: true }).click();
  await unlock(page);
  await confirmImport(page);
  expect(await page.evaluate(() => hosts.every((h) => !h.online))).toBe(true);
  await page.getByRole("button", { name: "删除演示记录", exact: true }).click();
  await page
    .locator("#dialog")
    .getByRole("button", { name: "删除演示记录", exact: true })
    .click();
  await expect(page.getByText("暂无备份记录", { exact: true })).toBeVisible();
  expect(outbound).toEqual([]);
});
test("S3 config survives navigation, changed target requires a fresh demo test", async ({
  page,
}) => {
  await configureS3(page);
  await page.locator('nav a[href="#/dashboard"]').click();
  await page.locator('nav a[href="#/settings"]').click();
  await page.locator('.settings-nav a[href="#/settings/backup"]').click();
  await expect(page.locator('[name="s3-endpoint"]')).toHaveValue(
    "https://s3.example.com",
  );
  await expect(
    page.getByRole("button", { name: "创建备份（演示）", exact: true }),
  ).toBeEnabled();
  await page.locator('[name="s3-bucket"]').fill("new-bucket");
  await page.locator('[name="s3-bucket"]').blur();
  await expect(
    page.getByRole("button", { name: "创建备份（演示）", exact: true }),
  ).toBeDisabled();
});
test("backup views and preview fit mobile and dark mode", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator('[data-action="theme"]').click();
  await page.locator('[data-action="set-theme:dark"]').click();
  await configureS3(page);
  await page
    .getByRole("button", { name: "创建备份（演示）", exact: true })
    .click();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/backup-s3-mobile-dark.png",
    fullPage: true,
  });
  await page.getByRole("button", { name: "预览恢复", exact: true }).click();
  await unlock(page);
  await expect(page.locator("#dialog")).toBeVisible();
  expect(
    await page
      .locator("#dialog")
      .evaluate((el) => el.scrollWidth <= el.clientWidth),
  ).toBe(true);
  await page.screenshot({ path: "test-results/backup-import-mobile-dark.png" });
});
