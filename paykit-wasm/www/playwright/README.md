# THROWAWAY (#411): Playwright drivers for the browser spike

These scripts drive `paykit-wasm/www` pages in Chrome (through Playwright's
`chromium` with `executablePath` set to Google Chrome), Playwright Firefox and
Playwright WebKit (the Safari engine, not Safari itself). Playwright is not a
repo dependency; install it into a scratch directory:

```sh
cd <scratch>/pw && npm init -y && npm install --ignore-scripts playwright@1.64.1
PLAYWRIGHT_BROWSERS_PATH=<scratch>/pw-browsers npx playwright install firefox webkit
cp <repo>/paykit-wasm/www/playwright/*.mjs .
```

Then, with a local `pubky-testnet`, `python3 -m http.server 8410 --directory paykit-wasm`
and (for the timeout steps) `python3 -I paykit-wasm/www/blackhole.py 15490`:

- `node run-wbt.mjs <engine> <url>`: open a `wasm-bindgen-test-runner` page
  started with `NO_HEADLESS=1` (add `WASM_BINDGEN_USE_DEDICATED_WORKER=1` for
  worker mode) and print the result.
- `node run-e2e.mjs <engine> "http://127.0.0.1:8410/www/index.html?bh=15490"`:
  the full e2e (use `worker.html` for the worker variant, `&nostore=1` to force
  `cache: "no-store"`).
- `node probe-sse.mjs <engine> <user>`: when `fetch()` resolves for an event
  stream whose first body bytes arrive late.
- `node lifecycle.mjs <engine> <multitab|reload|freeze|panic>`: the lifecycle
  experiments in `www/lifecycle.js`. `freeze` emulates a frozen tab with a
  main-thread busy loop, because CDP `Page.setWebLifecycleState` is a no-op
  in headless Chrome (`probe-freeze.mjs`, `probe-freeze2.mjs` show that).
- `node probe-features.mjs <engine>`: Web Locks, BroadcastChannel,
  SharedWorker, IndexedDB durability and WebDAV `LOCK` CORS preflight.
