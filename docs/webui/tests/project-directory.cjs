const { expect } = require("@playwright/test");

async function installProjectDirectory(page, initialYaml = true) {
  await page.addInitScript((withYaml) => {
    const files = {
      Dockerfile: "FROM scratch\n",
      "docker-compose.yml": "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n",
      "docker-compose.prod.yml": "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n",
      "sub/prod.yml": "services:\n  web:\n    image: ${DEVINGPANEL_IMAGE_WEB}\n",
    };
    if (withYaml) files["devingpanel.yaml"] = `version: 2
name: myapp
tag: '{git-sha}'
builds:
  - service: web
    dockerfile: Dockerfile
    image: myapp
    platform: auto
    args:
      NODE_ENV: production
deploy:
  compose:
    files: [docker-compose.yml, docker-compose.prod.yml]
`;
    window.testFiles = files;
    const directory = (prefix = "") => ({
      name: prefix || "myapp",
      getDirectoryHandle: async (name) => directory(prefix + name + "/"),
      getFileHandle: async (name, options = {}) => {
        const path = prefix + name;
        if (!(path in files) && !options.create) throw new DOMException("Missing file", "NotFoundError");
        return {
          getFile: async () => new File([files[path]], name),
          createWritable: async () => {
            let content;
            return {
              write: async (value) => { content = value; },
              close: async () => { files[path] = content; },
              abort: async () => {},
            };
          },
        };
      },
    });
    window.showDirectoryPicker = async () => directory();
  }, initialYaml);
}

async function connectProjectDirectory(page, initialYaml = true) {
  const selectedTab = await page.getByRole("tab", { selected: true }).textContent();
  if (selectedTab !== "基本信息") await page.getByRole("tab", { name: "基本信息" }).click();
  await page.getByRole("button", { name: "选择目录" }).click();
  await expect(page.getByText(initialYaml ? "已读取 devingpanel.yaml" : "目录中尚无 devingpanel.yaml，保存时创建", { exact: true })).toBeVisible();
  if (selectedTab !== "基本信息") await page.getByRole("tab", { name: selectedTab, exact: true }).click();
}

module.exports = { installProjectDirectory, connectProjectDirectory };
