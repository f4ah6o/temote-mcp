import test from "node:test";
import assert from "node:assert/strict";
import { chromium } from "playwright";
import { startFabricFixture } from "./fabric-app-fixture.mjs";

test("real MCP Apps SDK initializes in an iframe, uses initial data, and supports selection, refresh, theme and partial failures", async () => {
  const fixture = await startFabricFixture();
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (error) => errors.push(error.message));
    await page.goto(`${fixture.url}/host`);
    const app = page.frameLocator("iframe");
    await app.getByRole("button", { name: /host-a/ }).waitFor();
    assert.equal(await page.evaluate(() => window.fabricHarness.calls.length), 0);
    await app.getByRole("button", { name: /host-a/ }).click();
    await app.getByRole("button", { name: /session-one/ }).waitFor();
    await app.getByRole("button", { name: /session-one/ }).click();
    await app.getByText(/session-one-task/).waitFor();
    assert.equal(await page.evaluate(() => window.fabricHarness.calls.some((call) => !["fabric_overview", "fabric_session_list", "fabric_session_read"].includes(call.name))), false);
    await page.evaluate(() => { window.fabricHarness.partial = true; window.fabricHarness.theme(true); });
    await app.getByRole("button", { name: "Refresh", exact: true }).click();
    await app.getByText("Backend state unavailable.").waitFor();
    assert.equal(await app.locator("html").getAttribute("data-theme"), "dark");
    await app.getByText(/session-one-task/).waitFor();
    await page.evaluate(() => { window.fabricHarness.offline = true; });
    await app.getByRole("button", { name: "Refresh", exact: true }).click();
    await app.getByText("Saved state · host availability unconfirmed").waitFor();
    await page.evaluate(() => { window.fabricHarness.failed = true; });
    await app.getByRole("button", { name: "Refresh", exact: true }).click();
    await app.getByRole("alert").waitFor();
    await app.getByText(/session-one-task/).waitFor();
    await app.getByRole("button", { name: /host-a/ }).focus();
    await page.keyboard.press("Tab");
    assert.match(await app.locator(":focus").innerText(), /host-b/);
    for (const width of [375, 560, 1100]) {
      await page.setViewportSize({ width, height: 950 });
      const overflow = await app.locator("html").evaluate((node) => node.scrollWidth > node.clientWidth);
      assert.equal(overflow, false, `page overflows at ${width}px`);
    }
    assert.deepEqual(errors, []);
  } finally {
    try {
      await browser?.close();
    } finally {
      await fixture.close();
    }
  }
});

test("automatic refresh observes foreground and background intervals without an initial duplicate call", async () => {
  const fixture = await startFabricFixture();
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage();
    await page.clock.install();
    await page.goto(fixture.url);
    await page.getByRole("button", { name: /host-a/ }).waitFor();
    assert.equal(await page.evaluate(() => window.fabricHarness.calls.length), 0);
    await page.clock.runFor(5_100);
    assert.equal(await page.evaluate(() => window.fabricHarness.calls.length), 1);
    await page.evaluate(() => {
      Object.defineProperty(document, "hidden", { configurable: true, get: () => true });
      document.dispatchEvent(new Event("visibilitychange"));
    });
    await page.clock.runFor(29_900);
    assert.equal(await page.evaluate(() => window.fabricHarness.calls.length), 1);
    await page.clock.runFor(200);
    assert.equal(await page.evaluate(() => window.fabricHarness.calls.length), 2);
  } finally {
    try {
      await browser?.close();
    } finally {
      await fixture.close();
    }
  }
});
