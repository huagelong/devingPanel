"use strict";
const I18n = (() => {
  const supported = ["zh-CN", "en"];
  let locale = "zh-CN";
  try {
    const saved = localStorage.getItem("devingpanel-language");
    if (supported.includes(saved)) locale = saved;
  } catch {}
  const keys = Object.keys(I18N_EN).sort((a, b) => b.length - a.length);
  const pattern = new RegExp(
    keys.map((key) => key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"),
    "g",
  );
  // Translation is explicit at UI boundaries. Interpolated data is never scanned.
  function t(value) {
    if (value && typeof value === "object" && value.i18nParts) {
      const key = value.i18nParts.map((part, i) => part + (i < value.values.length ? `{${i}}` : "")).join("");
      if (locale === "en" && Object.hasOwn(I18N_EN, key))
        return I18N_EN[key].replace(/\{(\d+)\}/g, (_, index) => String(value.values[Number(index)]));
      return value.i18nParts
        .map(
          (part, i) =>
            t(part) + (i < value.values.length ? String(value.values[i]) : ""),
        )
        .join("");
    }
    const source = String(value ?? "");
    if (locale === "zh-CN") return source;
    if (Object.hasOwn(I18N_EN, source)) return I18N_EN[source];
    return source.replace(pattern, (key) => I18N_EN[key]);
  }
  function html(parts, ...values) {
    return parts
      .map((part, i) => t(part) + (i < values.length ? String(values[i]) : ""))
      .join("");
  }
  function msg(parts, ...values) {
    return {
      i18nParts: [...parts],
      values,
      toString() {
        return parts
          .map((part, i) => part + (i < values.length ? String(values[i]) : ""))
          .join("");
      },
    };
  }
  function setLocale(value) {
    if (!supported.includes(value)) return;
    locale = value;
    document.documentElement.lang = value;
    try {
      localStorage.setItem("devingpanel-language", value);
    } catch {}
  }
  document.documentElement.lang = locale;
  return {
    t,
    html,
    msg,
    setLocale,
    get locale() {
      return locale;
    },
  };
})();
