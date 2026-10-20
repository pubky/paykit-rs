// Run a local Pubky stack for the example app's `local` mode, until killed.
//
// It runs pubky-testnet's own `pubky-testnet` binary: a StaticTestnet with a
// DHT bootstrap node, a pkarr relay, an HTTP relay and one homeserver, all on
// the fixed ports that `FfiPubkyClientConfig.localTestnetHost` expects, and
// signups open without a token. Postgres comes from TEST_PUBKY_CONNECTION_STRING
// or, when that is unset, from a throwaway Docker container.
//
//   node scripts/local-stack.mjs    (or `npm run local-stack` in example/)
//
// Environment:
//   TEST_PUBKY_CONNECTION_STRING  use this Postgres server instead of Docker.
//                                 The testnet creates and drops its own
//                                 ephemeral database on it.
//   DOCKER_HOST                   Docker socket, for example
//                                 unix://$HOME/.colima/default/docker.sock
//   PAYKIT_LOCAL_STACK_BIN        prebuilt pubky-testnet binary to run.
//
// Without PAYKIT_LOCAL_STACK_BIN, the binary is built once with
// `cargo install --locked`, at the pubky-testnet version in the workspace
// Cargo.lock, into <repo>/target/local-stack/.
//
// When everything is up it prints one line:
//   PAYKIT_LOCAL_STACK_READY homeserver=<key> ... startup_ms=<n>
import { execFileSync, spawn } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
  '..',
  '..'
);
// Fixed by pubky-testnet's StaticTestnet and pubky_common::constants::testnet_ports.
const HOMESERVER = '8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo';
const PORTS = {
  dht_bootstrap: 6881,
  pkarr_relay: 15411,
  http_relay: 15412,
  homeserver_http: 6286,
  homeserver_pubky_tls: 6287,
};
const POSTGRES_IMAGE = 'postgres:17-alpine';
// The binary logs this once the homeserver is listening.
const READY_LOG = 'Homeserver Pubky HTTPS';

const startedAt = Date.now();
let postgresContainer;
let testnet;
let stopping = false;

function log(message) {
  console.error(`[local-stack] ${message}`);
}

function pubkyTestnetVersion() {
  const lock = readFileSync(path.join(repoRoot, 'Cargo.lock'), 'utf8');
  const match = lock.match(/name = "pubky-testnet"\nversion = "([^"]+)"/);
  if (!match) {
    throw new Error('pubky-testnet is not in Cargo.lock');
  }
  return match[1];
}

function testnetBinary() {
  if (process.env.PAYKIT_LOCAL_STACK_BIN) {
    return process.env.PAYKIT_LOCAL_STACK_BIN;
  }
  const version = pubkyTestnetVersion();
  const root = path.join(repoRoot, 'target', 'local-stack', version);
  const binary = path.join(root, 'bin', 'pubky-testnet');
  if (!existsSync(binary)) {
    log(`building pubky-testnet ${version} (once)`);
    execFileSync(
      'cargo',
      [
        'install',
        'pubky-testnet',
        '--version',
        `=${version}`,
        '--locked',
        '--root',
        root,
        '--target-dir',
        path.join(repoRoot, 'target', 'local-stack', 'build'),
      ],
      { stdio: ['ignore', 'inherit', 'inherit'] }
    );
  }
  return binary;
}

function docker(args) {
  return execFileSync('docker', args, { encoding: 'utf8' }).trim();
}

async function startPostgres() {
  const name = `paykit-rn-local-stack-postgres-${process.pid}`;
  // Host port 0: Docker picks a free port, so an existing Postgres on 5432
  // is left alone.
  docker([
    'run',
    '--detach',
    '--rm',
    '--name',
    name,
    '--env',
    'POSTGRES_PASSWORD=postgres',
    '--publish',
    '127.0.0.1:0:5432',
    POSTGRES_IMAGE,
  ]);
  postgresContainer = name;
  const port = docker(['port', name, '5432/tcp']).split(':').pop();
  for (let attempt = 0; attempt < 60; attempt++) {
    try {
      docker([
        'exec',
        name,
        'pg_isready',
        '--host',
        '127.0.0.1',
        '-U',
        'postgres',
      ]);
      return `postgres://postgres:postgres@127.0.0.1:${port}/postgres`;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
  }
  throw new Error('Postgres did not become ready');
}

function stopPostgres() {
  if (postgresContainer) {
    try {
      docker(['rm', '--force', postgresContainer]);
    } catch (error) {
      log(`could not remove ${postgresContainer}: ${error.message}`);
    }
    postgresContainer = undefined;
  }
}

function stop(code) {
  if (stopping) {
    return;
  }
  stopping = true;
  const finish = () => {
    stopPostgres();
    process.exit(code);
  };
  if (!testnet || testnet.exitCode !== null) {
    finish();
    return;
  }
  // SIGINT lets the testnet drop its ephemeral database before exiting.
  testnet.once('exit', finish);
  testnet.kill('SIGINT');
  setTimeout(() => testnet.kill('SIGKILL'), 10_000).unref();
}

process.on('SIGINT', () => stop(0));
process.on('SIGTERM', () => stop(0));

try {
  const binary = testnetBinary();
  const connectionString =
    process.env.TEST_PUBKY_CONNECTION_STRING ?? (await startPostgres());
  const buildMs = Date.now() - startedAt;
  const launchedAt = Date.now();

  testnet = spawn(binary, [], {
    env: {
      ...process.env,
      NO_COLOR: '1',
      TEST_PUBKY_CONNECTION_STRING: connectionString,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let ready = false;
  // Keep a tail of the output, so a log line split across chunks still matches.
  let tail = '';
  const watch = (chunk) => {
    process.stderr.write(chunk);
    tail = (tail + chunk.toString()).slice(-4096);
    if (!ready && tail.includes(READY_LOG)) {
      ready = true;
      const fields = Object.entries({
        homeserver: HOMESERVER,
        ...Object.fromEntries(
          Object.entries(PORTS).map(([name, port]) => [
            name,
            `127.0.0.1:${port}`,
          ])
        ),
        startup_ms: Date.now() - launchedAt,
        setup_ms: buildMs,
      }).map(([key, value]) => `${key}=${value}`);
      console.log(`PAYKIT_LOCAL_STACK_READY ${fields.join(' ')}`);
    }
  };
  testnet.stdout.on('data', watch);
  testnet.stderr.on('data', watch);
  testnet.on('exit', (code, signal) => {
    if (!stopping) {
      log(`pubky-testnet exited (${signal ?? code}) before it was stopped`);
      stop(1);
    }
  });
} catch (error) {
  log(error.message);
  stop(1);
}
