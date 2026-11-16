// THROWAWAY (#411): when does fetch() resolve for an SSE response whose
// headers arrive at once but whose first body bytes come 15 s later?
import { chromium, firefox, webkit } from "playwright";
const [,, engine, user] = process.argv;
const types = { chromium, firefox, webkit };
const opts = engine === "chromium" ? { executablePath: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" } : {};
const browser = await types[engine].launch({ headless: true, ...opts });
const page = await browser.newPage();
await page.goto("http://127.0.0.1:8410/");
const r = await page.evaluate(async (user) => {
  const t0 = performance.now();
  const ms = () => Math.round(performance.now() - t0);
  const ctrl = new AbortController();
  const res = await fetch(`http://localhost:6286/events-stream?user=${user}&live=true&path=/pub/paykit/v0/private`, { signal: ctrl.signal });
  const headersAt = ms();
  const reader = res.body.getReader();
  const first = await reader.read();
  const firstChunkAt = ms();
  ctrl.abort();
  return { headersAt, firstChunkAt, bytes: first.value?.length, contentType: res.headers.get("content-type") };
}, user);
console.log(engine, browser.version(), JSON.stringify(r));
await browser.close();
