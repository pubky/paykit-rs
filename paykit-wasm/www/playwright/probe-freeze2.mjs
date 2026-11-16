// THROWAWAY (#411): hide a headless tab by opening a foreground tab in the
// same window, then try Page.setWebLifecycleState frozen.
import { chromium } from "playwright";
const browser = await chromium.launch({ headless: true, executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" });
const ctx = await browser.newContext();
const page = await ctx.newPage();
await page.goto("http://127.0.0.1:8410/");
const cdp = await ctx.newCDPSession(page);
await page.evaluate(() => { globalThis.ticks = 0; globalThis.events = []; setInterval(() => globalThis.ticks++, 100);
  for (const n of ["freeze", "resume", "visibilitychange", "pagehide"]) document.addEventListener(n, () => events.push(`${n}:${document.visibilityState}@${Math.round(performance.now())}`)); });
const bsession = await browser.newBrowserCDPSession();
const { targetInfos } = await bsession.send("Target.getTargets");
const me = targetInfos.find((t) => t.type === "page" && t.url.startsWith("http://127.0.0.1:8410"));
const created = await bsession.send("Target.createTarget", { url: "about:blank", newWindow: false, background: false, browserContextId: me.browserContextId }).catch((e) => ({ err: e.message }));
await new Promise((r) => setTimeout(r, 500));
const vis = await page.evaluate(() => document.visibilityState);
const t0 = await page.evaluate(() => globalThis.ticks);
const r1 = await cdp.send("Page.setWebLifecycleState", { state: "frozen" }).catch((e) => ({ err: e.message }));
await new Promise((r) => setTimeout(r, 3000));
const r2 = await cdp.send("Page.setWebLifecycleState", { state: "active" }).catch((e) => ({ err: e.message }));
const after = await page.evaluate(() => ({ ticks: globalThis.ticks, events: globalThis.events, vis: document.visibilityState }));
console.log(JSON.stringify({ created, visAfterNewTab: vis, ticksBefore: t0, r1, r2, after }));
await browser.close();
