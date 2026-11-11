"use strict";
const ManifestIO = (() => {
  const filename = "devingpanel.yaml";
  const limit = 64 * 1024;
  const encoder = new TextEncoder();
  const exact = (value, keys, label) => {
    if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).some((key) => !keys.includes(key))) throw new Error(`${label} 包含未知字段或格式错误。`);
  };
  const pathParts = (path, rootOnly = false) => {
    if (typeof path !== "string" || !path || path.includes("\\") || path.includes(":") || path.startsWith("/") || path.split("/").some((part) => !part || part === "." || part === "..") || (rootOnly && path.includes("/"))) throw new Error(`无效的项目内相对路径：${path}`);
    return path.split("/");
  };
  async function fileIn(directory, path) {
    const parts = pathParts(path);
    let current = directory;
    for (const part of parts.slice(0, -1)) current = await current.getDirectoryHandle(part);
    return current.getFileHandle(parts.at(-1));
  }
  function parse(source) {
    if (encoder.encode(source).length > limit) throw new Error("devingpanel.yaml 不能超过 64 KiB。");
    const data = jsyaml.load(source, { schema: jsyaml.JSON_SCHEMA });
    exact(data, ["version", "name", "tag", "builds", "deploy"], "项目配置");
    if (data.version !== 2) throw new Error("只支持 devingpanel.yaml version: 2。");
    if (typeof data.name !== "string" || !data.name.trim() || typeof data.tag !== "string" || !data.tag.trim()) throw new Error("项目名和 Tag 规则不能为空。");
    if (!Array.isArray(data.builds) || !data.builds.length || data.builds.length > 30) throw new Error("请配置 1–30 个构建服务。");
    const builds = data.builds.map((build) => {
      exact(build, ["service", "dockerfile", "image", "platform", "args"], "构建配置");
      if (typeof build.service !== "string" || !/^[A-Za-z][A-Za-z0-9_-]*$/.test(build.service) || typeof build.image !== "string" || !build.image.trim() || !["auto", "linux/amd64", "linux/arm64"].includes(build.platform)) throw new Error("构建服务、镜像或平台无效。");
      pathParts(build.dockerfile);
      exact(build.args || {}, Object.keys(build.args || {}), "构建参数");
      if (Object.entries(build.args || {}).some(([key, value]) => !/^[A-Za-z_][A-Za-z0-9_]*$/.test(key) || typeof value !== "string")) throw new Error("构建参数必须是字符串键值。");
      return { service: build.service, dockerfile: build.dockerfile, image: build.image, platform: build.platform, args: Object.entries(build.args || {}) };
    });
    if (new Set(builds.map((build) => build.service)).size !== builds.length) throw new Error("构建服务名不能重复。");
    exact(data.deploy, ["compose", "targets"], "部署配置");
    exact(data.deploy.compose, ["files"], "Compose 配置");
    const files = data.deploy.compose.files;
    if (!Array.isArray(files) || !files.length || files.length > 30 || files.some((file, i) => { try { pathParts(file, i === 0); return !/\.ya?ml$/i.test(file); } catch { return true; } }) || new Set(files).size !== files.length) throw new Error("Compose 文件路径无效；首个文件须位于项目根目录。");
    const targets = data.deploy.targets;
    if (!Array.isArray(targets) || !targets.length || targets.length > 100) throw new Error("请配置目标主机。");
    targets.forEach((target) => {
      exact(target, ["group", "host"], "目标主机");
      if (typeof target.group !== "string" || !target.group.trim() || typeof target.host !== "string" || !target.host.trim()) throw new Error("目标组和主机别名不能为空。");
    });
    if (new Set(targets.map((target) => `${target.group}\0${target.host}`)).size !== targets.length) throw new Error("目标主机不能重复。");
    return { name: data.name, manifest: { tag: data.tag, builds, files: [...files], targets: targets.map((target) => ({ ...target })) } };
  }
  function serialize(name, manifest) {
    const data = {
      version: 2,
      name,
      tag: manifest.tag,
      builds: manifest.builds.map((build) => ({ service: build.service, dockerfile: build.dockerfile, image: build.image, platform: build.platform, args: Object.fromEntries(build.args) })),
      deploy: { compose: { files: manifest.files }, targets: manifest.targets },
    };
    const source = jsyaml.dump(data, { lineWidth: -1, noRefs: true });
    parse(source);
    return source;
  }
  async function verifyFiles(directory, manifest) {
    for (const build of manifest.builds) await fileIn(directory, build.dockerfile);
    for (const path of manifest.files) await fileIn(directory, path);
    for (const build of manifest.builds) {
      const variable = `DEVINGPANEL_IMAGE_${build.service.toUpperCase().replace(/-/g, "_")}`;
      let found = false;
      for (const path of manifest.files) {
        const file = await fileIn(directory, path);
        const compose = jsyaml.load(await (await file.getFile()).text(), { schema: jsyaml.JSON_SCHEMA });
        if (compose?.services?.[build.service]?.image === `\${${variable}}`) found = true;
      }
      if (!found) throw new Error(`Compose 服务 ${build.service} 须使用 \${${variable}} 镜像变量。`);
    }
  }
  async function read(directory) {
    const file = await directory.getFileHandle(filename);
    const source = await (await file.getFile()).text();
    return { ...parse(source), source };
  }
  async function save(directory, expectedSource, name, manifest) {
    let current;
    try { current = await read(directory); } catch (error) {
      if (error.name !== "NotFoundError") throw error;
      if (expectedSource !== null) throw new Error("项目配置文件已被删除，请重新读取配置。");
    }
    if (current && current.source !== expectedSource) throw new Error("项目配置文件已被外部修改，请重新读取配置。");
    if (!current && expectedSource !== null) throw new Error("项目配置文件已被删除，请重新读取配置。");
    const source = serialize(name, manifest);
    if (encoder.encode(source).length > limit) throw new Error("devingpanel.yaml 不能超过 64 KiB。");
    await verifyFiles(directory, manifest);
    const file = await directory.getFileHandle(filename, { create: true });
    const writer = await file.createWritable();
    try { await writer.write(source); await writer.close(); } catch (error) { await writer.abort().catch(() => {}); throw error; }
    return source;
  }
  async function pick() {
    if (!window.isSecureContext || typeof window.showDirectoryPicker !== "function") throw new Error("请在支持目录读写的 Chrome/Edge 中通过 localhost 打开页面。");
    return window.showDirectoryPicker({ mode: "readwrite" });
  }
  return { parse, serialize, verifyFiles, read, save, pick };
})();
