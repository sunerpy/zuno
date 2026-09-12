import { test, expect } from "@playwright/test";
import { fixture } from "./fixture.mjs";
import { resolve } from "node:path";

let app;
test.beforeEach(async () => { app = await fixture(); });
test.afterEach(async () => { await app.close(); });
test.use({ ignoreHTTPSErrors: true });

test("session display is responsive, reasoning folded, approval explicit and external images inert", async ({ page }) => {
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  const external = [];
  page.on("request", (request) => { if (request.url().startsWith("https://external.invalid")) external.push(request.url()); });
  await page.goto(`${app.origin}/app/`);
  await page.getByRole("button", { name: "检查数据库迁移与恢复" }).click();
  await expect(page.getByText("请检查迁移逻辑，并列出验证步骤。", { exact: true })).toBeVisible();
  await expect(page.locator(".thinking").first()).not.toHaveAttribute("open");
  for (const width of [320, 375, 414, 768, 1280]) {
    await page.setViewportSize({ width, height: width < 700 ? 812 : 900 });
    await page.evaluate(() => document.fonts.ready);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth + 1);
    expect(overflow).toBe(false);
    await page.screenshot({ path: resolve(`../../target/enterprise-validation/web-${width}.png`) });
  }
  await page.getByRole("button", { name: "查看审批", exact: true }).click();
  await expect(page.getByRole("heading", { name: "操作审批" })).toBeVisible();
  await expect(page.getByRole("button", { name: "批准此操作" })).toBeVisible();
  expect(app.state.answers).toHaveLength(0);
  await page.getByRole("button", { name: "批准此操作" }).click();
  await expect(page.locator(".details-panel").getByText("已批准", { exact: true })).toBeVisible();
  expect(app.state.answers).toHaveLength(1);
  expect(external).toHaveLength(0); expect(errors).toEqual([]);
});

test("lost creation and submission responses retain idempotency keys", async ({ page }) => {
  app.state.losesCreate = true; app.state.losesSubmit = true;
  await page.goto(`${app.origin}/app/`);
  await page.getByLabel("任务内容").fill("验证响应丢失时不会产生重复任务");
  await page.getByRole("button", { name: "发送 ↑", exact: true }).click();
  await expect(page.getByRole("alert")).toBeVisible();
  await page.getByRole("button", { name: "发送 ↑", exact: true }).click();
  await expect(page.getByLabel("任务内容")).toHaveValue("");
  expect(app.state.createRequests.at(-1)).toBe(app.state.createRequests.at(-2));
  expect(app.state.submissions.filter((id) => id === app.state.submissions.at(-1))).toHaveLength(1);
});

test("account changes invalidate old content and mobile drawers contain keyboard focus", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  app.state.subject = "alice";
  await page.goto(`${app.origin}/app/`);
  await page.getByRole("button", { name: "打开导航" }).click();
  await expect(page.getByRole("dialog", { name: "工作区导航" })).toBeVisible();
  await expect(page.getByLabel("工作区", { exact: true })).toBeFocused();
  await page.keyboard.press("Shift+Tab");
  await expect(page.getByRole("button", { name: "退出登录" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.getByLabel("工作区", { exact: true })).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("button", { name: "打开导航" })).toBeFocused();
  await page.getByRole("button", { name: "打开导航" }).click();
  await page.getByRole("button", { name: "检查数据库迁移与恢复" }).click();
  await expect(page.getByText("请检查迁移逻辑，并列出验证步骤。", { exact: true })).toBeVisible();
  app.state.subject = "bob";
  await expect(page.getByText("请检查迁移逻辑，并列出验证步骤。", { exact: true })).not.toBeVisible({ timeout: 10000 });
  await expect(page.getByRole("heading", { name: "开始一项任务" })).toBeVisible();
  app.state.subject = "alice";
});

test("login, logout, dark mode and reduced motion work under the production CSP", async ({ page }) => {
  app.state.signedIn = false;
  await page.emulateMedia({ colorScheme: "dark", reducedMotion: "reduce" });
  await page.goto(`${app.origin}/app/`);
  await expect(page.getByRole("heading", { name: "进入你的工作区" })).toBeVisible();
  await expect(page.getByRole("link", { name: "开源许可" })).toHaveAttribute("href", "/app/assets/licenses.txt");
  await page.getByRole("button", { name: "使用企业账号登录" }).click();
  await expect(page.getByRole("heading", { name: "开始一项任务" })).toBeVisible();
  expect(await page.locator(".button--primary").first().evaluate((node) => getComputedStyle(node).transitionDuration)).toBe("0s");
  await page.getByRole("button", { name: "退出登录" }).click();
  await expect(page.getByRole("heading", { name: "进入你的工作区" })).toBeVisible();
});

test("an unresolved submission cannot be silently replaced with another request", async ({ page }) => {
  app.state.losesSubmit = true; app.state.lookupFailures = 1;
  await page.goto(`${app.origin}/app/`);
  await page.getByLabel("任务内容").fill("只接纳一次");
  await page.getByRole("button", { name: "发送 ↑", exact: true }).click();
  await expect(page.getByRole("button", { name: "重试发送" })).toBeVisible();
  await expect(page.getByLabel("任务内容")).toBeDisabled();
  await expect(page.getByRole("button", { name: "＋ 新建会话" })).toBeDisabled();
  await page.getByRole("button", { name: "重试发送" }).click();
  await expect(page.getByLabel("任务内容")).toHaveValue("");
  expect(new Set(app.state.submissions).size).toBe(1);
});

test("history paging, session paging and snapshot recovery do not mix sessions", async ({ page }) => {
  app.state.historyCount = 120;
  app.state.hiddenSessions = [{ id: "older-session", workspaceId: "workspace", title: "更早会话", createdAt: 1, updatedAt: 1 }];
  await page.goto(`${app.origin}/app/`);
  await page.getByRole("button", { name: "更多会话" }).click();
  await expect(page.getByRole("button", { name: "更早会话", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "检查数据库迁移与恢复" }).click();
  await expect(page.getByText("历史记录 120 — 检查保留的输入和执行结果。", { exact: true })).toBeVisible();
  await page.locator(".transcript-scroll").evaluate((node) => { node.scrollTop = 0; });
  await page.getByRole("button", { name: "加载更早记录" }).click();
  await expect(page.getByText("历史记录 81 — 检查保留的输入和执行结果。", { exact: true })).toBeAttached();
  await page.getByRole("button", { name: "返回最新记录 ↓" }).click();
  await expect(page.getByText("历史记录 81 — 检查保留的输入和执行结果。", { exact: true })).not.toBeAttached();
  const reads = app.state.historyReads;
  app.state.frameFailures = 1;
  await expect.poll(() => app.state.historyReads).toBeGreaterThan(reads);
  await expect(page.getByText("已连接", { exact: true })).toBeVisible();
  app.state.slowSession = 400;
  await page.getByRole("button", { name: "更早会话", exact: true }).click();
  await page.getByRole("button", { name: "＋ 新建会话" }).click();
  await page.waitForTimeout(500);
  await expect(page.getByRole("heading", { name: "开始一项任务" })).toBeVisible();
  await expect(page.getByText("历史记录 120 — 检查保留的输入和执行结果。", { exact: true })).not.toBeAttached();
});

test("late approval responses cannot reopen a closed panel or another session", async ({ page }) => {
  app.state.delayedAnswer = 500;
  await page.setViewportSize({ width: 768, height: 900 });
  await page.goto(`${app.origin}/app/`);
  await page.getByRole("button", { name: "检查数据库迁移与恢复" }).click();
  const open = page.getByRole("button", { name: "查看审批", exact: true });
  await open.click();
  await expect(page.getByRole("button", { name: "关闭详情" })).toBeFocused();
  await expect(page.getByRole("button", { name: "批准此操作" })).toBeVisible();
  await page.keyboard.press("Shift+Tab");
  await expect(page.getByRole("button", { name: "批准此操作" })).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.getByRole("button", { name: "关闭详情" })).toBeFocused();
  await page.getByRole("button", { name: "批准此操作" }).click();
  await page.keyboard.press("Escape");
  await expect(open).toBeFocused();
  await page.getByRole("button", { name: "＋ 新建会话" }).click();
  await expect.poll(() => app.state.approval).toBe("approved");
  await expect(page.getByRole("heading", { name: "开始一项任务" })).toBeVisible();
  await expect(page.getByRole("dialog", { name: "操作审批" })).not.toBeAttached();
});
