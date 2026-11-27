const { test, expect } = require("@playwright/test");
const { installProjectDirectory, connectProjectDirectory } = require("./project-directory.cjs");

test.beforeEach(async ({ page }) => {
  page.scriptErrors = [];
  page.on("pageerror", (e) => page.scriptErrors.push(e.message));
});
test.afterEach(async ({ page }) => expect(page.scriptErrors).toEqual([]));
async function editProject(page, tab = "build") {
  await installProjectDirectory(page);
  await page.goto(`/#/projects/1/${tab}`);
  await connectProjectDirectory(page);
}

test("failed file save can be corrected and retried from the same button", async ({ page }) => {
  await editProject(page);
  const dockerfile = page.locator('[name="manifest-dockerfile-0"]');
  await dockerfile.fill("MissingDockerfile");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator("#manifest-errors")).toContainText("MissingDockerfile");
  await expect(dockerfile).toBeFocused();
  await expect(dockerfile).toHaveAttribute("aria-invalid", "true");
  await page.locator(".toast").evaluateAll((nodes) => nodes.forEach((node) => node.remove()));
  await expect(page.locator("#manifest-errors")).toBeVisible();
  await dockerfile.fill("Dockerfile");
  await page.locator('[name="manifest-tag"]').fill("retry-{git-sha}");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("retry-{git-sha}");
  await expect(page.locator("#manifest-errors")).toBeHidden();
});

test("duplicate build args are rejected without changing the YAML", async ({ page }) => {
  await editProject(page);
  const original = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  await page.getByRole("button", { name: "添加参数" }).click();
  const duplicate = page.getByRole("textbox", { name: "构建 1 参数 2 名称", exact: true });
  await duplicate.fill("NODE_ENV");
  await page.getByRole("textbox", { name: "构建 1 参数 2 值", exact: true }).fill("test-mode");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(duplicate).toBeFocused();
  await expect(page.locator("#manifest-errors")).toContainText("参数名不能重复");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toBe(original);
  expect(await page.evaluate(() => { try { ManifestIO.serialize("myapp", manifestFor(project())); return "lost"; } catch (e) { return e.message; } })).toContain("不能重复");
  await duplicate.fill("MODE");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("MODE: test-mode");
});

test("theme switching preserves environment, security, registry and dialog inputs", async ({ page }) => {
  await page.goto("/#/projects/1/env");
  await page.getByRole("button", { name: "添加变量" }).click();
  await page.locator('[name="key"]').fill("TEST_MODE");
  await page.locator('[name="value"]').fill("demo-value");
  await page.locator('[name="value"]').focus();
  await page.evaluate(() => theme("dark"));
  await expect(page.locator('[name="value"]')).toBeFocused();
  await page.locator('[data-action="theme"]').click();
  await expect(page.locator('[name="key"]')).toHaveValue("TEST_MODE");
  await expect(page.locator('[name="value"]')).toHaveValue("demo-value");
  await page.goto("/#/settings/security");
  const passwords = page.locator('#content input[type="password"]');
  await passwords.first().fill("demo-only-password");
  await passwords.last().fill("demo-only-password");
  await page.locator('[data-action="theme"]').click();
  await expect(passwords.first()).toHaveValue("demo-only-password");
  await expect(passwords.last()).toHaveValue("demo-only-password");
  await page.goto("/#/settings/registry");
  const token = page.locator('#content input[type="password"]');
  await token.fill("DEMO_INVALID_TOKEN");
  await page.locator('[data-action="theme"]').click();
  await expect(token).toHaveValue("DEMO_INVALID_TOKEN");
  await page.goto("/#/hosts");
  await page.getByRole("button", { name: "添加主机" }).click();
  await page.locator('#dialog [name="address"]').fill("demo.invalid");
  await page.evaluate(() => theme("dark"));
  await expect(page.locator('#dialog [name="address"]')).toHaveValue("demo.invalid");
});

test("project name stays a draft until save and can be discarded", async ({ page }) => {
  await editProject(page, "basic");
  await page.locator('[name="manifest-name"]').fill("draft-name");
  await expect(page.locator("#project-draft")).toBeVisible();
  await expect(page.locator("h1")).toHaveText("myapp");
  await page.locator('nav a[href="#/projects"]').click();
  await expect(page.locator("tbody")).toContainText("myapp");
  await expect(page.locator("tbody")).not.toContainText("draft-name");
  await page.locator('tbody a[href="#/projects/1"]').click();
  await page.getByRole("tab", { name: "基本信息" }).click();
  await expect(page.locator('[name="manifest-name"]')).toHaveValue("draft-name");
  await page.getByRole("button", { name: "放弃修改" }).click();
  await expect(page.locator('[name="manifest-name"]')).toHaveValue("myapp");
  await expect(page.locator("#project-draft")).toBeHidden();
});

test("settings preserve focus and entering custom retention does not change its value", async ({ page }) => {
  await page.goto("/#/settings/retention");
  const retention = page.locator('[name="retention"]');
  await retention.focus();
  await retention.selectOption("custom");
  await expect(retention).toBeFocused();
  await expect(page.locator('[name="retention-custom"]')).toHaveValue("3");
  expect(await page.evaluate(() => state.keep)).toBe(3);
  await page.locator('[name="retention-custom"]').fill("7");
  await page.locator('[name="retention-custom"]').press("Tab");
  expect(await page.evaluate(() => state.keep)).toBe(7);
  await page.goto("/#/settings/appearance");
  const theme = page.locator('[name="theme"][value="dark"]');
  await theme.focus();
  await theme.check();
  await expect(theme).toBeFocused();
  await expect(page.locator('html')).toHaveClass(/dark/);
});

test("narrow windows show selected tabs and every table action without horizontal scrolling", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 1000 });
  await page.goto("/#/projects/1/history");
  const active = page.getByRole("tab", { selected: true });
  const strip = await page.locator('.tabs').boundingBox(), selected = await active.boundingBox();
  expect(selected.x).toBeGreaterThanOrEqual(strip.x);
  expect(selected.x + selected.width).toBeLessThanOrEqual(strip.x + strip.width + 1);
  await expect(page.locator('.tab-scroll-hint')).toBeVisible();
  for (const route of ["dashboard", "hosts", "projects", "deploy/0/history", "containers/1/containers", "containers/1/images"]) {
    await page.goto(`/#/${route}`);
    const layout = await page.evaluate(() => ({
      page: document.documentElement.scrollWidth,
      actions: [...document.querySelectorAll('td.action-col')].map((el) => ({ left: el.getBoundingClientRect().left, right: el.getBoundingClientRect().right })),
      labels: [...document.querySelectorAll('tbody td')].every((el) => el.hasAttribute('data-label')),
      tables: [...document.querySelectorAll('.table-wrap')].every((el) => el.scrollWidth <= el.clientWidth + 1),
    }));
    expect(layout.page).toBeLessThanOrEqual(390);
    expect(layout.tables).toBe(true);
    expect(layout.labels).toBe(true);
    for (const action of layout.actions) { expect(action.left).toBeGreaterThanOrEqual(0); expect(action.right).toBeLessThanOrEqual(390); }
  }
});

test("container confirmation identifies the host, container and action", async ({ page }) => {
  await page.goto("/#/containers/1/containers");
  await page.locator('tbody tr').first().locator('.menu summary').click();
  await page.locator('[data-action="container-stop:1-1"]').click();
  const identity = await page.evaluate(() => { const c = containers.find((item) => item.id === '1-1'); return { host: hosts.find((h) => h.id === c.host).name, container: c.name }; });
  await expect(page.locator('#dialog')).toContainText(identity.host);
  await expect(page.locator('#dialog')).toContainText(identity.container);
  await expect(page.locator('#dialog').getByRole('button', { name: '停止', exact: true })).toBeVisible();
});

test("host info agrees with missing Docker and CPU trend includes readable numbers", async ({ page }) => {
  await page.goto("/#/hosts/1/info");
  await page.evaluate(() => { state.previewDockerMissing = true; render(); });
  await expect(page.locator('.info-list')).toContainText("未安装");
  await expect(page.locator('.info-list')).not.toContainText("27.3.1");
  await expect(page.locator('.info-list')).toContainText("密码（演示）");
  await page.evaluate(() => { state.previewDockerMissing = false; state.stage = 'p2'; location.hash = '#/hosts/1/monitor'; });
  await expect(page.locator('.chart')).toContainText("100%");
  await expect(page.locator('.chart')).toHaveAttribute('aria-label', /当前 42%.*峰值 78%/);
});

test("Save all keeps environment values when another tab has a validation error", async ({ page }) => {
  await editProject(page);
  await page.locator('[name="manifest-tag"]').fill("");
  await page.getByRole('tab', { name: '环境变量' }).click();
  await page.getByRole('button', { name: '添加变量' }).click();
  await page.locator('[name="key"]').fill('DEMO_MODE');
  await page.locator('[name="value"]').fill('demo-only');
  await page.getByRole('button', { name: '全部保存' }).click();
  await expect(page.locator('#manifest-errors')).toContainText('Tag 规则不能为空');
  await expect(page.locator('[name="value"]')).toHaveValue('demo-only');
});

test("edits made during disk I/O remain unsaved drafts after the submitted snapshot succeeds", async ({ page }) => {
  await editProject(page);
  await page.evaluate(() => {
    const original = ManifestIO.save;
    ManifestIO.save = async (...args) => { await new Promise((resolve) => { window.releaseSave = resolve; }); return original(...args); };
  });
  await page.locator('[name="manifest-tag"]').fill('submitted-{git-sha}');
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await expect(page.getByRole('button', { name: '保存', exact: true })).toBeDisabled();
  await page.locator('[name="manifest-tag"]').fill('later-{git-sha}');
  await page.evaluate(() => window.releaseSave());
  await expect.poll(() => page.evaluate(() => window.testFiles['devingpanel.yaml'])).toContain('submitted-{git-sha}');
  await expect(page.locator('[name="manifest-tag"]')).toHaveValue('later-{git-sha}');
  await expect(page.locator('#project-draft')).toBeVisible();
});

test("row labels are contextual and renumber after additions and removals in English", async ({ page }) => {
  await page.goto('/#/settings/appearance');
  await page.locator('[name="language"]').selectOption('en');
  await page.goto('/#/projects/1/build');
  await page.getByRole('button', { name: 'Add argument', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Build 1, argument 2, name', exact: true })).toBeVisible();
  await page.locator('[data-action="manifest-arg-remove:0:0"]').click();
  await expect(page.getByRole('textbox', { name: 'Build 1, argument 1, name', exact: true })).toBeVisible();
  await expect(page.getByRole('textbox', { name: 'Build 1, argument 2, name', exact: true })).toHaveCount(0);
  await page.getByRole('tab', { name: 'Compose', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Compose file 1', exact: true })).toBeVisible();
  await page.goto('/#/hosts');
  await page.getByRole('button', { name: 'Add host', exact: true }).click();
  await page.locator('[name="connection-mode"][value="jump"]').check();
  await expect(page.getByRole('combobox', { name: 'Jump host 1', exact: true })).toBeVisible();
  await page.locator('[data-action="host-jump-add"]').click();
  await expect(page.getByRole('combobox', { name: 'Jump host 2', exact: true })).toBeVisible();
  await page.locator('[data-action="host-jump-remove"]').first().click();
  await expect(page.getByRole('combobox', { name: 'Jump host 1', exact: true })).toBeVisible();
});

test("Save all locates errors on another configuration tab without discarding drafts", async ({ page }) => {
  await editProject(page);
  await page.locator('[name="manifest-image-0"]').fill('');
  await page.getByRole('tab', { name: 'Compose', exact: true }).click();
  await page.locator('[name="manifest-file-1"]').fill('sub/prod.yml');
  await page.getByRole('button', { name: '全部保存' }).click();
  await expect(page.locator('[name="manifest-image-0"]')).toBeFocused();
  await expect(page.locator('#manifest-errors')).toContainText('请填写镜像名');
  await page.getByRole('tab', { name: 'Compose', exact: true }).click();
  await expect(page.locator('[name="manifest-file-1"]')).toHaveValue('sub/prod.yml');
});

test("write permission failures remain actionable and YAML syntax errors do not echo file content", async ({ page }) => {
  await editProject(page);
  await page.evaluate(() => {
    const directory = project().directoryHandle, getFile = directory.getFileHandle.bind(directory);
    window.denyWrite = true;
    directory.getFileHandle = async (name, options) => {
      const file = await getFile(name, options);
      if (name === 'devingpanel.yaml' && options?.create) {
        const create = file.createWritable;
        file.createWritable = async () => { if (window.denyWrite) throw new DOMException('Write denied', 'NotAllowedError'); return create(); };
      }
      return file;
    };
  });
  await page.locator('[name="manifest-tag"]').fill('first-{git-sha}');
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await expect(page.locator('#manifest-errors')).toContainText('检查目录写入权限');
  await page.evaluate(() => { window.denyWrite = false; });
  await page.locator('[name="manifest-tag"]').fill('corrected-{git-sha}');
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await expect.poll(() => page.evaluate(() => window.testFiles['devingpanel.yaml'])).toContain('corrected-{git-sha}');
  await page.evaluate(() => { window.testFiles['devingpanel.yaml'] = 'invalid: [DEMO_CONTENT_SHOULD_NOT_APPEAR'; });
  await page.getByRole('button', { name: '保存', exact: true }).click();
  await expect(page.locator('#manifest-errors')).toContainText('YAML 语法');
  await expect(page.locator('#manifest-errors')).not.toContainText('DEMO_CONTENT_SHOULD_NOT_APPEAR');
  await expect(page.locator('#toasts')).not.toContainText('DEMO_CONTENT_SHOULD_NOT_APPEAR');
});
