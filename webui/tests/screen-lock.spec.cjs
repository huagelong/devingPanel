const { test, expect } = require("@playwright/test");
const PASSWORD = "test-screen-lock-2026!";

async function setup(page, minutes = "1") {
  await page.goto("/#/settings/security");
  await page.locator('[name="screen-new"]').fill(PASSWORD);
  await page.locator('[name="screen-confirm"]').fill(PASSWORD);
  await page.locator('[name="screen-interval"]').selectOption(minutes);
  await page.locator('[data-action="screen-setup"]').click();
  await expect(page.getByText("已启用", { exact: true })).toBeVisible();
}
async function unlock(page, password = PASSWORD) {
  await page.locator('[name="screen-unlock-password"]').fill(password);
  await page.locator('#screen-unlock-form button[type="submit"]').click();
}
test.beforeEach(async ({ page }) => {
  page.errors = [];
  page.on("pageerror", (e) => page.errors.push(e.message));
});
test.afterEach(async ({ page }) => expect(page.errors).toEqual([]));
test("setup stores only a salted verifier; manual lock and reload require a password", async ({
  page,
}) => {
  await setup(page);
  const config = await page.evaluate(() =>
    JSON.parse(localStorage.getItem("devingpanel-screen-lock")),
  );
  expect(config.version).toBe(1);
  expect(config.iterations).toBe(600000);
  expect(config.idleMinutes).toBe(1);
  expect(Buffer.from(config.salt, "base64")).toHaveLength(16);
  expect(Buffer.from(config.verifier, "base64")).toHaveLength(32);
  expect(JSON.stringify(config)).not.toContain(PASSWORD);
  await page.locator('[data-action="screen-lock"]').first().click();
  await expect(page.locator("#screen-lock")).toBeVisible();
  await expect(page.locator("#content")).toBeHidden();
  await expect(page).toHaveTitle("已锁定 · devingPanel");
  await page.keyboard.press("Escape");
  await expect(page.locator("#screen-lock")).toBeVisible();
  await unlock(page, "wrong-password");
  await expect(page.locator("#screen-unlock-error")).toHaveText(
    "密码错误，请重试。",
  );
  await expect(page.locator("#content")).toBeHidden();
  await unlock(page);
  await expect(page.locator("#screen-lock")).not.toBeVisible();
  await page.reload();
  await expect(page.locator("#screen-lock")).toBeVisible();
  await unlock(page);
  await expect(page.locator(".settings-content h2")).toHaveText("安全");
});
test("idle timeout is reset by activity and is configurable to custom minutes", async ({
  page,
}) => {
  await page.clock.install({ time: new Date("2026-10-25T04:00:00Z") });
  await page.clock.pauseAt(new Date("2026-10-25T04:00:01Z"));
  await setup(page);
  await page.clock.runFor(50000);
  await page.mouse.move(100, 200);
  await page.clock.runFor(59000);
  await expect(page.locator("#screen-lock")).not.toBeVisible();
  await page.clock.runFor(1000);
  await expect(page.locator("#screen-lock")).toBeVisible();
  await unlock(page);
  await page.locator('[name="screen-interval"]').selectOption("custom");
  await page.locator('[name="screen-custom"]').fill("3");
  await page.locator('[data-action="screen-save-interval"]').click();
  await expect(page.locator('[name="screen-current"]')).toBeVisible();
  await page.locator('[name="screen-current"]').fill("bad-password");
  await page.locator('[data-action="screen-confirm"]').click();
  await expect(page.locator("#screen-auth-error")).toContainText(
    "当前密码不正确",
  );
  expect(
    await page.evaluate(
      () =>
        JSON.parse(localStorage.getItem("devingpanel-screen-lock")).idleMinutes,
    ),
  ).toBe(1);
  await page.locator('[name="screen-current"]').fill(PASSWORD);
  await page.locator('[data-action="screen-confirm"]').click();
  await expect(page.locator("#dialog")).not.toBeVisible();
  await expect(page.locator("#screen-lock")).not.toBeVisible();
  expect(
    await page.evaluate(
      () =>
        JSON.parse(localStorage.getItem("devingpanel-screen-lock")).idleMinutes,
    ),
  ).toBe(3);
  await page.clock.runFor(179999);
  await expect(page.locator("#screen-lock")).not.toBeVisible();
  await page.clock.runFor(1);
  await expect(page.locator("#screen-lock")).toBeVisible();
});
test("password change and disable both require the current password", async ({
  page,
}) => {
  await setup(page);
  await page.locator('[data-action="screen-change-password"]').click();
  await page.locator('[name="screen-current"]').fill(PASSWORD);
  await page.locator('[name="screen-new"]').fill("different-screen-password");
  await page
    .locator('[name="screen-confirm"]')
    .fill("different-screen-password");
  await page.locator('[data-action="screen-confirm"]').click();
  await expect(page.locator("#dialog")).not.toBeVisible();
  await page.locator('[data-action="screen-lock"]').first().click();
  await unlock(page, PASSWORD);
  await expect(page.locator("#screen-lock")).toBeVisible();
  await unlock(page, "different-screen-password");
  await expect(page.locator("#screen-lock")).not.toBeVisible();
  await page.locator('[data-action="screen-disable"]').click();
  await page.locator('[name="screen-current"]').fill(PASSWORD);
  await page.locator('[data-action="screen-confirm"]').click();
  await expect(page.locator("#screen-auth-error")).toContainText(
    "当前密码不正确",
  );
  await page
    .locator('[name="screen-current"]')
    .fill("different-screen-password");
  await page.locator('[data-action="screen-confirm"]').click();
  await expect(page.getByText("未启用", { exact: true })).toBeVisible();
  expect(
    await page.evaluate(() => localStorage.getItem("devingpanel-screen-lock")),
  ).toBeNull();
  await page.reload();
  await expect(page.locator("#screen-lock")).not.toBeVisible();
});
test("lock clears unsaved secrets and the backup encryption password", async ({
  page,
}) => {
  await setup(page);
  await page.goto("/#/settings/backup");
  await page.locator('[data-action="backup-password"]').click();
  await page
    .locator('[name="backup-password"]')
    .fill("temporary-backup-password");
  await page
    .locator('[name="backup-password-confirm"]')
    .fill("temporary-backup-password");
  await page.locator('[data-action="backup-password-save"]').click();
  await page.getByRole("tab", { name: "S3 备份", exact: true }).click();
  await page.locator('[name="s3-access-key"]').fill("UNSAVED_DEMO_ID");
  await page.locator('[name="s3-secret-key"]').fill("UNSAVED_DEMO_SECRET");
  await page.locator('[data-action="screen-lock"]').first().click();
  await expect(page.locator("#content")).toBeHidden();
  await unlock(page);
  await expect(
    page.getByText("未设置，无法创建备份", { exact: true }),
  ).toBeVisible();
  await expect(page.locator('[name="s3-access-key"]')).toHaveValue("");
  await expect(page.locator('[name="s3-secret-key"]')).toHaveValue("");
  expect(await page.evaluate(() => JSON.stringify(localStorage))).not.toContain(
    "UNSAVED_DEMO",
  );
});
test("locking the UI does not terminate an already-running deployment", async ({
  page,
}) => {
  await page.clock.install({ time: new Date("2026-10-25T04:00:00Z") });
  await page.clock.pauseAt(new Date("2026-10-25T04:00:01Z"));
  await setup(page);
  await page.goto("/#/deploy");
  await page.locator('[data-action="deploy-start"]').click();
  await page.locator('[data-action="screen-lock"]').first().click();
  await expect(page.locator("#screen-lock")).toBeVisible();
  await page.clock.runFor(14000);
  await expect(page.locator("#screen-lock")).toBeVisible();
  expect(await page.evaluate(() => state.deploy.result)).toBe("成功");
  await unlock(page);
  await expect(page.locator("tbody .badge")).toHaveText(["成功", "成功"]);
});
test("lockscreen is bilingual and remains usable on mobile", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await setup(page);
  await page.locator('[data-action="screen-lock"]').first().click();
  await page.locator('[name="screen-language"]').selectOption("en");
  await expect(page.locator("#screen-lock h1")).toHaveText("Locked");
  await expect(page.locator("#screen-lock")).toContainText(
    "Enter the lock password to continue.",
  );
  expect(
    await page
      .locator("#screen-lock")
      .evaluate((el) => el.scrollWidth <= el.clientWidth),
  ).toBe(true);
  await page.screenshot({ path: "test-results/screen-lock-mobile-en.png" });
  await unlock(page, "wrong-password");
  await expect(page.locator("#screen-unlock-error")).toHaveText(
    "Incorrect password. Try again.",
  );
  await unlock(page);
  await expect(page.locator("html")).toHaveAttribute("lang", "en");
  await expect(page.locator("#screen-lock")).not.toBeVisible();
});
test("an enabled lock applies to another open tab and cancels stale actions", async ({
  page,
  context,
}) => {
  const other = await context.newPage();
  await other.goto("/#/dashboard");
  await setup(page);
  await expect(other.locator("#screen-lock")).toBeVisible();
  await expect(other.locator("#content")).toBeHidden();
  await unlock(other);
  await expect(other.locator("#screen-lock")).not.toBeVisible();
  await other.locator('[data-action="screen-lock"]').first().click();
  await expect(other.locator("#screen-lock")).toBeVisible();
  await other.goto("/#/settings/backup");
  await expect(other.locator("#screen-lock")).toBeVisible();
  await expect(other.locator("#content")).toBeHidden();
  await other.close();
});
test("corrupt configuration and missing Web Crypto fail closed", async ({
  page,
}) => {
  await page.addInitScript(() =>
    localStorage.setItem("devingpanel-screen-lock", "corrupt"),
  );
  await page.goto("/#/settings/security");
  await expect(page.locator("#screen-lock")).toContainText("锁屏配置损坏");
  await expect(page.locator("#screen-unlock-form")).toHaveCount(0);
  await expect(page.locator("#content")).toBeHidden();
  await page.reload();
  await expect(page.locator("#screen-lock")).toBeVisible();
});
test("setup never stores a verifier if cryptography or persistence is unavailable", async ({
  page,
}) => {
  await page.goto("/#/settings/security");
  await page.evaluate(() =>
    Object.defineProperty(globalThis, "crypto", {
      value: {},
      configurable: true,
    }),
  );
  await page.locator('[name="screen-new"]').fill(PASSWORD);
  await page.locator('[name="screen-confirm"]').fill(PASSWORD);
  await page.locator('[data-action="screen-setup"]').click();
  await expect(page.locator("#screen-settings-error")).toContainText(
    "无法安全验证密码",
  );
  expect(
    await page.evaluate(() => localStorage.getItem("devingpanel-screen-lock")),
  ).toBeNull();
  await page.reload();
  await page.evaluate(() => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (key, value) {
      if (key === "devingpanel-screen-lock")
        throw new DOMException("Storage unavailable", "SecurityError");
      return original.call(this, key, value);
    };
  });
  await page.locator('[name="screen-new"]').fill(PASSWORD);
  await page.locator('[name="screen-confirm"]').fill(PASSWORD);
  await page.locator('[data-action="screen-setup"]').click();
  await expect(page.locator("#screen-settings-error")).toContainText(
    "无法保存锁屏设置",
  );
  expect(
    await page.evaluate(() => localStorage.getItem("devingpanel-screen-lock")),
  ).toBeNull();
});
