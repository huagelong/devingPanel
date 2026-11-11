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
  targets:
    - group: myapp-prod
      host: web-1
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
  await page.getByRole("button", { name: "保存到 devingpanel.yaml" }).click();
  await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
  const saved = await page.evaluate(() => window.testFiles["devingpanel.yaml"]);
  expect(saved).toContain("release-{timestamp}");
  expect(saved).toContain("service: web");
  await page.locator('[name="manifest-tag"]').fill("another-{timestamp}");
  await page.evaluate(() => { window.testFiles["devingpanel.yaml"] += "# external change\n"; });
  await page.getByRole("button", { name: "保存到 devingpanel.yaml" }).click();
  await expect(page.locator(".toast.error")).toContainText("外部修改");
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).not.toContain("another-{timestamp}");
  await page.getByRole("button", { name: "重新读取配置" }).click();
  await expect(page.locator('[name="manifest-tag"]')).toHaveValue("release-{timestamp}");
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
  await page.getByRole("button", { name: "保存到 devingpanel.yaml" }).click();
  await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
  expect(await page.evaluate(() => window.testFiles["devingpanel.yaml"])).toContain("service: worker");
  await expect(page.getByRole("button", { name: "发布", exact: true })).toBeDisabled();
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
    await page.getByRole("button", { name: "保存到 devingpanel.yaml" }).click();
    await expect(page.locator(".toast").filter({ hasText: "已写入项目目录" })).toBeVisible();
    expect(fs.readFileSync(path.join(directory, files[0]), "utf8")).toContain("disk-{timestamp}");
  } finally {
    files.forEach((name) => fs.unlinkSync(path.join(directory, name)));
    fs.rmdirSync(directory);
  }
});
