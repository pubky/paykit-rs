#!/usr/bin/env node
// TLS and CORS in front of a local Pubky Switch service, for the paykit-browser network tests.
// Test rig code only, never for production: it serves a throwaway self-signed certificate,
// answers CORS for pages served on a loopback address only, and runs a native enrol program
// on request.
//
//   node switch-rig.mjs --listen <port> --upstream <port> [--enroll-bin <path>]
//
// `test.sh --switch` starts it; the service listens on 127.0.0.1:<upstream> in plain HTTP with
// the origin https://localhost:<listen>.
//
// - At start it makes a self-signed certificate for localhost (DNS:localhost, IP:127.0.0.1,
//   IP:::1) with openssl in a temporary folder, reads it, and removes the folder. It then
//   serves https://localhost:<listen> on both 127.0.0.1 and ::1, since a browser may resolve
//   localhost to either, and prints "[switch-rig] ready <origin>". When the port is taken on
//   either address it exits with an error. Only when the host has no IPv6 loopback address,
//   so that no other process can listen on [::1] either, does it serve on 127.0.0.1 alone,
//   with a warning in its log.
// - It answers CORS only for the test page's origin: http://127.0.0.1, http://localhost or
//   http://[::1], on any port, since wasm-bindgen-test-runner picks one. A dedicated worker's
//   requests carry its page's origin. A request from such an origin gets that origin back in
//   access-control-allow-origin; a request from any other origin, or with none, gets no CORS
//   headers, so a browser hides the answer from any other page. Every answer has
//   vary: origin.
// - It answers every OPTIONS request itself, as a CORS preflight (the service answers none).
// - It serves the routes under /rig/ itself and never forwards them; a bad parameter gets a 400
//   with a fixed text:
//   GET /rig/health                   200 "ok", once the rig serves.
//   any method but OPTIONS /rig/echo  200 JSON {"method", "path" (the raw request target),
//                                     "headers": [[lower-case name, value], ...] in the order
//                                     received, "body_b64": the body in base64url without
//                                     padding}.
//   GET /rig/bytes?n=N&seed=S[&chunked=1]
//                                     200 application/octet-stream, N bytes (at most 64 MiB),
//                                     byte i being (i * 31 + S) mod 256 for S in 0..255, with
//                                     x-rig-fixed: rig-fixed-value; with content-length, or
//                                     streamed in chunks without it given chunked=1.
//   GET /rig/truncated?length=L       200 application/octet-stream with content-length L
//                                     (17..9999999999), then 16 bytes of body, then closes the
//                                     connection.
//   GET /rig/status?code=C            status C (200..299 or 400..599), body "status C".
//   GET /rig/redirect?tag=T           302 to /rig/echo?tag=T.
//   GET /rig/slow?ms=M                200 "slow" after M ms, at most 30000.
//   GET /rig/count?tag=T              200 JSON {"requests": the requests so far whose query
//                                     has tag=T, on any route but this one, preflights
//                                     included; "tls_errors": the failed TLS handshakes so far}.
//   POST /rig/world                   body {"identities": [name, ...], "apps": [{"identity",
//                                     "app"}, ...]}, names matching ^[a-z][a-z0-9-]{0,31}$: runs
//                                     `<enroll-bin> --switch-url http://127.0.0.1:<upstream>
//                                     --origin https://localhost:<listen> --seed-prefix <32 hex>
//                                     --identity <name>... --app <identity>:<app>...` once, with
//                                     a fresh random seed prefix, and answers 200 with the JSON
//                                     object it prints, unchanged, or 500 with a fixed text when
//                                     it fails or takes over 45 s; its stderr goes to this log
//                                     only. When the client's connection closes first, the rig
//                                     stops the program. A request with an Origin header that
//                                     is not the test page's gets a 403 and runs nothing, so no
//                                     other page can start the program, not even without CORS;
//                                     a request with no Origin, such as from curl, is served.
// - It forwards every other request to the service as received: the method, the raw request
//   target, the raw headers in their order except the hop-by-hop ones, and the body. It passes
//   the status and the raw response headers back the same way, and the body. When the service
//   cannot be reached or breaks off, it destroys the browser's connection rather than make up an
//   answer.
// - It logs one line per request: the method, the path without the query, and the status.
//   It never logs header values or bodies.

import { execFile, execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import http from "node:http";
import https from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Readable, pipeline } from "node:stream";
import { parseArgs } from "node:util";

// The request headers a Switch request carries (RECEIVED_HEADERS of pubky_switch_core::http,
// Switch wire 7.1), and the body's type.
const ALLOW_HEADERS = [
  "switch-identity",
  "switch-app",
  "switch-cert",
  "switch-invite-code",
  "switch-transfer",
  "switch-timestamp",
  "switch-signature",
  "switch-created",
  "content-type",
];
// The signed response headers the Switch client reads, Retry-After (read on a 429), and the
// header of /rig/bytes. A cross-origin page can read only these and the CORS-safelisted ones.
const EXPOSE_HEADERS = [
  "switch-service-signature",
  "switch-server-time",
  "switch-head",
  "retry-after",
  "x-rig-fixed",
];
// The origins of the test page, which wasm-bindgen-test-runner serves over plain HTTP on a
// loopback address and a port it picks.
const PAGE_ORIGIN = /^http:\/\/(?:127\.0\.0\.1|localhost|\[::1\])(?::[1-9][0-9]{0,4})?$/;
const EXPOSE = ["access-control-expose-headers", EXPOSE_HEADERS.join(", ")];
const PREFLIGHT_HEADERS = [
  ["access-control-allow-methods", "GET, POST, PUT, DELETE"],
  ["access-control-allow-headers", ALLOW_HEADERS.join(", ")],
  ["access-control-max-age", "600"],
];
// Headers that belong to one connection, not to the message (RFC 9110 7.6.1).
const HOP_BY_HOP = new Set([
  "connection",
  "keep-alive",
  "proxy-connection",
  "transfer-encoding",
  "te",
  "trailer",
  "upgrade",
]);

// Whether the request's Origin header names the test page.
function fromPage(req) {
  return PAGE_ORIGIN.test(req.headers.origin ?? "");
}

// The CORS headers of an answer to `req`, with `headers` added for the test page's origin:
// none but vary: origin for any other origin, or with no Origin header.
function corsHeaders(req, headers = [EXPOSE]) {
  const vary = ["vary", "origin"];
  if (!fromPage(req)) return [vary];
  return [["access-control-allow-origin", req.headers.origin], vary, ...headers];
}

const MAX_BYTES = 64 * 1024 * 1024;
const BYTES_CHUNK = 64 * 1024; // a multiple of 256, so every chunk of /rig/bytes is the same
const MAX_SLOW_MS = 30_000;
const MAX_ECHO_BODY = 16 * 1024 * 1024;
const MAX_WORLD_BODY = 64 * 1024;
const NAME = /^[a-z][a-z0-9-]{0,31}$/;
const MAX_IDENTITIES = 8;
const MAX_APPS = 16;
// Shorter than the 60 s the browser test gives /rig/world (tests/switch_flow/switch_world.rs),
// so a slow enrolment fails with the rig's answer rather than the client's timeout.
const ENROLL_TIMEOUT_MS = 45_000;
const ENROLL_MAX_OUTPUT = 4 * 1024 * 1024;

// The rig's routes: path -> [method or null for any, handler].
const ROUTES = {
  "/rig/health": ["GET", health],
  "/rig/echo": [null, echo],
  "/rig/bytes": ["GET", bytes],
  "/rig/truncated": ["GET", truncated],
  "/rig/status": ["GET", status],
  "/rig/redirect": ["GET", redirect],
  "/rig/slow": ["GET", slow],
  "/rig/count": ["GET", count],
  "/rig/world": ["POST", world],
};

function log(line) {
  process.stdout.write(`[switch-rig] ${line}\n`);
}

function fail(message) {
  process.stderr.write(`[switch-rig] error: ${message}\n`);
  process.exit(2);
}

function port(text, option) {
  if (!/^[1-9][0-9]{0,4}$/.test(text ?? "") || Number(text) > 65535) {
    fail(`--${option} must be a port number`);
  }
  return Number(text);
}

let options;
try {
  ({ values: options } = parseArgs({
    options: {
      listen: { type: "string" },
      upstream: { type: "string" },
      "enroll-bin": { type: "string" },
    },
  }));
} catch (error) {
  fail(`${error.message}\nusage: switch-rig.mjs --listen <port> --upstream <port> [--enroll-bin <path>]`);
}
const listenPort = port(options.listen, "listen");
const upstreamPort = port(options.upstream, "upstream");
const enrollBin = options["enroll-bin"];
const origin = `https://localhost:${listenPort}`;
const switchUrl = `http://127.0.0.1:${upstreamPort}`;

// A fresh connection per forwarded request: a kept-alive connection the service closes while
// idle could break the next request on it.
const upstreamAgent = new http.Agent({ keepAlive: false });
// The requests seen per value of their `tag` query parameter, and the failed TLS handshakes.
const tagCounts = new Map();
let tlsErrors = 0;
// Running enrol programs, stopped when the rig stops.
const enrollChildren = new Set();

// A throwaway certificate, generated at each start and kept in memory only.
function makeCertificate() {
  const dir = mkdtempSync(join(tmpdir(), "paykit-browser-rig-tls-"));
  const keyPath = join(dir, "key.pem");
  const certPath = join(dir, "cert.pem");
  let failure;
  try {
    execFileSync(
      "openssl",
      [
        "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
        "-keyout", keyPath, "-out", certPath, "-days", "2", "-subj", "/CN=localhost",
        "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1",
      ],
      { stdio: ["ignore", "ignore", "pipe"] },
    );
    return { key: readFileSync(keyPath), cert: readFileSync(certPath) };
  } catch (error) {
    failure = error.stderr?.toString().trim() || error.message;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  fail(`openssl could not make the certificate: ${failure}`);
}

// [[name, value], ...] from a raw header list, which alternates names and values.
function rawPairs(raw) {
  const pairs = [];
  for (let i = 0; i < raw.length; i += 2) pairs.push([raw[i], raw[i + 1]]);
  return pairs;
}

// [[name, value], ...] from a raw header list, without the hop-by-hop headers and the headers
// the Connection header names.
function endToEndHeaders(raw) {
  const pairs = rawPairs(raw);
  const dropped = new Set(HOP_BY_HOP);
  for (const [name, value] of pairs) {
    if (name.toLowerCase() !== "connection") continue;
    for (const token of value.split(",")) dropped.add(token.trim().toLowerCase());
  }
  return pairs.filter(([name]) => !dropped.has(name.toLowerCase()));
}

function answer(res, code, type, body, headers = []) {
  const head = [...headers, ...corsHeaders(res.req)];
  if (type) head.push(["content-type", type]);
  if (code !== 204) head.push(["content-length", String(Buffer.byteLength(body))]);
  res.writeHead(code, head.flat());
  res.end(code === 204 ? undefined : body);
}

function text(res, code, body, headers) {
  answer(res, code, "text/plain; charset=utf-8", body, headers);
}

function json(res, value) {
  answer(res, 200, "application/json", JSON.stringify(value));
}

// The request body, up to `limit` bytes; null when it is longer.
function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let length = 0;
    req.on("data", (chunk) => {
      length += chunk.length;
      if (length <= limit) chunks.push(chunk);
    });
    req.on("end", () => resolve(length <= limit ? Buffer.concat(chunks) : null));
    req.on("error", reject);
  });
}

// An integer query parameter within [min, max], or undefined when it is missing or malformed.
function intParam(query, name, min, max) {
  const value = query.get(name);
  if (value === null || !/^[0-9]{1,10}$/.test(value)) return undefined;
  const number = Number(value);
  return number >= min && number <= max ? number : undefined;
}

function health(req, res) {
  req.resume();
  text(res, 200, "ok");
}

// The request as the rig received it.
async function echo(req, res) {
  const body = await readBody(req, MAX_ECHO_BODY);
  if (body === null) return text(res, 413, "body too large");
  const headers = rawPairs(req.rawHeaders).map(([name, value]) => [name.toLowerCase(), value]);
  json(res, { method: req.method, path: req.url, headers, body_b64: body.toString("base64url") });
}

// N bytes, byte i being (i * 31 + seed) mod 256.
function bytes(req, res, query) {
  req.resume();
  const n = intParam(query, "n", 0, MAX_BYTES);
  const seed = intParam(query, "seed", 0, 255);
  const chunked = query.get("chunked");
  if (n === undefined || seed === undefined || (chunked !== null && chunked !== "1")) {
    return text(res, 400, `need n in 0..${MAX_BYTES}, seed in 0..255, and chunked=1 or nothing`);
  }
  const block = Buffer.alloc(Math.min(n, BYTES_CHUNK));
  for (let i = 0; i < block.length; i += 1) block[i] = (i * 31 + seed) % 256;
  const head = [["content-type", "application/octet-stream"], ["x-rig-fixed", "rig-fixed-value"]];
  if (chunked === null) head.push(["content-length", String(n)]);
  res.writeHead(200, [...head, ...corsHeaders(req)].flat());
  function* blocks() {
    for (let left = n; left > 0; left -= block.length) {
      yield left >= block.length ? block : block.subarray(0, left);
    }
  }
  pipeline(Readable.from(blocks(), { objectMode: false }), res, () => {});
}

// A body that breaks off: 16 bytes of the `length` its content-length announces.
function truncated(req, res, query) {
  req.resume();
  const length = intParam(query, "length", 17, 9_999_999_999);
  if (length === undefined) return text(res, 400, "need length in 17..9999999999");
  const head = [["content-type", "application/octet-stream"], ["content-length", String(length)]];
  res.writeHead(200, [...head, ...corsHeaders(req)].flat());
  res.write(Buffer.alloc(16), () => res.destroy());
}

function status(req, res, query) {
  req.resume();
  const code = intParam(query, "code", 200, 599);
  if (code === undefined || (code >= 300 && code < 400)) {
    return text(res, 400, "need code in 200..299 or 400..599");
  }
  text(res, code, code === 204 || code === 205 ? "" : `status ${code}`);
}

function redirect(req, res, query) {
  req.resume();
  const location = `/rig/echo?tag=${encodeURIComponent(query.get("tag") ?? "")}`;
  text(res, 302, "redirect", [["location", location]]);
}

function slow(req, res, query) {
  req.resume();
  const ms = intParam(query, "ms", 0, MAX_SLOW_MS);
  if (ms === undefined) return text(res, 400, `need ms in 0..${MAX_SLOW_MS}`);
  const timer = setTimeout(() => text(res, 200, "slow"), ms);
  res.on("close", () => clearTimeout(timer));
}

function count(req, res, query) {
  req.resume();
  const tag = query.get("tag");
  if (tag === null) return text(res, 400, "need tag");
  json(res, { requests: tagCounts.get(tag) ?? 0, tls_errors: tlsErrors });
}

// The identities and apps of a /rig/world request, or a fixed error text.
function parseWorld(body) {
  let request;
  try {
    request = JSON.parse(body.toString("utf8"));
  } catch {
    return { error: "the body is not JSON" };
  }
  const plain = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  if (!plain(request) || Object.keys(request).some((key) => key !== "identities" && key !== "apps")) {
    return { error: 'need an object with "identities" and "apps" only' };
  }
  const { identities, apps } = request;
  if (
    !Array.isArray(identities) ||
    identities.length < 1 ||
    identities.length > MAX_IDENTITIES ||
    !identities.every((name) => typeof name === "string" && NAME.test(name)) ||
    new Set(identities).size !== identities.length
  ) {
    return { error: `need 1..${MAX_IDENTITIES} distinct identity names matching ${NAME.source}` };
  }
  const seen = new Set();
  if (!Array.isArray(apps) || apps.length > MAX_APPS) {
    return { error: `need 0..${MAX_APPS} apps` };
  }
  for (const app of apps) {
    const valid =
      plain(app) &&
      Object.keys(app).length === 2 &&
      identities.includes(app.identity) &&
      typeof app.app === "string" &&
      NAME.test(app.app) &&
      !seen.has(`${app.identity}:${app.app}`);
    if (!valid) {
      return { error: `each app needs a declared "identity" and an "app" name matching ${NAME.source}, once` };
    }
    seen.add(`${app.identity}:${app.app}`);
  }
  return { identities, apps };
}

// Enrols the identities and approves the apps with the native enrol program, under a fresh
// seed prefix, and answers with the JSON object it prints.
async function world(req, res) {
  // A page of another origin can send a POST without CORS: refuse it before reading anything.
  if (req.headers.origin !== undefined && !fromPage(req)) {
    req.resume();
    return text(res, 403, "the rig serves /rig/world to the test page only");
  }
  const body = await readBody(req, MAX_WORLD_BODY);
  if (body === null) return text(res, 413, "body too large");
  const parsed = parseWorld(body);
  if (parsed.error) return text(res, 400, parsed.error);
  if (!enrollBin) return text(res, 500, "the rig has no enrol program");

  const args = [
    "--switch-url", switchUrl,
    "--origin", origin,
    "--seed-prefix", randomBytes(16).toString("hex"),
  ];
  for (const name of parsed.identities) args.push("--identity", name);
  for (const { identity, app } of parsed.apps) args.push("--app", `${identity}:${app}`);

  const child = execFile(
    enrollBin,
    args,
    { encoding: "utf8", timeout: ENROLL_TIMEOUT_MS, maxBuffer: ENROLL_MAX_OUTPUT },
    (error, stdout, stderr) => {
      enrollChildren.delete(child);
      for (const line of stderr.split("\n")) if (line) log(`enrol: ${line}`);
      if (res.destroyed) {
        log("enrol program stopped: the client left");
        return;
      }
      if (error) {
        log(`enrol program failed: ${error.code ?? error.signal ?? error.message}`);
        return text(res, 500, "the enrol program failed");
      }
      let output;
      try {
        output = JSON.parse(stdout);
      } catch {
        output = undefined;
      }
      if (output === null || typeof output !== "object" || Array.isArray(output)) {
        log("enrol program printed no JSON object");
        return text(res, 500, "the enrol program printed no JSON object");
      }
      answer(res, 200, "application/json", stdout);
    },
  );
  enrollChildren.add(child);
  // A client that gives up, such as on its own timeout, closes its connection: stop the
  // program rather than let it enrol for nobody.
  res.on("close", () => {
    if (enrollChildren.has(child)) child.kill("SIGTERM");
  });
}

function serveRig(req, res, path, query) {
  if (!Object.hasOwn(ROUTES, path)) {
    req.resume();
    return text(res, 404, "no such rig route");
  }
  const [method, handler] = ROUTES[path];
  if (method !== null && req.method !== method) {
    req.resume();
    return text(res, 405, "method not allowed", [["allow", method]]);
  }
  Promise.resolve(handler(req, res, query)).catch((error) => {
    log(`${req.method} ${path}: ${error.message}`);
    res.destroy();
  });
}

function forward(req, res) {
  const upstream = http.request({
    host: "127.0.0.1",
    port: upstreamPort,
    method: req.method,
    path: req.url,
    headers: endToEndHeaders(req.rawHeaders).flat(),
    setHost: false,
    agent: upstreamAgent,
  });
  upstream.on("response", (reply) => {
    const head = [...endToEndHeaders(reply.rawHeaders), ...corsHeaders(req)];
    res.writeHead(reply.statusCode, reply.statusMessage, head.flat());
    pipeline(reply, res, (error) => {
      if (error) {
        log(`upstream answer broke off: ${error.message}`);
        res.destroy();
      }
    });
  });
  upstream.on("error", (error) => {
    log(`upstream: ${error.message}`);
    res.destroy();
  });
  pipeline(req, upstream, (error) => {
    if (error) upstream.destroy();
  });
}

function handle(req, res) {
  const target = req.url ?? "";
  const queryStart = target.indexOf("?");
  const path = queryStart < 0 ? target : target.slice(0, queryStart);
  const query = new URLSearchParams(queryStart < 0 ? "" : target.slice(queryStart + 1));
  res.on("close", () => {
    const code = res.headersSent ? res.statusCode : "no answer";
    log(`${req.method} ${path} ${code}${res.writableFinished ? "" : " (connection closed early)"}`);
  });

  if (path !== "/rig/count") {
    for (const tag of new Set(query.getAll("tag"))) {
      tagCounts.set(tag, (tagCounts.get(tag) ?? 0) + 1);
    }
  }
  if (req.method === "OPTIONS") {
    req.resume();
    res.writeHead(204, corsHeaders(req, PREFLIGHT_HEADERS).flat());
    return res.end();
  }
  // Decided on the raw path, so no spelling of a /rig/ path reaches the service.
  if (path === "/rig" || path.startsWith("/rig/")) return serveRig(req, res, path, query);
  forward(req, res);
}

// One server per loopback address, with the same certificate and handler.
function serve(tls, address) {
  return new Promise((resolve, reject) => {
    const server = https.createServer(tls, handle);
    server.on("tlsClientError", (error, socket) => {
      tlsErrors += 1;
      log(`TLS client error: ${error.code ?? error.message}`);
      socket.destroy();
    });
    server.once("error", reject);
    server.listen(listenPort, address, () => {
      server.off("error", reject);
      server.on("error", (error) => fail(`serving on ${address}: ${error.message}`));
      resolve(server);
    });
  });
}

const tls = makeCertificate();
try {
  await serve(tls, "127.0.0.1");
} catch (error) {
  fail(`cannot serve on 127.0.0.1:${listenPort}: ${error.message}`);
}
try {
  await serve(tls, "::1");
} catch (error) {
  // No IPv6 loopback address on this host: nothing can listen on [::1], and no browser
  // reaches localhost there.
  if (error.code !== "EADDRNOTAVAIL" && error.code !== "EAFNOSUPPORT") {
    fail(`cannot serve on [::1]:${listenPort}: ${error.message}`);
  }
  log(`warning: no IPv6 loopback address (${error.code}); serving on 127.0.0.1 only`);
}
log(`ready ${origin}`);

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    for (const child of enrollChildren) child.kill("SIGTERM");
    process.exit(0);
  });
}
