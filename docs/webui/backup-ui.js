"use strict";
const BackupUI = (() => {
  const records = [];
  let pending = null;
  let readVersion = 0;
  let message = null;
  let testedTarget = "";
  let encryptionPassword = "";
  const dateLabel = (value) =>
    new Date(value).toLocaleString(I18n.locale, {
      timeZone: "Asia/Shanghai",
      hour12: false,
    });
  const sizeLabel = (data) =>
    `${(new Blob([JSON.stringify(data, null, 2)]).size / 1024).toFixed(1)} KB`;
  const snapshot = () => PanelBackup.create(hosts, projects, state);
  const targetKey = () => JSON.stringify(state.s3);
  const warning =
    "备份必须使用密码加密（AES-256-GCM）。不包含 SSH 密码/私钥、Registry/S3 凭据、环境变量值、构建参数值、镜像、卷、发布历史或回滚材料。加密密码不写入文件，忘记密码无法恢复。";
  function status(text, error = false) {
    message = { text, tone: error ? "destructive" : "info" };
    if (state.route === "settings") render();
    toast(text, error);
  }
  function summary(data) {
    return I18n.html`<div class="grid two backup-summary"><div class="notice"><h3>主机配置</h3><p class="stat">${data.hosts.length}</p></div><div class="notice"><h3>项目配置</h3><p class="stat">${data.projects.length}</p></div></div><p class="muted">格式 v${data.version} · ${sizeLabel(data)} · ${dateLabel(data.createdAt)}（北京时间）</p>`;
  }
  function renderBackup() {
    const tab = state.backupTab;
    const locked = deployLocked();
    const content =
      tab === "local"
        ? I18n.html`<div class="stack"><h3>本地配置备份</h3><p class="muted">导出当前主机、项目索引及非秘密设置为密码加密的 JSON；导入时预览并整体替换本次会话中的配置。外部 devingpanel.yaml 文件及多构建内容不在备份内，恢复后须重新读取项目文件。</p><div class="row wrap">${badge(I18n.msg`${hosts.length} 台主机`)}${badge(I18n.msg`${projects.length} 个项目`)}${badge("加密 JSON · AES-256-GCM")}</div><div class="actions">${button(icon("download") + "导出配置", "backup-export", "primary")}${button(icon("upload") + "选择文件导入", "backup-import", "", locked ? I18n.t('disabled title="发布未结束，暂不能导入"') : "")}</div><input id="backup-file" type="file" accept=".json,application/json" aria-label="选择本地备份 JSON 文件" hidden><small>最大 5 MB · 先输入密码解密校验，再预览确认；不接受明文文件。</small></div>`
        : I18n.html`<div class="stack"><div class="notice info">S3 页面为交互演示：不会连接云端或上传数据，备份记录仅保留在当前浏览器会话中，刷新后消失。请勿填写真实凭据。</div><form id="s3-form" class="stack"><div class="form-grid">${field("Endpoint", input("s3-endpoint", state.s3.endpoint, 'type="url" required placeholder="https://s3.amazonaws.com" class="mono"'))}${field("Region", input("s3-region", state.s3.region, 'required placeholder="us-east-1" class="mono"'))}${field("Bucket", input("s3-bucket", state.s3.bucket, 'required placeholder="my-devingpanel-backups" class="mono"'))}${field("Prefix（可选）", input("s3-prefix", state.s3.prefix, 'placeholder="devingpanel/" class="mono"'))}${field("Access Key ID", input("s3-access-key", "", 'autocomplete="off" placeholder="演示中无需真实凭据"'))}${field("Secret Access Key", input("s3-secret-key", "", 'type="password" autocomplete="off" placeholder="不保存、不导出"'))}</div><label class="row"><input name="s3-path-style" type="checkbox" ${state.s3.pathStyle ? "checked" : ""}>使用 Path-style 寻址（S3 兼容服务）</label><div class="actions">${button("测试连接（演示）", "backup-s3-test")}${button(icon("cloud") + "创建备份（演示）", "backup-s3-create", "primary", testedTarget === targetKey() ? "" : I18n.t('disabled title="请先完成演示连接测试"'))}</div><small>非秘密配置即改即存于当前会话。凭据仅作为输入展示，测试后立即清空。</small></form><div class="row between wrap"><h3>S3 备份记录</h3><small>本地模拟记录，不是云端对象列表</small></div>${
            records.length
              ? table(
                  [
                    "时间（北京时间）",
                    "Bucket / 对象路径",
                    "大小",
                    "状态",
                    "操作",
                  ],
                  records.map((record) =>
                    row([
                      dateLabel(record.data.createdAt),
                      I18n.html`<span class="mono backup-key">${esc(record.bucket)}<br>${esc(record.key)}</span>`,
                      sizeLabel(record.data),
                      badge("已加密 · 仅本地模拟", "warning"),
                      I18n.html`<div class="actions">${button("下载", `backup-s3-download:${record.id}`, "ghost")}${button("预览恢复", `backup-s3-restore:${record.id}`, "", locked ? I18n.t('disabled title="发布未结束，暂不能恢复"') : "")}${ib("trash", "删除演示记录", `backup-s3-delete:${record.id}`)}</div>`,
                    ]),
                  ),
                )
              : I18n.html`<div class="backup-empty">${icon("cloud")}<p>暂无备份记录</p><small>设置加密密码并完成连接演示后，可创建一份加密配置快照。</small></div>`
          }</div>`;
    return card(
      "备份与恢复",
      "备份客户端配置，不是 VPS 业务数据备份。",
      I18n.html`<div class="stack"><div class="notice"><div class="row between wrap"><div><h3>备份加密密码</h3><p>${encryptionPassword ? badge("已设置 · 仅本次会话", "success") : badge("未设置，无法创建备份", "warning")}</p></div><div class="actions">${button(encryptionPassword ? I18n.t("更改加密密码") : I18n.t("设置加密密码"), "backup-password")}${encryptionPassword ? button("清除密码", "backup-password-clear", "ghost") : ""}</div></div><p class="muted">密码不持久化，刷新后需重新设置。更改密码仅用于新备份，旧备份仍需旧密码；忘记密码无法找回。</p></div>${tabs(
        [
          ["local", "本地备份"],
          ["s3", "S3 备份"],
        ],
        tab,
        "backup-tab",
      )}${content}${locked ? I18n.t('<div class="notice warning">发布运行中、暂停或待核实时，禁止导入和恢复；仍可导出配置。</div>') : ""}<div class="notice muted">${I18n.t(warning)}</div>${message ? I18n.html`<div class="notice ${message.tone}" role="status">${esc(I18n.t(message.text))}</div>` : ""}</div>`,
    );
  }
  function download(data) {
    PanelBackup.validateEnvelope(data);
    const url = URL.createObjectURL(
      new Blob([JSON.stringify(data, null, 2) + "\n"], {
        type: "application/json",
      }),
    );
    const link = document.createElement("a");
    link.href = url;
    link.download = `devingpanel-encrypted-${data.createdAt.replace(/[:.]/g, "-")}.json`;
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  }
  function previewImport(data, source) {
    if (deployLocked()) throw new Error("发布尚未结束，不能导入或恢复配置。");
    PanelBackup.validate(data);
    pending = { kind: "import", data: structuredClone(data) };
    modal(
      "导入配置预览",
      I18n.html`<div class="stack"><p>来源：<span class="mono">${esc(source?.i18nParts ? I18n.t(source) : source)}</span></p>${summary(data)}<details><summary>查看将恢复的配置</summary>${table(["类型", "名称", "说明"], [...data.hosts.map((h) => row([I18n.t("主机"), esc(h.name), I18n.html`<span class="mono">${esc(h.address)}:${h.port}</span>`])), ...data.projects.map((p) => row([I18n.t("项目"), esc(p.name), I18n.msg`${p.hostIds.length} 台目标主机`]))])}</details><div class="notice warning">将整体替换当前 ${hosts.length} 台主机、${projects.length} 个项目及非秘密设置，并清空 ${history.length} 条当前演示发布历史和容器列表。不合并配置，不恢复任何服务器数据。</div><p class="muted">所有主机将重置为离线、未信任；项目标记待确认。需重新确认主机指纹、核对本地路径、补齐构建参数及凭据。${I18n.t(warning)}</p><label class="row"><input id="backup-overwrite" name="backup-overwrite" type="checkbox">我确认覆盖当前会话中的配置</label></div>`,
      button("取消", "close") +
        button("确认导入", "backup-import-confirm", "danger", "disabled"),
      true,
    );
  }
  function setPasswordDialog() {
    modal(
      "设置备份加密密码",
      I18n.html`<form id="backup-password-form" class="stack">${field("加密密码", input("backup-password", "", 'type="password" required minlength="8" maxlength="1024" autocomplete="new-password"'))}${field("再次输入密码", input("backup-password-confirm", "", 'type="password" required minlength="8" maxlength="1024" autocomplete="new-password"'))}<div class="notice warning">至少 8 个字符，建议使用较长且唯一的口令。仅保存在本次页面会话内，不写入配置或浏览器存储。忘记密码无法恢复备份。</div><p class="muted">更改不会重新加密已有备份，旧文件仍需原密码。</p><p id="backup-password-error" class="destructive" role="alert"></p></form>`,
      button("取消", "close") +
        button("设置密码", "backup-password-save", "primary"),
    );
  }
  function requestDecryption(envelope, source) {
    if (deployLocked()) throw new Error("发布尚未结束，不能导入或恢复配置。");
    PanelBackup.validateEnvelope(envelope);
    pending = { kind: "decrypt", envelope, source };
    modal(
      "解密备份",
      I18n.html`<form id="backup-decrypt-form" class="stack"><p>来源：<span class="mono">${esc(source?.i18nParts ? I18n.t(source) : source)}</span></p><p class="muted">AES-256-GCM · PBKDF2-SHA-256 · 输入创建此备份时设置的密码。解密校验通过后才显示配置预览，不会自动覆盖。</p>${field("备份密码", input("backup-decrypt-password", "", 'type="password" required maxlength="1024" autocomplete="off"'))}<p class="muted">忘记密码无法恢复；原型不保留此次输入的解密密码。</p><p id="backup-decrypt-error" class="destructive" role="alert"></p></form>`,
      button("取消", "close") +
        button("解密并预览", "backup-decrypt", "primary"),
    );
  }
  function applyImport(data) {
    PanelBackup.validate(data);
    const restoredHosts = data.hosts.map((h) => ({
      ...h,
      online: false,
      trustedAddress: "",
      trustedPort: 0,
      cpu: 0,
      memory: 0,
      disk: 0,
      containers: 0,
    }));
    const restoredProjects = data.projects.map((p) => ({
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
      args: p.buildArgKeys.map((key) => [key, ""]),
      env: [],
      ready: false,
      restoreNeedsReview: true,
    }));
    hosts.splice(0, hosts.length, ...restoredHosts);
    resetHostGroups();
    projects.splice(0, projects.length, ...restoredProjects);
    history.splice(0);
    containers.splice(0);
    Object.assign(state, structuredClone(data.settings), {
      hostCollectionCustom: ![10, 30, 60, 300].includes(data.settings.hostCollectionIntervalSeconds),
      registry: false,
      registryConfigured: !!data.settings.registryAddress,
      registryDraftAddress: data.settings.registryAddress || "",
      registryDraftUser: data.settings.registryUser || "",
      selectedProject: projects[0]?.id || 0,
      selectedHosts: [],
      deploy: null,
      parallel: 1,
      containerHost: hosts[0]?.id || 0,
      group: "all",
      historyPage: 0,
      imagesRemoved: [],
      view: "normal",
    });
    testedTarget = "";
    theme(data.settings.theme);
  }
  async function handleAction(action, arg) {
    try {
      if (action === "backup-password") {
        setPasswordDialog();
        return;
      }
      if (action === "backup-password-save") {
        const form = $("#backup-password-form");
        if (!form.reportValidity()) return;
        const password = $('[name="backup-password"]').value;
        if (password !== $('[name="backup-password-confirm"]').value) {
          $("#backup-password-error").textContent =
            I18n.t("两次输入的密码不一致。");
          return;
        }
        PanelBackup.checkPassword(password);
        encryptionPassword = password;
        $$("input", form).forEach((el) => (el.value = ""));
        close();
        status(
          "加密密码已设置，仅用于本次会话中新创建的备份。请妥善记住密码。",
        );
        return;
      }
      if (action === "backup-password-clear") {
        cancelRead();
        encryptionPassword = "";
        status("本次会话的加密密码已清除；现有备份仍需原密码解密。");
        return;
      }
      if (action === "backup-decrypt") {
        if (
          pending?.kind !== "decrypt" ||
          !$("#backup-decrypt-form").reportValidity()
        )
          return;
        if (deployLocked()) throw new Error("发布尚未结束，不能恢复配置。");
        const operation = pending,
          version = ++readVersion;
        const password = $('[name="backup-decrypt-password"]').value;
        $('[name="backup-decrypt-password"]').value = "";
        const submit = $('[data-action="backup-decrypt"]');
        submit.disabled = true;
        submit.textContent = I18n.t("解密校验中…");
        $("#backup-decrypt-error").textContent = I18n.t("");
        try {
          const data = await PanelBackup.decrypt(operation.envelope, password);
          if (version !== readVersion) return;
          previewImport(data, operation.source);
        } catch (error) {
          if (version === readVersion)
            $("#backup-decrypt-error").textContent = I18n.t(error.message);
        } finally {
          if (submit.isConnected) {
            submit.disabled = false;
            submit.textContent = I18n.t("解密并预览");
          }
        }
        return;
      }
      if (action === "backup-tab") {
        cancelRead();
        state.backupTab = arg;
        message = null;
        render();
        return;
      }
      if (action === "backup-export") {
        if (!encryptionPassword) {
          setPasswordDialog();
          return;
        }
        const data = snapshot();
        pending = { kind: "export", data };
        modal(
          "导出本地配置",
          I18n.html`<div class="stack">${summary(data)}<div class="notice warning">${I18n.t(warning)}</div><p class="muted">仅导出当前会话中的非秘密配置。环境变量值及所有构建参数值均不进入文件。</p></div>`,
          button("取消", "close") +
            button("确认导出", "backup-export-confirm", "primary"),
        );
        return;
      }
      if (action === "backup-export-confirm") {
        if (pending?.kind !== "export") return;
        const version = ++readVersion;
        const submit = $('[data-action="backup-export-confirm"]');
        submit.disabled = true;
        submit.textContent = I18n.t("正在加密…");
        try {
          const encrypted = await PanelBackup.encrypt(
            pending.data,
            encryptionPassword,
          );
          if (version !== readVersion) return;
          download(encrypted);
          close();
          status("加密备份已交给浏览器下载；恢复时需要此次加密密码。");
        } catch (error) {
          if (version === readVersion) status(error.message, true);
        } finally {
          if (submit.isConnected) {
            submit.disabled = false;
            submit.textContent = I18n.t("确认导出");
          }
        }
        return;
      }
      if (action === "backup-import") {
        if (deployLocked()) throw new Error("发布尚未结束，不能导入配置。");
        $("#backup-file").click();
        return;
      }
      if (action === "backup-import-confirm") {
        if (deployLocked()) throw new Error("发布尚未结束，不能导入配置。");
        if (pending?.kind !== "import" || !$("#backup-overwrite")?.checked)
          return;
        applyImport(pending.data);
        close();
        status(
          "配置已在当前演示会话中恢复。请重新验证主机并确认项目配置；真实 VPS 未发生变化。",
        );
        return;
      }
      if (action === "backup-s3-test") {
        const error = PanelBackup.s3Error(state.s3);
        $$("#s3-form [name$='-key']").forEach((input) => (input.value = ""));
        testedTarget = error ? "" : targetKey();
        if (error) throw new Error(error);
        status("模拟配置检查通过；未连接 S3、未验证访问权限，输入凭据已清空。");
        return;
      }
      if (action === "backup-s3-create") {
        if (testedTarget !== targetKey())
          throw new Error("请先完成当前 S3 配置的演示连接测试。");
        if (!encryptionPassword) {
          setPasswordDialog();
          return;
        }
        const plaintext = snapshot();
        const target = { ...state.s3 };
        const version = ++readVersion;
        const submit = $('[data-action="backup-s3-create"]');
        submit.disabled = true;
        let data;
        try {
          data = await PanelBackup.encrypt(plaintext, encryptionPassword);
        } catch (error) {
          if (version === readVersion) status(error.message, true);
          return;
        } finally {
          if (submit.isConnected && testedTarget === targetKey())
            submit.disabled = false;
        }
        if (version !== readVersion) return;
        const prefix = target.prefix.replace(/\/+$/, "");
        const id = crypto.randomUUID();
        records.unshift({
          id,
          bucket: target.bucket,
          endpoint: target.endpoint,
          key: `${prefix ? prefix + "/" : ""}${data.createdAt.replace(/[:.]/g, "-")}-${id.slice(0, 8)}.json`,
          data,
        });
        status(
          "已创建真实加密的本地演示快照，未上传 S3。刷新页面后演示记录将消失。",
        );
        return;
      }
      const record = records.find((r) => r.id === arg);
      if (!record) return;
      if (action === "backup-s3-download") {
        download(record.data);
        status("演示快照已交给浏览器下载。未访问 S3。");
      }
      if (action === "backup-s3-restore") {
        readVersion++;
        requestDecryption(
          record.data,
          I18n.msg`${record.endpoint} / ${record.bucket} / ${record.key}（本地模拟）`,
        );
      }
      if (action === "backup-s3-delete")
        confirmAction(
          "删除演示备份记录？",
          "仅移除当前浏览器内存中的记录，不会删除任何云端对象。",
          `backup-s3-delete-confirm:${arg}`,
          "删除演示记录",
        );
      if (action === "backup-s3-delete-confirm") {
        records.splice(records.indexOf(record), 1);
        close();
        status("演示备份记录已删除，云端未发生变化。");
      }
    } catch (error) {
      status(error.message, true);
    }
  }
  async function handleChange(el) {
    if (el.name === "backup-overwrite") {
      $('[data-action="backup-import-confirm"]').disabled = !el.checked;
      return;
    }
    const names = {
      "s3-endpoint": "endpoint",
      "s3-region": "region",
      "s3-bucket": "bucket",
      "s3-prefix": "prefix",
      "s3-path-style": "pathStyle",
    };
    if (Object.hasOwn(names, el.name)) {
      cancelRead();
      state.s3[names[el.name]] =
        el.type === "checkbox" ? el.checked : el.value.trim();
      testedTarget = "";
      $('[data-action="backup-s3-create"]').disabled = true;
      return;
    }
    if (el.id !== "backup-file") return;
    const file = el.files?.[0];
    const version = ++readVersion;
    if (!file) return;
    try {
      if (deployLocked()) throw new Error("发布尚未结束，不能导入配置。");
      if (file.size > PanelBackup.MAX_BYTES)
        throw new Error("备份文件不能超过 5 MB。");
      const contents = await file.text();
      if (version !== readVersion) return;
      requestDecryption(PanelBackup.parse(contents), file.name);
    } catch (error) {
      if (version === readVersion) status(error.message, true);
    } finally {
      el.value = "";
    }
  }
  function onClose() {
    pending = null;
    cancelRead();
  }
  function cancelRead() {
    readVersion++;
  }
  function clearSessionSecrets() {
    encryptionPassword = "";
    testedTarget = "";
    message = null;
    onClose();
  }
  return {
    render: renderBackup,
    handleAction,
    handleChange,
    onClose,
    cancelRead,
    clearSessionSecrets,
  };
})();
