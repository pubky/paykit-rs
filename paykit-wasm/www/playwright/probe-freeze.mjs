// THROWAWAY (#411): does Page.setWebLifecycleState frozen stop timers?
import { chromium } from "playwright";
for (const variant of ["minimized", "minimized-no-background-flags"]) {
  const ignoreDefaultArgs = variant === "minimized" ? false : ["--disable-renderer-backgrounding", "--disable-background-timer-throttling", "--disable-backgrounding-occluded-windows"];
  const browser = await chromium.launch({ headless: true, executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", ignoreDefaultArgs });
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto("http://127.0.0.1:8410/");
  const cdp = await ctx.newCDPSession(page);
  await page.evaluate(() => { globalThis.ticks = 0; globalThis.events = []; setInterval(() => globalThis.ticks++, 100);
    document.addEventListener("freeze", () => events.push("freeze@" + Math.round(performance.now())));
    document.addEventListener("resume", () => events.push("resume@" + Math.round(performance.now())));
    document.addEventListener("visibilitychange", () => events.push(document.visibilityState + "@" + Math.round(performance.now()))); });
  const { windowId } = await cdp.send("Browser.getWindowForTarget");
  const min = await cdp.send("Browser.setWindowBounds", { windowId, bounds: { windowState: "minimized" } }).catch((e) => "err " + e.message);
  await new Promise((r) => setTimeout(r, 300));
  const visAfterMin = await page.evaluate(() => document.visibilityState);
  console.log("minimize", JSON.stringify(min), visAfterMin);
  const t0 = await page.evaluate(() => globalThis.ticks);
  const r1 = await cdp.send("Page.setWebLifecycleState", { state: "frozen" });
  await new Promise((r) => setTimeout(r, 3000));
  const r2 = await cdp.send("Page.setWebLifecycleState", { state: "active" });
  await cdp.send("Browser.setWindowBounds", { windowId, bounds: { windowState: "normal" } }).catch(() => {});
  const t1 = await page.evaluate(() => ({ ticks: globalThis.ticks, events: globalThis.events, vis: document.visibilityState }));
  console.log(variant, JSON.stringify({ before: t0, after: t1, r1, r2 }));
  await browser.close();
}
