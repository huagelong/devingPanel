const { test, expect } = require("@playwright/test");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const source = `version: 2
name: sample
tag: '{timestamp}'
builds:
  - service: web
    dockerfile: Dockerfile
    image: sample
    platform: auto
    args: {}
deploy:
  compose:
    files: [compose.yaml]
`;

test.beforeEach(async ({ page }) => {
  await page.addInitScript((initial) => {
    const files = {
      "devingpanel.yaml": initial,
      Dockerfile: "FROM scratch\n",
      "compose.yaml": "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n",
    };
    window.testFiles = files;
    window.showDirectoryPicker = async () => ({
      name: "sample-project",
      getDirectoryHandle: async () => { throw new DOMException("Missing directory", "NotFoundError"); },
      getFileHandle: async (name, options = {}) => {
        if (!(name in files) && !options.create) throw new DOMException("Missing file", "NotFoundError");
        return {
          getFile: async () => new File([files[name]], name),
          createWritable: async () => {
            let next;
            return {
              write: async (value) => { next = value; },
              close: async () => { files[name] = next; },
              abort: async () => {},
            };
          },
        };
      },
    });
  }, source);
});

test("import reads YAML and save writes the selected project file", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await expect(page.locator("h1")).toHaveText("sample");
  await page.locator('[name="manifest-tag"]').fill("release-{timestamp}");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
  const saved = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  expect(saved).toContain("release-{timestamp}");
  expect(saved).toContain("service: web");
  await page.locator('[name="manifest-tag"]').fill("another-{timestamp}");
  await page.evaluate(() => { window.testFiles["devingpanel.yaml"] += "# external change\n"; });
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator(".toast.error")).toContainText("外部修改");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("another-{timestamp}");
  await page.getByRole("tab", { name: "基本信息" }).click();
  await page.getByRole("button", { name: "选择目录" }).click();
  await page.getByRole("tab", { name: "构建配置" }).click();
  await expect(page.locator('[name="manifest-tag"]')).toHaveValue("release-{timestamp}");
});

test("project edit opens basic information and saves the YAML project name", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await page.locator('nav a[href="#/projects"]').click();
  await page.locator(".menu summary").last().click();
  await page.getByRole("link", { name: "编辑" }).click();
  await expect(page).toHaveURL(/#\/projects\/\d+\/basic$/);
  await expect(page.locator('[name="path"]')).toHaveAttribute("readonly", "");
  await page.locator('[name="manifest-name"]').fill("renamed-sample");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator("h1")).toHaveText("renamed-sample");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("name: renamed-sample");
});

test("Save commits the current tab and Save all commits remaining YAML drafts", async ({ page }) => {
  await page.goto("/#/projects");
  await page.evaluate(() => {
    window.testFiles["other.yaml"] = "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n";
  });
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await page.locator('[name="manifest-tag"]').fill("pending-{timestamp}");
  await page.getByRole("tab", { name: "Compose" }).click();
  await page.getByRole("button", { name: "添加 Compose 文件" }).click();
  await page.locator('[name="manifest-file-1"]').fill("other.yaml");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
  let saved = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  expect(saved).toContain("other.yaml");
  expect(saved).not.toContain("pending-{timestamp}");
  await page.getByRole("tab", { name: "构建配置" }).click();
  await expect(page.locator('[name="manifest-tag"]')).toHaveValue("pending-{timestamp}");
  await page.getByRole("tab", { name: "Compose" }).click();
  await page.getByRole("button", { name: "全部保存" }).click();
  await expect.poll(() => page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("pending-{timestamp}");
  saved = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  expect(saved).toContain("other.yaml");
  expect(saved).toContain("pending-{timestamp}");
});

test("target Save updates only local bindings; Save all writes YAML without targets", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await page.getByRole("tab", { name: "基本信息" }).click();
  await page.locator('[name="manifest-name"]').fill("pending-name");
  await page.getByRole("tab", { name: "构建配置" }).click();
  await page.locator('[name="manifest-tag"]').fill("pending-{timestamp}");
  await page.getByRole("tab", { name: "目标主机" }).click();
  await page.getByRole("button", { name: "添加目标" }).click();
  await page.locator('[name="manifest-group-0"]').fill("myapp-prod");
  await page.locator('[name="manifest-host-0"]').fill("web-2");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await page.getByRole("button", { name: "确认保存" }).click();
  await expect.poll(() => page.evaluate(() => projects.find((p) => p.name === "pending-name").hostIds)).toEqual([2]);
  let saved = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  expect(saved).toContain("name: sample");
  expect(saved).not.toContain("pending-{timestamp}");
  expect(saved).not.toContain("targets:");
  expect(saved).not.toContain("web-2");
  await page.getByRole("button", { name: "全部保存" }).click();
  await expect.poll(() => page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("name: pending-name");
  saved = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  expect(saved).toContain("pending-{timestamp}");
  expect(saved).not.toContain("targets:");
  expect(saved).not.toContain("web-2");
  expect(await page.evaluate(() => projects.find((p) => p.name === "pending-name").hostIds)).toEqual([2]);
});

test("target Save works without a YAML directory and leaves the file untouched", async ({ page }) => {
  await page.goto("/#/projects/1/targets");
  await page.locator('[data-action="manifest-target-remove:2"]').click();
  await page.locator('[data-action="manifest-target-remove:1"]').click();
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await page.getByRole("button", { name: "确认保存" }).click();
  await expect.poll(() => page.evaluate(() => projects[0].hostIds)).toEqual([1]);
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toBe(source);
});

test("Save all does not commit target bindings when YAML save fails", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await page.getByRole("tab", { name: "目标主机" }).click();
  await page.getByRole("button", { name: "添加目标" }).click();
  await page.locator('[name="manifest-group-0"]').fill("myapp-prod");
  await page.locator('[name="manifest-host-0"]').fill("web-1");
  await page.evaluate(() => { window.testFiles["devingpanel.yaml"] += "# external change\n"; });
  await page.getByRole("button", { name: "全部保存" }).click();
  await page.getByRole("button", { name: "确认保存" }).click();
  await expect(page.locator(".toast.error")).toContainText("外部修改");
  expect(await page.evaluate(() => projects.find((p) => p.name === "sample").hostIds)).toEqual([]);
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("targets:");
});

test("environment values never enter YAML through Save all", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  for (const tab of ["基本信息", "构建配置", "Compose", "目标主机"]) {
    await page.getByRole("tab", { name: tab }).click();
    await expect(page.getByRole("button", { name: "全部保存" })).toBeVisible();
  }
  await page.getByRole("tab", { name: "环境变量" }).click();
  await page.getByRole("button", { name: "添加变量" }).click();
  await page.locator('#env-form [name="key"]').fill("DEMO_VALUE");
  await page.locator('#env-form [name="value"]').fill("not-a-real-secret");
  await page.getByRole("button", { name: "全部保存" }).click();
  await expect(page.locator(".toast").filter({ hasText: "环境变量值未保存" })).toBeVisible();
  await expect(page.locator('#env-form [name="value"]')).toHaveValue("not-a-real-secret");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("not-a-real-secret");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("targets:");
  await page.locator('#env-form [data-action="env-save"]').click();
  await expect(page.locator('#env-form [name="value"]')).toHaveValue("");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("not-a-real-secret");
});

test("invalid YAML is rejected before import", async ({ page }) => {
  await page.goto("/#/projects");
  await page.evaluate(() => { window.testFiles["devingpanel.yaml"] += "name: duplicate\n"; });
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await expect(page.locator(".toast.error")).toBeVisible();
  await expect(page).toHaveURL(/#\/projects$/);
});

test("multi-build edits write all services when Compose references their image variables", async ({ page }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await expect(page.locator("h1")).toHaveText("sample");
  await page.getByRole("button", { name: "添加构建" }).click();
  await page.locator('[name="manifest-service-1"]').fill("worker");
  await page.locator('[name="manifest-image-1"]').fill("sample-worker");
  await page.evaluate(() => { window.testFiles["compose.yaml"] += "  worker:\n    image: ${DEVINGPANEL_IMAGE_WORKER}\n"; });
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("service: worker");
  await expect(page.getByRole("button", { name: "发布", exact: true })).toBeDisabled();
});

test("new project creates YAML only after its configuration is complete", async ({ page }) => {
  await page.goto("/#/projects");
  await page.evaluate(() => { delete window.testFiles["devingpanel.yaml"]; });
  await page.getByRole("button", { name: "新建项目" }).click();
  await page.locator('#project-form [name="name"]').fill("sample");
  await page.getByRole("button", { name: "创建项目" }).click();
  await expect(page.getByRole("button", { name: "发布", exact: true })).toBeDisabled();
  await page.getByRole("tab", { name: "基本信息" }).click();
  await page.getByRole("button", { name: "选择目录" }).click();
  await expect(page.getByText("目录中尚无 devingpanel.yaml，保存时创建")).toBeVisible();
  await page.getByRole("tab", { name: "Compose" }).click();
  await page.getByRole("button", { name: "添加 Compose 文件" }).click();
  await page.locator('[name="manifest-file-0"]').fill("compose.yaml");
  await page.getByRole("tab", { name: "目标主机" }).click();
  await page.getByRole("button", { name: "添加目标" }).click();
  await page.locator('[name="manifest-group-0"]').fill("myapp-prod");
  await page.locator('[name="manifest-host-0"]').fill("web-1");
  await page.getByRole("button", { name: "全部保存" }).click();
  await page.getByRole("button", { name: "确认保存" }).click();
  await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("name: sample");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("targets:");
  expect(await page.evaluate(() => projects.find((p) => p.name === "sample").hostIds)).toEqual([1]);
  await page.getByRole("tab", { name: "基本信息" }).click();
  await expect(page.getByText("已读取 devingpanel.yaml", { exact: true })).toBeVisible();
});

test("new project does not overwrite a YAML file added before its first save", async ({ page }) => {
  await page.goto("/#/projects");
  await page.evaluate(() => { delete window.testFiles["devingpanel.yaml"]; });
  await page.getByRole("button", { name: "新建项目" }).click();
  await page.locator('#project-form [name="name"]').fill("sample");
  await page.getByRole("button", { name: "创建项目" }).click();
  await page.getByRole("tab", { name: "基本信息" }).click();
  await page.getByRole("button", { name: "选择目录" }).click();
  await page.getByRole("tab", { name: "Compose" }).click();
  await page.getByRole("button", { name: "添加 Compose 文件" }).click();
  await page.locator('[name="manifest-file-0"]').fill("compose.yaml");
  await page.getByRole("tab", { name: "目标主机" }).click();
  await page.getByRole("button", { name: "添加目标" }).click();
  await page.locator('[name="manifest-group-0"]').fill("myapp-prod");
  await page.locator('[name="manifest-host-0"]').fill("web-1");
  await page.evaluate((yaml) => { window.testFiles["devingpanel.yaml"] = yaml; }, source);
  await page.getByRole("button", { name: "全部保存" }).click();
  await page.getByRole("button", { name: "确认保存" }).click();
  await expect(page.locator(".toast.error").filter({ hasText: "外部修改" })).toBeVisible();
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toBe(source);
});

test("Compose override and image variable collision are rejected", async ({ page }) => {
  await page.goto("/#/projects");
  await page.evaluate(() => {
    window.testFiles["override.yaml"] = "services:\n  web:\n    image: wrong:latest\n";
  });
  await page.getByRole("button", { name: "导入已配置项目" }).click();
  await page.getByRole("tab", { name: "Compose" }).click();
  await page.getByRole("button", { name: "添加 Compose 文件" }).click();
  await page.locator('[name="manifest-file-1"]').fill("override.yaml");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator(".toast.error")).toContainText("最终须使用");
  await page.getByRole("tab", { name: "构建配置" }).click();
  await page.locator('[name="manifest-service-0"]').fill("api-a");
  await page.getByRole("button", { name: "添加构建" }).click();
  await page.locator('[name="manifest-service-1"]').fill("api_a");
  await page.locator('[name="manifest-image-1"]').fill("sample-api");
  await page.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.locator(".toast.error").filter({ hasText: "镜像变量名不能重复" })).toBeVisible();
});

test("save changes a real project YAML file through the browser file-handle contract", async ({ page }) => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "devingpanel-manifest-"));
  const files = ["devingpanel.yaml", "Dockerfile", "compose.yaml"];
  fs.writeFileSync(path.join(directory, files[0]), source);
  fs.writeFileSync(path.join(directory, files[1]), "FROM scratch\n");
  fs.writeFileSync(path.join(directory, files[2]), "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n");
  try {
    await page.exposeBinding("readProjectFile", (_, name) => fs.readFileSync(path.join(directory, name), "utf8"));
    await page.exposeBinding("writeProjectFile", (_, name, value) => fs.writeFileSync(path.join(directory, name), value));
    await page.addInitScript(() => {
      window.showDirectoryPicker = async () => ({
        name: "project",
        getDirectoryHandle: async () => { throw new DOMException("Missing directory", "NotFoundError"); },
        getFileHandle: async (name, options = {}) => {
          if (!options.create) await window.readProjectFile(name);
          return {
            getFile: async () => new File([await window.readProjectFile(name)], name),
            createWritable: async () => {
              let content;
              return {
                write: async (value) => { content = value; },
                close: async () => window.writeProjectFile(name, content),
                abort: async () => {},
              };
            },
          };
        },
      });
    });
    await page.goto("/#/projects");
    await page.getByRole("button", { name: "导入已配置项目" }).click();
    await expect(page.locator("h1")).toHaveText("sample");
    await page.locator('[name="manifest-tag"]').fill("disk-{timestamp}");
    await page.getByRole("button", { name: "保存", exact: true }).click();
    await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
    expect(fs.readFileSync(path.join(directory, files[0]), "utf8")).toContain("disk-{timestamp}");
  } finally {
    files.forEach((name) => fs.unlinkSync(path.join(directory, name)));
    fs.rmdirSync(directory);
  }
});
