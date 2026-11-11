const { test, expect } = require("@playwright/test");
const fs = require("node:fs/promises");
const crypto = require("node:crypto");
const PASSWORD = " 空格与中文-backup-2026! ";
async function setPassword(page, password = PASSWORD) {
  await page.locator('[data-action="backup-password"]').click();
  await page.locator('[name="backup-password"]').fill(password);
  await page.locator('[name="backup-password-confirm"]').fill(password);
  await page.locator('[data-action="backup-password-save"]').click();
}
async function exportFile(page) {
  await page.locator('[data-action="backup-export"]').click();
  const download = page.waitForEvent("download");
  await page.locator('[data-action="backup-export-confirm"]').click();
  return JSON.parse(await fs.readFile(await (await download).path(), "utf8"));
}
async function upload(page, envelope) {
  await page.locator("#backup-file").setInputFiles({
    name: "encrypted.json",
    mimeType: "application/json",
    buffer: Buffer.from(JSON.stringify(envelope)),
  });
}
async function unlock(page, password = PASSWORD) {
  await page.locator('[name="backup-decrypt-password"]').fill(password);
  await page.locator('[data-action="backup-decrypt"]').click();
}
function aad(envelope) {
  return Buffer.from(
    JSON.stringify([
      envelope.format,
      envelope.version,
      envelope.createdAt,
      envelope.cipher,
      envelope.kdf.name,
      envelope.kdf.hash,
      envelope.kdf.iterations,
      envelope.kdf.salt,
      envelope.iv,
    ]),
  );
}
function key(envelope) {
  return crypto.pbkdf2Sync(
    PASSWORD,
    Buffer.from(envelope.kdf.salt, "base64"),
    600000,
    32,
    "sha256",
  );
}
test.beforeEach(async ({ page }) => {
  page.errors = [];
  page.on("pageerror", (e) => page.errors.push(e.message));
  await page.goto("/#/settings/backup");
});
test.afterEach(async ({ page }) => expect(page.errors).toEqual([]));
test("password setup requires confirmation, is memory-only, and can be cleared", async ({
  page,
}) => {
  await page.locator('[data-action="backup-export"]').click();
  await expect(
    page.getByRole("heading", { name: "设置备份加密密码" }),
  ).toBeVisible();
  await page.locator('[name="backup-password"]').fill("short");
  await page.locator('[name="backup-password-confirm"]').fill("short");
  await page.locator('[data-action="backup-password-save"]').click();
  await expect(page.locator("#dialog")).toBeVisible();
  await page.locator('[name="backup-password"]').fill(PASSWORD);
  await page
    .locator('[name="backup-password-confirm"]')
    .fill("mismatched-password");
  await page.locator('[data-action="backup-password-save"]').click();
  await expect(page.locator("#backup-password-error")).toHaveText(
    "两次输入的密码不一致。",
  );
  await page.locator('[name="backup-password-confirm"]').fill(PASSWORD);
  await page.locator('[data-action="backup-password-save"]').click();
  await expect(
    page.getByText("已设置 · 仅本次会话", { exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(() =>
      JSON.stringify([localStorage, sessionStorage, state]),
    ),
  ).not.toContain(PASSWORD);
  await page.locator('nav a[href="#/dashboard"]').click();
  await page.locator('nav a[href="#/settings"]').click();
  await page.locator('.settings-nav a[href="#/settings/backup"]').click();
  await expect(
    page.getByText("已设置 · 仅本次会话", { exact: true }),
  ).toBeVisible();
  await page.locator('[data-action="backup-password-clear"]').click();
  await expect(
    page.getByText("未设置，无法创建备份", { exact: true }),
  ).toBeVisible();
  await setPassword(page);
  await page.reload();
  await expect(
    page.getByText("未设置，无法创建备份", { exact: true }),
  ).toBeVisible();
});
test("download is standard authenticated AES-256-GCM, with unique salt and nonce", async ({
  page,
}) => {
  await setPassword(page);
  const first = await exportFile(page),
    second = await exportFile(page);
  expect(first.kdf.iterations).toBe(600000);
  expect(first.kdf.hash).toBe("SHA-256");
  expect(Buffer.from(first.kdf.salt, "base64")).toHaveLength(16);
  expect(Buffer.from(first.iv, "base64")).toHaveLength(12);
  expect(first.iv).not.toBe(second.iv);
  expect(first.kdf.salt).not.toBe(second.kdf.salt);
  expect(first.ciphertext).not.toBe(second.ciphertext);
  const file = JSON.stringify(first);
  for (const secret of [PASSWORD, "203.0.113.10", "myapp", "NODE_ENV"])
    expect(file).not.toContain(secret);
  const ciphertext = Buffer.from(first.ciphertext, "base64");
  const decipher = crypto.createDecipheriv(
    "aes-256-gcm",
    key(first),
    Buffer.from(first.iv, "base64"),
  );
  decipher.setAAD(aad(first));
  decipher.setAuthTag(ciphertext.subarray(-16));
  const data = JSON.parse(
    Buffer.concat([
      decipher.update(ciphertext.subarray(0, -16)),
      decipher.final(),
    ]).toString("utf8"),
  );
  expect(data.hosts).toHaveLength(3);
  expect(data.settings).not.toHaveProperty("password");
  await page.reload();
  await upload(page, first);
  await expect(page.getByRole("heading", { name: "解密备份" })).toBeVisible();
  await expect(page.getByText("导入配置预览", { exact: true })).toHaveCount(0);
  await unlock(page);
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
});
test("wrong passwords and tampered ciphertext or metadata never reach restore preview", async ({
  page,
}) => {
  await setPassword(page);
  const envelope = await exportFile(page);
  await upload(page, envelope);
  await unlock(page, "wrong-password");
  await expect(page.locator("#backup-decrypt-error")).toContainText(
    "密码错误或备份文件已损坏",
  );
  expect(await page.evaluate(() => history.length)).toBe(3);
  await unlock(page);
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
  await page
    .locator("#dialog")
    .getByRole("button", { name: "取消", exact: true })
    .click();
  for (const part of ["ciphertext", "createdAt", "iv"]) {
    const damaged = structuredClone(envelope);
    if (part === "createdAt")
      damaged.createdAt = new Date(
        Date.parse(envelope.createdAt) + 1000,
      ).toISOString();
    else
      damaged[part] =
        (damaged[part][0] === "A" ? "B" : "A") + damaged[part].slice(1);
    await upload(page, damaged);
    await unlock(page);
    await expect(page.locator("#backup-decrypt-error")).toContainText(
      "密码错误或备份文件已损坏",
    );
    expect(await page.evaluate(() => hosts[0].name)).toBe("web-1");
    await page
      .locator("#dialog")
      .getByRole("button", { name: "取消", exact: true })
      .click();
  }
});
test("changed passwords only affect new backups; old files still require the old password", async ({
  page,
}) => {
  await setPassword(page);
  const old = await exportFile(page);
  await setPassword(page, "different-password-2026");
  const newer = await exportFile(page);
  await upload(page, old);
  await unlock(page, "different-password-2026");
  await expect(page.locator("#backup-decrypt-error")).toContainText(
    "密码错误或备份文件已损坏",
  );
  await unlock(page);
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
  await page
    .locator("#dialog")
    .getByRole("button", { name: "取消", exact: true })
    .click();
  await upload(page, newer);
  await unlock(page, "different-password-2026");
  await expect(
    page.getByRole("heading", { name: "导入配置预览" }),
  ).toBeVisible();
});
test("unsupported algorithms, excessive KDF work and plaintext files are rejected before decryption", async ({
  page,
}) => {
  await setPassword(page);
  const envelope = await exportFile(page);
  const plaintext = await page.evaluate(() =>
    PanelBackup.create(hosts, projects, state),
  );
  for (const data of [
    plaintext,
    { ...envelope, version: 2 },
    { ...envelope, cipher: "AES-128-CBC" },
    { ...envelope, kdf: { ...envelope.kdf, iterations: 999999999 } },
    { ...envelope, password: PASSWORD },
  ]) {
    await upload(page, data);
    await expect(page.locator('main [role="status"]')).toContainText(
      /格式错误|旧版明文/,
    );
    await expect(page.locator("#dialog")).not.toBeVisible();
    expect(await page.evaluate(() => history.length)).toBe(3);
  }
});
test("authenticated but invalid inner data is also rejected atomically", async ({
  page,
}) => {
  await setPassword(page);
  const envelope = await exportFile(page);
  const data = await page.evaluate(() =>
    PanelBackup.create(hosts, projects, state),
  );
  data.hosts.push(data.hosts[0]);
  const cipher = crypto.createCipheriv(
    "aes-256-gcm",
    key(envelope),
    Buffer.from(envelope.iv, "base64"),
  );
  cipher.setAAD(aad(envelope));
  envelope.ciphertext = Buffer.concat([
    cipher.update(JSON.stringify(data), "utf8"),
    cipher.final(),
    cipher.getAuthTag(),
  ]).toString("base64");
  await upload(page, envelope);
  await unlock(page);
  await expect(page.locator("#backup-decrypt-error")).toContainText(
    "主机 ID 重复",
  );
  expect(await page.evaluate(() => hosts.length)).toBe(3);
});
test("no Web Crypto means no plaintext fallback or download", async ({
  page,
}) => {
  await setPassword(page);
  const downloads = [];
  page.on("download", (d) => downloads.push(d));
  await page.evaluate(() =>
    Object.defineProperty(globalThis, "crypto", {
      value: {},
      configurable: true,
    }),
  );
  await page.locator('[data-action="backup-export"]').click();
  await page.locator('[data-action="backup-export-confirm"]').click();
  await expect(page.locator('main [role="status"]')).toContainText(
    "不会退回明文备份",
  );
  expect(downloads).toHaveLength(0);
});
test("cancelled decryption cannot overwrite a later form", async ({ page }) => {
  await setPassword(page);
  const envelope = await exportFile(page);
  await page.evaluate(() => {
    const original = PanelBackup.decrypt;
    PanelBackup.decrypt = (...args) =>
      new Promise((resolve) => {
        window.finishDecrypt = async () => resolve(await original(...args));
      });
  });
  await upload(page, envelope);
  await unlock(page);
  await page
    .locator("#dialog")
    .getByRole("button", { name: "取消", exact: true })
    .click();
  await page.locator('nav a[href="#/projects"]').click();
  await page.locator('[data-action="project-new"]').click();
  await page.locator('[name="name"]').fill("unsaved");
  await page.evaluate(() => window.finishDecrypt());
  await expect(
    page.getByRole("heading", { name: "新建项目", exact: true }),
  ).toBeVisible();
  await expect(page.locator('[name="name"]')).toHaveValue("unsaved");
});
