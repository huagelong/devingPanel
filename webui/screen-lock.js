"use strict";
const ScreenLock = (() => {
  const KEY = "devingpanel-screen-lock";
  const ITERATIONS = 600000;
  const choices = [1, 5, 15, 30, 60];
  let config = readConfig();
  let locked = !!config;
  let lastActivity = Date.now();
  let timer;
  let pending = null;
  let operation = 0;
  if (locked) document.documentElement.dataset.screenLocked = "true";

  function readConfig() {
    let raw;
    try {
      raw = localStorage.getItem(KEY);
    } catch {
      return null;
    }
    if (raw === null) return null;
    try {
      const value = JSON.parse(raw);
      if (
        Object.keys(value).sort().join(",") !==
          "idleMinutes,iterations,salt,verifier,version" ||
        value.version !== 1 ||
        value.iterations !== ITERATIONS ||
        !Number.isInteger(value.idleMinutes) ||
        value.idleMinutes < 1 ||
        value.idleMinutes > 1440 ||
        typeof value.salt !== "string" ||
        atob(value.salt).length !== 16 ||
        typeof value.verifier !== "string" ||
        atob(value.verifier).length !== 32
      )
        throw Error("invalid");
      return value;
    } catch {
      return { corrupt: true };
    }
  }
  function encode(bytes) {
    return btoa(String.fromCharCode(...bytes));
  }
  function persist(next) {
    try {
      if (next) localStorage.setItem(KEY, JSON.stringify(next));
      else localStorage.removeItem(KEY);
    } catch {
      throw Error("无法保存锁屏设置；请允许本机浏览器存储。");
    }
  }
  async function derive(password, salt) {
    if (!globalThis.crypto?.subtle)
      throw Error("当前浏览器无法安全验证密码；请使用 HTTPS 或 localhost。");
    const raw = new TextEncoder().encode(password);
    try {
      const key = await crypto.subtle.importKey("raw", raw, "PBKDF2", false, [
        "deriveBits",
      ]);
      return new Uint8Array(
        await crypto.subtle.deriveBits(
          { name: "PBKDF2", hash: "SHA-256", salt, iterations: ITERATIONS },
          key,
          256,
        ),
      );
    } finally {
      raw.fill(0);
    }
  }
  async function matches(password, saved) {
    const actual = await derive(
      password,
      Uint8Array.from(atob(saved.salt), (c) => c.charCodeAt(0)),
    );
    const expected = Uint8Array.from(atob(saved.verifier), (c) =>
      c.charCodeAt(0),
    );
    let difference = 0;
    for (let i = 0; i < expected.length; i++)
      difference |= actual[i] ^ expected[i];
    actual.fill(0);
    return difference === 0;
  }
  function checkPassword(password, confirmation) {
    if (password.length < 8 || password.length > 1024)
      throw Error("锁屏密码须为 8–1024 个字符。");
    if (password !== confirmation) throw Error("两次输入的密码不一致。");
  }
  function interval(form) {
    const selection = $('[name="screen-interval"]', form).value;
    const minutes =
      selection === "custom"
        ? Number($('[name="screen-custom"]', form).value)
        : Number(selection);
    if (!Number.isInteger(minutes) || minutes < 1 || minutes > 1440)
      throw Error("无操作间隔必须是 1–1440 分钟的整数。");
    return minutes;
  }
  function intervalFields(value) {
    const custom = !choices.includes(value);
    return `<div class="form-grid">${field("无操作自动锁屏间隔", select("screen-interval", [...choices.map((n) => [n, I18n.t(`${n} 分钟`)]), ["custom", I18n.t("自定义")]], custom ? "custom" : String(value)))}<div class="screen-custom-field" ${custom ? "" : "hidden"}>${field("自定义分钟数（1–1440）", input("screen-custom", value, `type="number" min="1" max="1440" step="1" ${custom ? "required" : "disabled"}`))}</div></div>`;
  }
  function settings() {
    const state = !config
      ? badge("未启用", "muted")
      : config.corrupt
        ? badge("配置损坏", "destructive")
        : badge("已启用", "success");
    if (config?.corrupt)
      return `<div class="notice destructive">${I18n.t("锁屏配置损坏，无法校验密码；请在可信环境中检查本机浏览器存储。不会自动解除锁定。")}</div>`;
    return `<div class="stack"><div class="row between wrap"><h3>${I18n.t("本机锁屏")}</h3>${state}</div><p class="muted">${I18n.t("无操作达到设定时间后自动锁定；刷新或重新打开应用也需解锁。这只是本机界面锁，不替代操作系统登录、磁盘加密或服务器认证。")}</p>${!config ? `<form id="screen-setup-form" class="stack">${field("新锁屏密码", input("screen-new", "", 'type="password" minlength="8" maxlength="1024" required autocomplete="new-password"'))}${field("确认新密码", input("screen-confirm", "", 'type="password" minlength="8" maxlength="1024" required autocomplete="new-password"'))}${intervalFields(5)}${button("启用锁屏", "screen-setup", "primary")}</form>` : `<div class="stack">${badge(I18n.msg`${config.idleMinutes} 分钟无操作后锁屏`, "info")}${intervalFields(config.idleMinutes)}<div class="actions">${button("保存间隔", "screen-save-interval")}${button("修改锁屏密码", "screen-change-password")}${button("立即锁屏", "screen-lock", "primary")}${button("关闭锁屏", "screen-disable", "danger")}</div></div>`}<p class="muted">${I18n.t("密码不以明文存储；丢失密码无法通过此原型找回。请妥善保存。锁屏时会清除尚未提交的敏感输入和当前会话的备份加密密码。")}</p><div id="screen-settings-error" class="destructive" role="alert"></div></div>`;
  }
  function error(text, root = "#screen-settings-error") {
    const el = $(root);
    if (el) el.textContent = I18n.t(text);
    else if (!locked) toast(text, true);
  }
  function schedule() {
    clearTimeout(timer);
    if (!config || config.corrupt || locked) return;
    timer = setTimeout(
      checkIdle,
      Math.max(0, lastActivity + config.idleMinutes * 60000 - Date.now()),
    );
  }
  function checkIdle() {
    if (!config || config.corrupt || locked) return;
    if (Date.now() - lastActivity >= config.idleMinutes * 60000) lock();
    else schedule();
  }
  function activity(event) {
    if (!config || locked) return;
    if (event.type === "pointermove" && Date.now() - lastActivity < 1000)
      return;
    checkIdle();
    if (!locked) {
      lastActivity = Date.now();
      schedule();
    }
  }
  function onDialogDismiss() {
    pending = null;
    operation++;
  }
  function lock() {
    if (!config || locked) return;
    clearTimeout(timer);
    onDialogDismiss();
    close();
    BackupUI.clearSessionSecrets();
    $$(
      'input[type="password"],input[name="s3-access-key"],input[name="value"],textarea[name="credential"]',
    ).forEach((el) => (el.value = ""));
    $("#toasts").replaceChildren();
    locked = true;
    document.documentElement.dataset.screenLocked = "true";
    show();
  }
  function show() {
    const dialog = $("#screen-lock");
    if (!locked) return;
    dialog.innerHTML = `<div class="lock-content"><div class="lock-panel stack"><div class="row">${icon("lock")}<strong>devingPanel</strong></div><div><h1 id="screen-lock-title">${I18n.t("已锁定")}</h1><p class="muted">${I18n.t("输入锁屏密码以继续使用。")}</p></div>${config.corrupt ? `<div class="notice destructive">${I18n.t("锁屏配置损坏，无法校验密码；请在可信环境中检查本机浏览器存储。不会自动解除锁定。")}</div>` : `<form id="screen-unlock-form" class="stack">${field("锁屏密码", input("screen-unlock-password", "", 'type="password" required autofocus autocomplete="off"'))}<p id="screen-unlock-error" class="destructive" role="alert"></p><button type="submit" class="primary">${I18n.t("解锁")}</button></form>`}<label class="field">${I18n.t("语言")}${select(
      "screen-language",
      [
        ["zh-CN", "简体中文"],
        ["en", "English"],
      ],
      I18n.locale,
    )}</label><small class="muted">${I18n.t("本机界面锁不替代系统账户、磁盘加密或服务器权限。")}</small></div></div>`;
    if (!dialog.open) dialog.showModal();
    document.title = `${I18n.t("已锁定")} · devingPanel`;
    $("#screen-unlock-form")?.addEventListener("submit", (event) => {
      event.preventDefault();
      unlock();
    });
    $('[name="screen-unlock-password"]')?.focus();
  }
  async function unlock() {
    if (!locked || config.corrupt) return;
    const form = $("#screen-unlock-form");
    if (!form?.reportValidity()) return;
    const password = $('[name="screen-unlock-password"]', form).value;
    $('[name="screen-unlock-password"]', form).value = "";
    const submit = form.querySelector('button[type="submit"]');
    submit.disabled = true;
    submit.textContent = I18n.t("正在验证…");
    const current = config;
    try {
      const ok = await matches(password, current);
      if (!locked || config !== current || !form.isConnected) return;
      if (!ok) {
        error("密码错误，请重试。", "#screen-unlock-error");
        return;
      }
      locked = false;
      const dialog = $("#screen-lock");
      dialog.close();
      dialog.innerHTML = "";
      delete document.documentElement.dataset.screenLocked;
      lastActivity = Date.now();
      schedule();
      render();
    } catch (failure) {
      if (form.isConnected) error(failure.message, "#screen-unlock-error");
    } finally {
      if (submit.isConnected) {
        submit.disabled = false;
        submit.textContent = I18n.t("解锁");
      }
    }
  }
  async function setup() {
    if (config || locked) return;
    const form = $("#screen-setup-form");
    if (!form?.reportValidity()) return;
    const password = $('[name="screen-new"]', form).value;
    const confirmation = $('[name="screen-confirm"]', form).value;
    let minutes;
    try {
      checkPassword(password, confirmation);
      minutes = interval(form);
    } catch (failure) {
      error(failure.message);
      return;
    }
    $('[name="screen-new"]', form).value = "";
    $('[name="screen-confirm"]', form).value = "";
    const token = ++operation;
    try {
      if (!globalThis.crypto?.subtle || !globalThis.crypto?.getRandomValues)
        throw Error("当前浏览器无法安全验证密码；请使用 HTTPS 或 localhost。");
      const salt = crypto.getRandomValues(new Uint8Array(16));
      const verifier = encode(await derive(password, salt));
      if (token !== operation || config || locked || !form.isConnected) return;
      const next = {
        version: 1,
        iterations: ITERATIONS,
        idleMinutes: minutes,
        salt: encode(salt),
        verifier,
      };
      persist(next);
      config = next;
      lastActivity = Date.now();
      schedule();
      render();
      toast("锁屏已启用。", false);
    } catch (failure) {
      if (form.isConnected) error(failure.message);
    }
  }
  function reauthenticate(kind) {
    if (!config || locked || config.corrupt) return;
    const current = config;
    let minutes = current.idleMinutes;
    if (kind === "interval") {
      try {
        minutes = interval($(".settings-content"));
      } catch (failure) {
        error(failure.message);
        return;
      }
    }
    pending = { kind, current, minutes };
    modal(
      kind === "interval"
        ? "确认修改间隔"
        : kind === "change"
          ? "修改锁屏密码"
          : "关闭锁屏？",
      `<form id="screen-auth-form" class="stack">${field("当前锁屏密码", input("screen-current", "", 'type="password" required autocomplete="off"'))}${kind === "change" ? field("新锁屏密码", input("screen-new", "", 'type="password" minlength="8" maxlength="1024" required autocomplete="new-password"')) + field("确认新密码", input("screen-confirm", "", 'type="password" minlength="8" maxlength="1024" required autocomplete="new-password"')) : ""}${kind === "interval" ? `<p class="muted">${I18n.t("新间隔")}: ${minutes} ${I18n.t("分钟")}</p>` : ""}${kind === "disable" ? `<div class="notice warning">${I18n.t("关闭后不会再自动锁屏；请确认这是您的设备。")}</div>` : ""}<p id="screen-auth-error" class="destructive" role="alert"></p></form>`,
      button("取消", "close") +
        button(
          kind === "interval"
            ? "保存间隔"
            : kind === "change"
              ? "修改密码"
              : "确认关闭",
          "screen-confirm",
          kind === "disable" ? "danger" : "primary",
        ),
    );
  }
  async function confirm() {
    const context = pending,
      form = $("#screen-auth-form");
    if (
      !context ||
      !form?.reportValidity() ||
      locked ||
      config !== context.current
    )
      return;
    const password = $('[name="screen-current"]', form).value;
    $('[name="screen-current"]', form).value = "";
    let nextPassword = "",
      confirmation = "";
    if (context.kind === "change") {
      nextPassword = $('[name="screen-new"]', form).value;
      confirmation = $('[name="screen-confirm"]', form).value;
      try {
        checkPassword(nextPassword, confirmation);
      } catch (failure) {
        error(failure.message, "#screen-auth-error");
        return;
      }
      $('[name="screen-new"]', form).value = "";
      $('[name="screen-confirm"]', form).value = "";
    }
    const token = ++operation;
    const submit = $('[data-action="screen-confirm"]', form.closest("dialog"));
    submit.disabled = true;
    try {
      const valid = await matches(password, context.current);
      if (
        token !== operation ||
        pending !== context ||
        locked ||
        config !== context.current ||
        !form.closest("dialog")?.open
      )
        return;
      if (!valid) {
        error("当前密码不正确。", "#screen-auth-error");
        return;
      }
      if (context.kind === "disable") {
        persist(null);
        config = null;
        clearTimeout(timer);
      } else if (context.kind === "interval") {
        const next = { ...config, idleMinutes: context.minutes };
        persist(next);
        config = next;
        lastActivity = Date.now();
        schedule();
      } else {
        const salt = crypto.getRandomValues(new Uint8Array(16));
        const verifier = encode(await derive(nextPassword, salt));
        if (
          token !== operation ||
          pending !== context ||
          locked ||
          config !== context.current ||
          !form.closest("dialog")?.open
        )
          return;
        const next = {
          version: 1,
          iterations: ITERATIONS,
          idleMinutes: config.idleMinutes,
          salt: encode(salt),
          verifier,
        };
        persist(next);
        config = next;
        lastActivity = Date.now();
        schedule();
      }
      close();
      render();
      toast(
        context.kind === "disable"
          ? "锁屏已关闭。"
          : context.kind === "interval"
            ? "自动锁屏间隔已更新。"
            : "锁屏密码已修改。",
      );
    } catch (failure) {
      if (form.isConnected) error(failure.message, "#screen-auth-error");
    } finally {
      if (submit.isConnected) submit.disabled = false;
    }
  }
  function onIntervalChange(el) {
    if (el.name !== "screen-interval") return;
    const wrapper = el.closest("form") || el.closest(".settings-content");
    const custom = $(".screen-custom-field", wrapper);
    custom.hidden = el.value !== "custom";
    $("[name=screen-custom]", custom).disabled = !custom.hidden ? false : true;
  }
  function init() {
    for (const name of [
      "pointerdown",
      "pointermove",
      "keydown",
      "input",
      "wheel",
      "touchstart",
    ])
      document.addEventListener(name, activity, {
        capture: true,
        passive: true,
      });
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) checkIdle();
    });
    window.addEventListener("focus", checkIdle);
    window.addEventListener("storage", (event) => {
      if (event.key !== KEY && event.key !== null) return;
      const next = readConfig();
      if (JSON.stringify(next) === JSON.stringify(config)) return;
      onDialogDismiss();
      clearTimeout(timer);
      config = next;
      if (config) {
        if (!locked) lock();
        else show();
      } else if (locked) {
        locked = false;
        const dialog = $("#screen-lock");
        dialog.querySelectorAll("input").forEach((el) => (el.value = ""));
        dialog.close();
        dialog.innerHTML = "";
        delete document.documentElement.dataset.screenLocked;
        render();
      } else render();
    });
    document.addEventListener(
      "click",
      (event) => {
        if (locked && !event.target.closest("#screen-lock")) {
          event.preventDefault();
          event.stopImmediatePropagation();
        }
      },
      true,
    );
    $("#screen-lock").addEventListener("cancel", (event) =>
      event.preventDefault(),
    );
    if (locked) show();
    else schedule();
  }
  function setLanguage(value) {
    if (!locked) return;
    I18n.setLocale(value);
    render();
    show();
  }
  return {
    init,
    settings,
    onIntervalChange,
    onDialogDismiss,
    setLanguage,
    handleAction(action) {
      if (action === "screen-setup") return setup();
      if (action === "screen-save-interval") return reauthenticate("interval");
      if (action === "screen-change-password") return reauthenticate("change");
      if (action === "screen-disable") return reauthenticate("disable");
      if (action === "screen-confirm") return confirm();
      if (action === "screen-lock") return lock();
    },
    isEnabled: () => !!config,
    get isLocked() {
      return locked;
    },
  };
})();
