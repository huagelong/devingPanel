/* Versioned, credential-free configuration format shared by local and demo S3 backups. */
"use strict";
const PanelBackup = (() => {
  const MAX_BYTES = 5 * 1024 * 1024;
  const FORMAT = "devingpanel-config";
  const ENCRYPTED_FORMAT = "devingpanel-encrypted-backup";
  const ITERATIONS = 600000;
  const encoder = new TextEncoder();
  function invalid(path) {
    throw new Error(`备份格式错误：${path}`);
  }
  function text(value, path, allowEmpty = true) {
    if (
      typeof value !== "string" ||
      value.length > 4096 ||
      (!allowEmpty && !value.trim())
    )
      invalid(path);
  }
  function integer(value, path, min = 1, max = Number.MAX_SAFE_INTEGER) {
    if (!Number.isSafeInteger(value) || value < min || value > max)
      invalid(path);
  }
  function choice(value, options, path) {
    if (!options.includes(value)) invalid(path);
  }
  function shape(value, fields, path) {
    if (!value || typeof value !== "object" || Array.isArray(value))
      invalid(path);
    if (Object.keys(value).some((key) => !Object.hasOwn(fields, key)))
      invalid(`${path} 包含未知字段或敏感字段`);
    for (const [key, check] of Object.entries(fields))
      check(value[key], `${path}.${key}`);
  }
  function list(value, check, path, max = 1000) {
    if (!Array.isArray(value) || value.length > max) invalid(path);
    value.forEach((item, index) => check(item, `${path}[${index}]`));
  }
  const required = (v, p) => text(v, p, false);
  const bool = (v, p) => {
    if (typeof v !== "boolean") invalid(p);
  };
  function validate(data) {
    shape(
      data,
      {
        format: (v, p) => choice(v, [FORMAT], p),
        version: (v, p) => choice(v, [1], p),
        createdAt: (v, p) => {
          required(v, p);
          if (!Number.isFinite(Date.parse(v))) invalid(p);
        },
        hosts: (v, p) =>
          list(
            v,
            (host, at) =>
              shape(
                host,
                {
                  id: integer,
                  name: required,
                  address: required,
                  port: (v, p) => integer(v, p, 1, 65535),
                  user: required,
                  group: required,
                  arch: (v, p) => choice(v, ["amd64", "arm64"], p),
                },
                at,
              ),
            p,
          ),
        projects: (v, p) =>
          list(
            v,
            (project, at) =>
              shape(
                project,
                {
                  id: integer,
                  name: required,
                  path: required,
                  dockerfile: required,
                  image: required,
                  tag: required,
                  platform: (v, p) =>
                    choice(v, ["auto", "linux/amd64", "linux/arm64"], p),
                  service: text,
                  hostIds: (v, p) => list(v, integer, p),
                  files: (v, p) => list(v, required, p, 100),
                  buildArgKeys: (v, p) =>
                    list(
                      v,
                      (key, at) => {
                        required(key, at);
                        if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) invalid(at);
                      },
                      p,
                      200,
                    ),
                },
                at,
              ),
            p,
          ),
        settings: (v, p) =>
          shape(
            v,
            {
              theme: (v, p) => choice(v, ["light", "dark", "system"], p),
              keep: integer,
              registryAddress: (v, p) => {
                text(v, p);
                if (/[?#@]/.test(v)) invalid(`${p} 不能含凭据或查询参数`);
              },
              registryUser: text,
              alerts: (v, p) =>
                shape(
                  v,
                  {
                    cpu: (v, p) => integer(v, p, 1, 100),
                    memory: (v, p) => integer(v, p, 1, 100),
                    disk: (v, p) => integer(v, p, 1, 100),
                    duration: (v, p) =>
                      choice(v, ["1 分钟", "5 分钟", "10 分钟"], p),
                  },
                  p,
                ),
              s3: (v, p) =>
                shape(
                  v,
                  {
                    endpoint: (v, p) => {
                      text(v, p);
                      if (v && endpointError(v)) invalid(p);
                    },
                    region: text,
                    bucket: text,
                    prefix: text,
                    pathStyle: bool,
                  },
                  p,
                ),
            },
            p,
          ),
      },
      "文件",
    );
    const unique = (values, path) => {
      if (new Set(values).size !== values.length) invalid(`${path} 重复`);
    };
    unique(
      data.hosts.map((h) => h.id),
      "主机 ID",
    );
    unique(
      data.projects.map((p) => p.id),
      "项目 ID",
    );
    unique(
      data.projects.map((p) => p.name),
      "项目名称",
    );
    const byHost = new Map(data.hosts.map((h) => [h.id, h]));
    for (const project of data.projects) {
      unique(project.hostIds, "目标主机 ID");
      unique(project.buildArgKeys, "构建参数名称");
      if (project.hostIds.some((id) => !byHost.has(id)))
        invalid("项目引用了不存在的主机");
      if (new Set(project.hostIds.map((id) => byHost.get(id).group)).size > 1)
        invalid("项目目标主机不属于同一分组");
    }
    return data;
  }
  function create(hosts, projects, settings) {
    // Do not spread application objects: runtime fields and secrets must never enter the file.
    const data = validate({
      format: FORMAT,
      version: 1,
      createdAt: new Date().toISOString(),
      hosts: hosts.map((h) => ({
        id: h.id,
        name: h.name,
        address: h.address,
        port: h.port,
        user: h.user,
        group: h.group,
        arch: h.arch,
      })),
      projects: projects.map((p) => ({
        id: p.id,
        name: p.name,
        path: p.path,
        dockerfile: p.dockerfile,
        image: p.image,
        tag: p.tag,
        platform: p.platform,
        service: p.service,
        hostIds: [...p.hostIds],
        files: [...p.files],
        buildArgKeys: [...new Set(p.args.map(([key]) => key).filter(Boolean))],
      })),
      settings: {
        theme: settings.theme,
        keep: settings.keep,
        registryAddress: settings.registryAddress,
        registryUser: settings.registryUser,
        alerts: {
          cpu: settings.alerts.cpu,
          memory: settings.alerts.memory,
          disk: settings.alerts.disk,
          duration: settings.alerts.duration,
        },
        s3: {
          endpoint: settings.s3.endpoint,
          region: settings.s3.region,
          bucket: settings.s3.bucket,
          prefix: settings.s3.prefix,
          pathStyle: settings.s3.pathStyle,
        },
      },
    });
    if (
      new TextEncoder().encode(JSON.stringify(data, null, 2) + "\n")
        .byteLength > MAX_BYTES
    )
      throw new Error("备份文件不能超过 5 MB。");
    return data;
  }
  function parse(text) {
    if (new TextEncoder().encode(text).byteLength > MAX_BYTES)
      throw new Error("备份文件不能超过 5 MB。");
    let data;
    try {
      data = JSON.parse(text);
    } catch {
      throw new Error("文件不是有效的 JSON 备份。");
    }
    if (data?.format === FORMAT)
      throw new Error("仅支持加密备份，不接受旧版明文 JSON。");
    return validateEnvelope(data);
  }
  function base64(bytes) {
    let binary = "";
    for (let i = 0; i < bytes.length; i += 32768)
      binary += String.fromCharCode(...bytes.subarray(i, i + 32768));
    return btoa(binary);
  }
  function fromBase64(value, path, length) {
    if (
      typeof value !== "string" ||
      value.length > MAX_BYTES ||
      value.length % 4 !== 0 ||
      !/^[A-Za-z0-9+/]*={0,2}$/.test(value)
    )
      invalid(path);
    const bytes = Uint8Array.from(atob(value), (c) => c.charCodeAt(0));
    if (
      (length !== undefined && bytes.length !== length) ||
      (length === undefined && bytes.length < 16)
    )
      invalid(path);
    return bytes;
  }
  function validateEnvelope(data) {
    shape(
      data,
      {
        format: (v, p) => choice(v, [ENCRYPTED_FORMAT], p),
        version: (v, p) => choice(v, [1], p),
        createdAt: (v, p) => {
          required(v, p);
          if (!Number.isFinite(Date.parse(v))) invalid(p);
        },
        cipher: (v, p) => choice(v, ["AES-256-GCM"], p),
        kdf: (v, p) =>
          shape(
            v,
            {
              name: (v, p) => choice(v, ["PBKDF2"], p),
              hash: (v, p) => choice(v, ["SHA-256"], p),
              iterations: (v, p) => choice(v, [ITERATIONS], p),
              salt: (v, p) => fromBase64(v, p, 16),
            },
            p,
          ),
        iv: (v, p) => fromBase64(v, p, 12),
        ciphertext: (v, p) => fromBase64(v, p),
      },
      "加密文件",
    );
    if (
      encoder.encode(JSON.stringify(data, null, 2) + "\n").byteLength >
      MAX_BYTES
    )
      throw new Error("备份文件不能超过 5 MB。");
    return data;
  }
  function checkPassword(password) {
    if (
      typeof password !== "string" ||
      password.length < 8 ||
      password.length > 1024
    )
      throw new Error("加密密码须为 8–1024 个字符，建议使用较长且唯一的口令。");
  }
  async function deriveKey(password, salt, usage) {
    if (!globalThis.crypto?.subtle)
      throw new Error(
        "当前浏览器不支持安全加密，请使用 HTTPS 或本机 localhost 预览；不会退回明文备份。",
      );
    const bytes = encoder.encode(password);
    try {
      const material = await crypto.subtle.importKey(
        "raw",
        bytes,
        "PBKDF2",
        false,
        ["deriveKey"],
      );
      return await crypto.subtle.deriveKey(
        { name: "PBKDF2", hash: "SHA-256", salt, iterations: ITERATIONS },
        material,
        { name: "AES-GCM", length: 256 },
        false,
        [usage],
      );
    } finally {
      bytes.fill(0);
    }
  }
  function authenticatedHeader(data) {
    return encoder.encode(
      JSON.stringify([
        data.format,
        data.version,
        data.createdAt,
        data.cipher,
        data.kdf.name,
        data.kdf.hash,
        data.kdf.iterations,
        data.kdf.salt,
        data.iv,
      ]),
    );
  }
  async function encrypt(data, password) {
    validate(data);
    checkPassword(password);
    if (!globalThis.crypto?.subtle)
      throw new Error("当前浏览器不支持安全加密，不会退回明文备份。");
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const envelope = {
      format: ENCRYPTED_FORMAT,
      version: 1,
      createdAt: data.createdAt,
      cipher: "AES-256-GCM",
      kdf: {
        name: "PBKDF2",
        hash: "SHA-256",
        iterations: ITERATIONS,
        salt: base64(salt),
      },
      iv: base64(iv),
    };
    const plaintext = encoder.encode(JSON.stringify(data));
    try {
      const key = await deriveKey(password, salt, "encrypt");
      const ciphertext = await crypto.subtle.encrypt(
        {
          name: "AES-GCM",
          iv,
          additionalData: authenticatedHeader(envelope),
          tagLength: 128,
        },
        key,
        plaintext,
      );
      return validateEnvelope({
        ...envelope,
        ciphertext: base64(new Uint8Array(ciphertext)),
      });
    } finally {
      plaintext.fill(0);
    }
  }
  async function decrypt(envelope, password) {
    validateEnvelope(envelope);
    if (
      typeof password !== "string" ||
      !password.length ||
      password.length > 1024
    )
      throw new Error("请输入此备份的加密密码（最多 1024 个字符）。");
    const key = await deriveKey(
      password,
      fromBase64(envelope.kdf.salt, "salt", 16),
      "decrypt",
    );
    let plaintext;
    try {
      plaintext = new Uint8Array(
        await crypto.subtle.decrypt(
          {
            name: "AES-GCM",
            iv: fromBase64(envelope.iv, "iv", 12),
            additionalData: authenticatedHeader(envelope),
            tagLength: 128,
          },
          key,
          fromBase64(envelope.ciphertext, "ciphertext"),
        ),
      );
    } catch {
      throw new Error("密码错误或备份文件已损坏，未恢复任何数据。");
    }
    try {
      const data = validate(
        JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(plaintext)),
      );
      if (data.createdAt !== envelope.createdAt) invalid("加密头与内容不一致");
      return data;
    } finally {
      plaintext.fill(0);
    }
  }
  function endpointError(endpoint) {
    try {
      const url = new URL(endpoint);
      if (
        url.protocol !== "https:" ||
        url.username ||
        url.password ||
        url.search ||
        url.hash
      )
        return "Endpoint 必须是无凭据、查询参数和片段的 HTTPS 地址。";
    } catch {
      return "请填写有效的 HTTPS Endpoint。";
    }
    return "";
  }
  function s3Error(config) {
    const error = endpointError(config.endpoint);
    if (error) return error;
    if (!config.region.trim()) return "请填写 Region。";
    if (!/^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$/.test(config.bucket))
      return "Bucket 应为 3–63 位小写字母、数字、点或连字符。";
    if (
      config.prefix.startsWith("/") ||
      config.prefix.includes("\\") ||
      config.prefix.split("/").includes("..")
    )
      return "Prefix 应为相对对象前缀，不能包含上级目录。";
    return "";
  }
  return {
    MAX_BYTES,
    create,
    parse,
    validate,
    s3Error,
    encrypt,
    decrypt,
    checkPassword,
    validateEnvelope,
  };
})();
