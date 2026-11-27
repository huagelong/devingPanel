"use strict";
const ManifestIO = (() => {
  const filename = "devingpanel.yaml";
  const limit = 64 * 1024;
  const encoder = new TextEncoder();
  const imageVariable = (service) => `DEVINGPANEL_IMAGE_${service.toUpperCase().replace(/-/g, "_")}`;
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
    try {
      for (const part of parts.slice(0, -1)) current = await current.getDirectoryHandle(part);
      return await current.getFileHandle(parts.at(-1));
    } catch {
      const error = new Error(I18n.t(I18n.msg`无法读取项目文件：${path}。请确认文件存在且允许读取。`));
      throw error;
    }
  }
  function parse(source) {
    if (encoder.encode(source).length > limit) throw new Error("devingpanel.yaml 不能超过 64 KiB。");
    let data;
    try { data = jsyaml.load(source, { schema: jsyaml.JSON_SCHEMA }); }
    catch { throw new Error("devingpanel.yaml 格式错误。请检查 YAML 语法后重新选择目录。"); }
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
    if (new Set(builds.map((build) => imageVariable(build.service))).size !== builds.length) throw new Error("构建服务生成的镜像变量名不能重复。");
    exact(data.deploy, ["compose"], "部署配置");
    exact(data.deploy.compose, ["files"], "Compose 配置");
    const files = data.deploy.compose.files;
    if (!Array.isArray(files) || !files.length || files.length > 30 || files.some((file, i) => { try { pathParts(file, i === 0); return !/\.ya?ml$/i.test(file); } catch { return true; } }) || new Set(files).size !== files.length) throw new Error("Compose 文件路径无效；首个文件须位于项目根目录。");
    return { name: data.name, manifest: { tag: data.tag, builds, files: [...files] } };
  }
  function serialize(name, manifest) {
    for (const build of manifest.builds) {
      if (new Set(build.args.map(([key]) => key)).size !== build.args.length) throw new Error("同一构建内的参数名不能重复。");
    }
    const data = {
      version: 2,
      name,
      tag: manifest.tag,
      builds: manifest.builds.map((build) => ({ service: build.service, dockerfile: build.dockerfile, image: build.image, platform: build.platform, args: Object.fromEntries(build.args) })),
      deploy: { compose: { files: manifest.files } },
    };
    const source = jsyaml.dump(data, { lineWidth: -1, noRefs: true });
    parse(source);
    return source;
  }
  async function verifyFiles(directory, manifest) {
    for (const [index, build] of manifest.builds.entries()) {
      try { await fileIn(directory, build.dockerfile); }
      catch (error) { error.manifestField = `manifest-dockerfile-${index}`; error.manifestTab = "build"; throw error; }
    }
    const images = new Map();
    for (const [index, path] of manifest.files.entries()) {
      try {
        const file = await fileIn(directory, path);
        const compose = jsyaml.load(await (await file.getFile()).text(), { schema: jsyaml.JSON_SCHEMA });
        for (const [service, config] of Object.entries(compose?.services || {})) {
          if (config && Object.hasOwn(config, "image")) images.set(service, config.image);
        }
      } catch {
        // YAML parser diagnostics can contain file content; show only the path.
        const error = new Error(I18n.t(I18n.msg`无法读取或解析 Compose 文件：${path}。请检查文件、权限和 YAML 格式。`));
        error.manifestField = `manifest-file-${index}`;
        error.manifestTab = "compose";
        throw error;
      }
    }
    for (const build of manifest.builds) {
      const variable = imageVariable(build.service);
      if (images.get(build.service) !== `\${${variable}}`) {
        const error = new Error(I18n.t(I18n.msg`Compose 服务 ${build.service} 最终须使用 ${"${" + variable + "}"} 镜像变量。`));
        error.manifestField = "manifest-file-0";
        error.manifestTab = "compose";
        throw error;
      }
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
      if (expectedSource !== null) throw new Error("项目配置文件已被删除，请重新选择项目目录读取配置。");
    }
    if (current && current.source !== expectedSource) throw new Error("项目配置文件已被外部修改，请重新选择项目目录读取配置。");
    if (!current && expectedSource !== null) throw new Error("项目配置文件已被删除，请重新选择项目目录读取配置。");
    const source = serialize(name, manifest);
    if (encoder.encode(source).length > limit) throw new Error("devingpanel.yaml 不能超过 64 KiB。");
    await verifyFiles(directory, manifest);
    try {
      const file = await directory.getFileHandle(filename, { create: true });
      const writer = await file.createWritable();
      try { await writer.write(source); await writer.close(); }
      catch (error) { await writer.abort().catch(() => {}); throw error; }
    } catch { throw new Error("无法写入 devingpanel.yaml。请检查目录写入权限后重试。"); }
    return source;
  }
  async function pick() {
    if (!window.isSecureContext || typeof window.showDirectoryPicker !== "function") throw new Error("请在支持目录读写的 Chrome/Edge 中通过 localhost 打开页面。");
    return window.showDirectoryPicker({ mode: "readwrite" });
  }
  return { parse, serialize, verifyFiles, read, save, pick };
})();
