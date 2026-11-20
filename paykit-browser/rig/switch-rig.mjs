#!/usr/bin/env node
// TLS and CORS in front of a local Pubky Switch service, for the paykit-browser network tests.
// Test rig code only, never for production: it serves a throwaway self-signed certificate,
// answers CORS for pages served on a loopback address only, and runs a native enrol program
// on request.
//
//   PAYKIT_BROWSER_RIG_TOKEN=<32 hex> node switch-rig.mjs --listen <port> --upstream <port> [--enroll-bin <path>]
//
// `test.sh --switch` starts it; the service listens on 127.0.0.1:<upstream> in plain HTTP with
// the origin https://localhost:<listen>. PAYKIT_BROWSER_RIG_TOKEN is a random token of the run,
// 32 lower-case hex digits, that POST /rig/world and /rig/fault require; the rig exits with an
// error without one. It comes in the environment, not as an argument, so that the process list
// does not show it.
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
// - It answers every OPTIONS request itself, as a CORS preflight, and forwards none to the
//   service, with access-control-max-age: 600. A preflight whose query has max_age=none gets
//   no access-control-max-age, and one with max_age=0 gets 0, on any path. The query of a
//   forwarded request reaches the service unchanged.
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
//   GET /rig/hang?tag=T[&stage=headers]
//                                     reads the request and never answers; with stage=headers
//                                     it sends the head of a 200 application/octet-stream
//                                     answer with content-length: 1024, and then no body. The
//                                     rig destroys the connection after 60 s.
//   GET /rig/cacheable?tag=T          200 with cache-control: public, max-age=60, body the
//                                     number of requests to this route with tag=T so far,
//                                     this one included, in decimal.
//   GET /rig/count?tag=T              200 JSON {"requests": the requests so far whose query
//                                     has tag=T, on any route but this one, preflights
//                                     included; "preflights": the OPTIONS requests among
//                                     them; "closed_early": the others whose connection the
//                                     client closed before the rig finished its answer (the
//                                     rig's own destroys, as on /rig/hang, do not count);
//                                     "open": the others still in progress}.
//   GET /rig/hits?path=P[&method=M]   200 JSON {"preflights": the OPTIONS requests so far to
//                                     the forwarded raw path P, without its query, of any
//                                     method; and among the other requests to P, with method
//                                     M when given: "forwarded": those the rig sent on to the
//                                     service, a lost or held one included once it is sent;
//                                     "refused": those a "refuse" fault kept from the service;
//                                     "open": those still in progress, sent on or not}. A
//                                     held request whose client closes its connection before
//                                     the hold ends is neither forwarded nor refused. P starts
//                                     with / and has at most 512 bytes.
//   POST /rig/world?token=<token>     body {"identities": [name, ...], "apps": [{"identity",
//                                     "app"}, ...]}, names matching ^[a-z][a-z0-9-]{0,31}$: runs
//                                     `<enroll-bin> --switch-url http://127.0.0.1:<upstream>
//                                     --origin https://localhost:<listen> --seed-prefix <32 hex>
//                                     --identity <name>... --app <identity>:<app>...` once, with
//                                     a fresh random seed prefix, and answers 200 with the JSON
//                                     object it prints, unchanged, or 500 with a fixed text when
//                                     it fails or takes over 45 s; its stderr goes to this log
//                                     only. When the client's connection closes first, the rig
//                                     stops the program. A request with an Origin header that
//                                     is not the test page's, or without the run's token, gets
//                                     a 403 and runs nothing, so no other page, and no program
//                                     that does not know the token, can start the enrol
//                                     program, not even without CORS. The token is compared in
//                                     constant time.
//   POST /rig/fault?token=<token>     body {"method": "GET", "POST", "PUT" or "DELETE",
//                                     "path": a raw path without query, starting with / and of
//                                     at most 512 bytes, "kind": "lose", "refuse" or "hold",
//                                     "ms" in 1..30000 for "hold" only, and optionally "after"
//                                     in 0..16, 0 when left out}: arms that one fault, replacing
//                                     any armed one, and answers 200 JSON {"armed": true}. Only
//                                     a request that is the first its connection carries can
//                                     fire the fault or count toward "after": the forwarded
//                                     requests, not OPTIONS, with that method and path, each
//                                     first on its connection, go through unchanged until
//                                     "after" of them have; the next one fires the fault and
//                                     disarms it. A matching request on a connection that
//                                     carried a request before goes through unchanged and is
//                                     not counted. The rig first reads the body of the
//                                     request the fault fires on whole, at most 16 MiB (a 413
//                                     above that), then: "lose" forwards it, reads the
//                                     service's whole answer, and destroys the browser's
//                                     connection without a byte; "refuse" destroys the
//                                     browser's connection and forwards nothing; "hold"
//                                     forwards it after ms, or never when the client closes
//                                     its connection first. While a fault is armed, the rig
//                                     closes each browser connection after its answer, the
//                                     arming one included. When it is armed, the rig closes
//                                     the idle connections at once, and each busy one fully,
//                                     reading nothing more from it, once its answer in
//                                     progress has ended, without cutting that answer. A
//                                     browser may send a request again on its own when a
//                                     connection it reused closes without a byte of the
//                                     answer, which would hide the fault or have the service
//                                     apply the request twice. Firing only on the first
//                                     request of a connection is meant to avoid that, and
//                                     the rig counts any such resend: a later forwarded
//                                     request with the same method, raw request target,
//                                     Switch request headers (ALLOW_HEADERS below) in order
//                                     and body as the one the fault fired on, whose body it
//                                     reads whole to compare it, is forwarded unchanged and
//                                     counted in "resent".
//   GET /rig/fault?token=<token>      200 JSON {"armed": whether a fault is armed, "fired": the
//                                     faults fired since the rig started, "resent": the
//                                     resends of the request the last fault fired on, since
//                                     it was armed}.
//   DELETE /rig/fault?token=<token>   disarms the fault; 200 JSON {"armed": false}.
//                                     POST and DELETE both forget the request the last fault
//                                     fired on, and set "resent" to 0. /rig/fault checks the
//                                     Origin header and the token as POST /rig/world does,
//                                     with the same 403s.
// - It forwards every other request to the service as received: the method, the raw request
//   target, the raw headers in their order except the hop-by-hop ones, and the body. It passes
//   the status and the raw response headers back the same way, and the body. When the service
//   cannot be reached or breaks off, it destroys the browser's connection rather than make up an
//   answer.
// - It logs one line per request: the method, the path without the query, and the status,
//   one line per fired fault: the kind, the method and the path without the query, and one
//   line per resend of a faulted request: the method and the path without the query. It
//   never logs header values, bodies or the token.

import { execFile, execFileSync } from "node:child_process";
import { createHash, randomBytes, timingSafeEqual } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import http from "node:http";
import https from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Readable, finished, pipeline } from "node:stream";
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
const HANG_CAP_MS = 60_000;
const MAX_HITS_PATH = 512;
const MAX_FAULT_BODY = 1024;
const FAULT_METHODS = ["GET", "POST", "PUT", "DELETE"];
const FAULT_KINDS = ["lose", "refuse", "hold"];
const FAULT_ERROR =
  `need {"method": GET, POST, PUT or DELETE, "path": a raw path starting with / of at most ` +
  `${MAX_HITS_PATH} bytes, "kind": lose, refuse or hold, "ms" in 1..30000 for hold only, and ` +
  'optionally "after" in 0..16}';
// The most matching requests a fault lets through before it fires.
const MAX_FAULT_AFTER = 16;

// The rig's routes: path -> [methods, or null for any, handler].
const ROUTES = {
  "/rig/health": [["GET"], health],
  "/rig/echo": [null, echo],
  "/rig/bytes": [["GET"], bytes],
  "/rig/truncated": [["GET"], truncated],
  "/rig/status": [["GET"], status],
  "/rig/redirect": [["GET"], redirect],
  "/rig/slow": [["GET"], slow],
  "/rig/hang": [["GET"], hang],
  "/rig/cacheable": [["GET"], cacheable],
  "/rig/count": [["GET"], count],
  "/rig/hits": [["GET"], hits],
  "/rig/world": [["POST"], world],
  "/rig/fault": [["GET", "POST", "DELETE"], fault],
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
// The run's token for /rig/world and /rig/fault, which test.sh makes and builds into the
// network tests.
const runToken = process.env.PAYKIT_BROWSER_RIG_TOKEN ?? "";
if (!/^[0-9a-f]{32}$/.test(runToken)) {
  fail("PAYKIT_BROWSER_RIG_TOKEN must be the run's token, 32 lower-case hex digits");
}
const origin = `https://localhost:${listenPort}`;
const switchUrl = `http://127.0.0.1:${upstreamPort}`;

// A fresh connection per forwarded request: a kept-alive connection the service closes while
// idle could break the next request on it.
const upstreamAgent = new http.Agent({ keepAlive: false });
// The requests seen per value of their `tag` query parameter: {requests, preflights,
// closed_early, open}.
const tagCounts = new Map();
// /rig/cacheable's requests per tag.
const cacheableCounts = new Map();
// The requests seen per raw path the rig forwards: {preflights, methods: method ->
// {forwarded, refused, open}}.
const pathHits = new Map();
// Answers the rig destroyed itself, so that their close does not count as the client's.
const cutByRig = new WeakSet();
// The browser connections that carried a request already: no fault fires on them.
const usedConnections = new WeakSet();
// The answers in progress, whose connections close after them once a fault is armed.
const inFlight = new Set();
// The servers, one per loopback address.
const servers = [];
// The armed fault, or null, and the faults fired so far.
let armedFault = null;
let faultsFired = 0;
// The request the last fault fired on, {method, target, digest} (see fingerprint), or null;
// the resends of it seen since; and a count of the arms and disarms, so that a fingerprint
// taken after the fault was replaced is dropped.
let faultedRequest = null;
let faultResends = 0;
let faultEpoch = 0;
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

// Destroys the browser's connection, as the rig's own doing rather than the client's.
function cut(res) {
  cutByRig.add(res);
  res.destroy();
}

function plain(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

// The entry of `key` in `map`, made with `make` when missing.
function entry(map, key, make) {
  if (!map.has(key)) map.set(key, make());
  return map.get(key);
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
  res.write(Buffer.alloc(16), () => cut(res));
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

// An answer that never comes, or whose body never comes, until the rig's cap.
function hang(req, res, query) {
  req.resume();
  const stage = query.get("stage");
  if (query.get("tag") === null || (stage !== null && stage !== "headers")) {
    return text(res, 400, "need tag, and stage=headers or nothing");
  }
  if (stage === "headers") {
    const head = [["content-type", "application/octet-stream"], ["content-length", "1024"]];
    res.writeHead(200, [...head, ...corsHeaders(req)].flat());
    res.flushHeaders();
  }
  const timer = setTimeout(() => cut(res), HANG_CAP_MS);
  res.on("close", () => clearTimeout(timer));
}

function cacheable(req, res, query) {
  req.resume();
  const tag = query.get("tag");
  if (tag === null) return text(res, 400, "need tag");
  const served = (cacheableCounts.get(tag) ?? 0) + 1;
  cacheableCounts.set(tag, served);
  text(res, 200, String(served), [["cache-control", "public, max-age=60"]]);
}

function newTagCount() {
  return { requests: 0, preflights: 0, closed_early: 0, open: 0 };
}

function count(req, res, query) {
  req.resume();
  const tag = query.get("tag");
  if (tag === null) return text(res, 400, "need tag");
  json(res, tagCounts.get(tag) ?? newTagCount());
}

// Whether `path` may name a forwarded raw path in /rig/hits and /rig/fault.
function validPath(path) {
  return typeof path === "string" && path.startsWith("/") && Buffer.byteLength(path) <= MAX_HITS_PATH;
}

function hits(req, res, query) {
  req.resume();
  const path = query.get("path");
  const method = query.get("method");
  if (!validPath(path) || (method !== null && !/^[A-Z]{1,16}$/.test(method))) {
    return text(
      res,
      400,
      `need a path starting with / of at most ${MAX_HITS_PATH} bytes, and an upper-case method or nothing`,
    );
  }
  const seen = pathHits.get(path);
  const total = { preflights: seen?.preflights ?? 0, forwarded: 0, refused: 0, open: 0 };
  for (const [name, counts] of seen?.methods ?? []) {
    if (method !== null && name !== method) continue;
    total.forwarded += counts.forwarded;
    total.refused += counts.refused;
    total.open += counts.open;
  }
  json(res, total);
}

// The identities and apps of a /rig/world request, or a fixed error text.
function parseWorld(body) {
  let request;
  try {
    request = JSON.parse(body.toString("utf8"));
  } catch {
    return { error: "the body is not JSON" };
  }
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

// Whether `query` carries the run's token, compared in constant time.
function hasRunToken(query) {
  const given = Buffer.from(query.get("token") ?? "", "utf8");
  const expected = Buffer.from(runToken, "utf8");
  return given.length === expected.length && timingSafeEqual(given, expected);
}

// Answers 403 and returns true for a request from another page, or without the run's token.
// A page of another origin can send a POST without CORS, and any local program can send
// one: the routes that change the rig refuse them before reading anything.
function refuseOutsider(req, res, query, route) {
  if (req.headers.origin !== undefined && !fromPage(req)) {
    req.resume();
    text(res, 403, `the rig serves ${route} to the test page only`);
    return true;
  }
  if (!hasRunToken(query)) {
    req.resume();
    text(res, 403, `the rig serves ${route} to this run's tests only`);
    return true;
  }
  return false;
}

// Enrols the identities and approves the apps with the native enrol program, under a fresh
// seed prefix, and answers with the JSON object it prints.
async function world(req, res, query) {
  if (refuseOutsider(req, res, query, "/rig/world")) return;
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

// The fault a /rig/fault body describes, or null when it is not one.
function parseFault(body) {
  let request;
  try {
    request = JSON.parse(body.toString("utf8"));
  } catch {
    return null;
  }
  if (!plain(request)) return null;
  const { method, path, kind, ms, after = 0 } = request;
  const keys = Object.keys(request)
    .filter((key) => key !== "after")
    .sort()
    .join(" ");
  const valid =
    keys === (kind === "hold" ? "kind method ms path" : "kind method path") &&
    Number.isInteger(after) &&
    after >= 0 &&
    after <= MAX_FAULT_AFTER &&
    FAULT_METHODS.includes(method) &&
    FAULT_KINDS.includes(kind) &&
    validPath(path) &&
    (kind !== "hold" || (Number.isInteger(ms) && ms >= 1 && ms <= MAX_SLOW_MS));
  if (!valid) return null;
  return { method, path, kind, ms, after };
}

// Arms `fault`, or disarms with null, and forgets the request the last fault fired on.
function setFault(fault) {
  armedFault = fault;
  faultEpoch += 1;
  faultedRequest = null;
  faultResends = 0;
}

// Arms, reports or disarms the one fault for forwarded requests.
async function fault(req, res, query) {
  if (refuseOutsider(req, res, query, "/rig/fault")) return;
  if (req.method === "GET") {
    req.resume();
    return json(res, { armed: armedFault !== null, fired: faultsFired, resent: faultResends });
  }
  if (req.method === "DELETE") {
    req.resume();
    setFault(null);
    return json(res, { armed: false });
  }
  const body = await readBody(req, MAX_FAULT_BODY);
  if (body === null) return text(res, 413, "body too large");
  const parsed = parseFault(body);
  if (parsed === null) return text(res, 400, FAULT_ERROR);
  setFault(parsed);
  // The connections open now close, so the browser's next requests come on new ones, the
  // first request of which can fire the fault (see the header): the busy ones, this one
  // included, close after their answers, the idle ones now.
  for (const busy of inFlight) closeAfterAnswer(busy);
  for (const server of servers) server.closeIdleConnections();
  json(res, { armed: true });
}

// Makes the browser's connection of `res`, an answer in progress, close once the answer has
// ended, without cutting it: before the head is sent, the head says connection: close, and
// Node closes the connection after the answer; after, once the answer's last byte is handed
// to the connection, the rig stops reading from it, so that no request the browser sends on
// it later is parsed, then ends it and destroys it once that byte is written. A connection
// whose answer is already finished is idle, and closeIdleConnections closes it.
function closeAfterAnswer(res) {
  if (!res.headersSent) {
    res.shouldKeepAlive = false;
    return;
  }
  const socket = res.socket;
  if (!socket || socket.destroyed) return;
  const close = () => {
    socket.pause();
    socket.end(() => socket.destroy());
  };
  if (res.writableFinished) return close();
  res.once("finish", close);
}

function serveRig(req, res, path, query) {
  if (!Object.hasOwn(ROUTES, path)) {
    req.resume();
    return text(res, 404, "no such rig route");
  }
  const [methods, handler] = ROUTES[path];
  if (methods !== null && !methods.includes(req.method)) {
    req.resume();
    return text(res, 405, "method not allowed", [["allow", methods.join(", ")]]);
  }
  Promise.resolve(handler(req, res, query)).catch((error) => {
    log(`${req.method} ${path}: ${error.message}`);
    cut(res);
  });
}

// The armed fault when it matches this forwarded request, the first its connection carries
// (`firstOnConnection`), and let its "after" matching requests through already, disarmed;
// otherwise null.
function takeFault(req, path, firstOnConnection) {
  const armed = armedFault;
  if (armed === null || !firstOnConnection) return null;
  if (req.method !== armed.method || path !== armed.path) return null;
  if (armed.after > 0) {
    armed.after -= 1;
    return null;
  }
  armedFault = null;
  faultsFired += 1;
  log(`fault ${armed.kind} ${req.method} ${path}`);
  return armed;
}

// The fingerprint of a forwarded request, to recognize a browser's own resend of it: a
// SHA-256 of its method, raw request target, Switch request headers in order and body, the
// body read whole first. A Switch request's signature covers its timestamp and its body's
// hash (Switch wire 7.1), so two requests the client signed apart differ. Returns {digest,
// body}, or null when the body is longer than MAX_ECHO_BODY. The digest is only compared,
// never logged or answered.
async function fingerprint(req) {
  const body = await readBody(req, MAX_ECHO_BODY);
  if (body === null) return null;
  const switchHeaders = rawPairs(req.rawHeaders).filter(([name]) =>
    ALLOW_HEADERS.includes(name.toLowerCase()),
  );
  const hash = createHash("sha256");
  hash.update(JSON.stringify([req.method, req.url, switchHeaders]));
  hash.update(body);
  return { digest: hash.digest(), body };
}

// Forwards a request to the service under its armed fault, if any. `methodHits` counts the
// request when it is sent on, or when the fault refuses it. The request the fault fires on,
// and any later one with its method and raw target, are read whole first and fingerprinted,
// so that a resend of the faulted request is counted (see the header).
function forwardWithFault(req, res, path, methodHits, firstOnConnection) {
  const armed = takeFault(req, path, firstOnConnection);
  const maybeResend =
    armed === null && faultedRequest?.method === req.method && faultedRequest.target === req.url;
  if (armed === null && !maybeResend) return forward(req, res, methodHits);
  if (armed?.kind === "refuse") methodHits.refused += 1;
  const epoch = faultEpoch;
  fingerprint(req).then(
    (print) => {
      if (res.destroyed) return;
      if (print === null) return text(res, 413, "body too large");
      if (armed === null) {
        if (epoch === faultEpoch && faultedRequest?.digest.equals(print.digest)) {
          faultResends += 1;
          log(`resend ${req.method} ${path}`);
        }
        return forward(req, res, methodHits, false, print.body);
      }
      if (epoch === faultEpoch) {
        faultedRequest = { method: req.method, target: req.url, digest: print.digest };
      }
      if (armed.kind === "refuse") return cut(res);
      if (armed.kind === "lose") return forward(req, res, methodHits, true, print.body);
      const timer = setTimeout(() => forward(req, res, methodHits, false, print.body), armed.ms);
      res.on("close", () => clearTimeout(timer));
    },
    () => {
      if (!res.destroyed) cut(res);
    },
  );
}

// Sends the request on to the service, counted in `methodHits.forwarded`, with `body` when the
// rig read it already, or else streaming it from the browser. With `lose`, the service's
// whole answer is read, so that it has acted, and the browser's connection is destroyed
// without a byte of it.
function forward(req, res, methodHits, lose = false, body = undefined) {
  methodHits.forwarded += 1;
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
    if (lose) {
      reply.resume();
      return finished(reply, () => cut(res));
    }
    const head = [...endToEndHeaders(reply.rawHeaders), ...corsHeaders(req)];
    res.writeHead(reply.statusCode, reply.statusMessage, head.flat());
    pipeline(reply, res, (error) => {
      if (error) {
        log(`upstream answer broke off: ${error.message}`);
        cut(res);
      }
    });
  });
  upstream.on("error", (error) => {
    log(`upstream: ${error.message}`);
    cut(res);
  });
  if (body !== undefined) return upstream.end(body);
  pipeline(req, upstream, (error) => {
    if (error) upstream.destroy();
  });
}

// The CORS headers of a preflight, with the access-control-max-age its query asks for.
function preflightHeaders(query) {
  const maxAge = ["none", "0"].includes(query.get("max_age")) ? query.get("max_age") : "600";
  if (maxAge === "none") return PREFLIGHT_HEADERS;
  return [...PREFLIGHT_HEADERS, ["access-control-max-age", maxAge]];
}

function handle(req, res) {
  const target = req.url ?? "";
  const queryStart = target.indexOf("?");
  const path = queryStart < 0 ? target : target.slice(0, queryStart);
  const query = new URLSearchParams(queryStart < 0 ? "" : target.slice(queryStart + 1));
  inFlight.add(res);
  res.on("close", () => {
    inFlight.delete(res);
    const code = res.headersSent ? res.statusCode : "no answer";
    log(`${req.method} ${path} ${code}${res.writableFinished ? "" : " (connection closed early)"}`);
  });

  // While a fault is armed, no connection is kept for a next request, and a fault fires only
  // on the first request of a connection (see the header).
  if (armedFault !== null) res.shouldKeepAlive = false;
  const firstOnConnection = !usedConnections.has(req.socket);
  usedConnections.add(req.socket);

  // Decided on the raw path, so no spelling of a /rig/ path reaches the service.
  const forRig = path === "/rig" || path.startsWith("/rig/");
  const tags = path === "/rig/count" ? [] : [...new Set(query.getAll("tag"))];
  const counts = tags.map((tag) => entry(tagCounts, tag, newTagCount));
  for (const tagCount of counts) tagCount.requests += 1;
  const pathHit = forRig
    ? null
    : entry(pathHits, path, () => ({ preflights: 0, methods: new Map() }));
  if (req.method === "OPTIONS") {
    for (const tagCount of counts) tagCount.preflights += 1;
    if (pathHit !== null) pathHit.preflights += 1;
    req.resume();
    res.writeHead(204, corsHeaders(req, preflightHeaders(query)).flat());
    return res.end();
  }

  // Every other request is open until its connection closes: early when the client closes it
  // before the answer is finished, unless the rig destroyed it itself.
  const open = [...counts];
  let methodHits = null;
  if (pathHit !== null) {
    methodHits = entry(pathHit.methods, req.method, () => ({ forwarded: 0, refused: 0, open: 0 }));
    open.push(methodHits);
  }
  for (const counter of open) counter.open += 1;
  res.on("close", () => {
    for (const counter of open) counter.open -= 1;
    if (res.writableFinished || cutByRig.has(res)) return;
    for (const tagCount of counts) tagCount.closed_early += 1;
  });

  if (forRig) return serveRig(req, res, path, query);
  forwardWithFault(req, res, path, methodHits, firstOnConnection);
}

// One server per loopback address, with the same certificate and handler.
function serve(tls, address) {
  return new Promise((resolve, reject) => {
    const server = https.createServer(tls, handle);
    server.on("tlsClientError", (error, socket) => {
      log(`TLS client error: ${error.code ?? error.message}`);
      socket.destroy();
    });
    server.once("error", reject);
    server.listen(listenPort, address, () => {
      server.off("error", reject);
      server.on("error", (error) => fail(`serving on ${address}: ${error.message}`));
      servers.push(server);
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
