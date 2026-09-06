// Run with: docker compose run --rm --no-deps adjutant
//   node /usr/local/lib/adjutant/check-codex-sandbox.mjs
// No login, model request, or production connection is needed. Temporary files are cleaned up.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

assert.equal(process.getuid(), 10001, 'run through the image entrypoint as UID 10001');
const directory = mkdtempSync(join(tmpdir(), 'adjutant-sandbox-check-'));
// Codex refuses helper aliases under /tmp. Use a fresh private home inside its writable volume.
assert.ok(process.env.CODEX_HOME, 'CODEX_HOME must name the image Codex volume');
const codexHome = mkdtempSync(join(process.env.CODEX_HOME, 'sandbox-check-'));
const listener = createServer(socket => socket.end());
const fixture = 'adjutant sandbox fixture';
const environment = {
  PATH: process.env.PATH,
  HOME: directory,
  CODEX_HOME: codexHome,
  LANG: 'C.UTF-8',
};

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: directory,
    env: environment,
    encoding: 'utf8',
    timeout: 30_000,
    killSignal: 'SIGKILL',
    maxBuffer: 64 * 1024,
  });
  assert.ifError(result.error);
  assert.equal(result.status, 0,
    `${command} failed (${result.signal ?? result.status}):\n${result.stdout}\n${result.stderr}`);
  return result.stdout;
}

try {
  writeFileSync(join(directory, 'fixture'), fixture);
  await new Promise((resolve, reject) => {
    listener.once('error', reject);
    listener.listen(0, '127.0.0.1', resolve);
  });
  const port = String(listener.address().port);
  // A positive control proves that this network is available outside Codex's sandbox.
  // Only connect to our own temporary listener, never a production endpoint.
  run('timeout', ['5s', 'bash', '-c', 'exec 3<>/dev/tcp/127.0.0.1/"$1"', 'check', port]);
  const probe = `
set -eu
[ "$(id -u)" = 10001 ]
[ "$(cat "$1/fixture")" = 'adjutant sandbox fixture' ]
git --version >/dev/null
rg --fixed-strings --quiet 'adjutant sandbox fixture' "$1/fixture"
minidump-stackwalk --version >/dev/null
if (printf changed > "$1/fixture") 2>/dev/null; then
  echo 'FAIL: sandbox allowed overwriting a file' >&2; exit 1
fi
if (printf created > "$1/new-file") 2>/dev/null; then
  echo 'FAIL: sandbox allowed creating a file' >&2; exit 1
fi
if chmod 600 "$1/fixture" 2>/dev/null; then
  echo 'FAIL: sandbox allowed changing file permissions' >&2; exit 1
fi
if rm "$1/fixture" 2>/dev/null; then
  echo 'FAIL: sandbox allowed deleting a file' >&2; exit 1
fi
if timeout 5s bash -c 'exec 3<>/dev/tcp/127.0.0.1/"$1"' check "$2" 2>/dev/null; then
  echo 'FAIL: sandbox could reach the container network' >&2; exit 1
fi
printf 'sandbox checks passed: reads work, writes and networking are blocked\\n'
`;
  const output = run('timeout', ['--kill-after=2s', '20s', 'codex', 'sandbox',
    '-c', 'sandbox_mode="read-only"', '--disable', 'use_legacy_landlock',
    '--', 'sh', '-c', probe, 'check', directory, port]);
  assert.equal(readFileSync(join(directory, 'fixture'), 'utf8'), fixture);
  assert.equal(output.trim(), 'sandbox checks passed: reads work, writes and networking are blocked',
    'sandbox command did not finish every check');
  console.log(output.trim());
} finally {
  listener.close();
  // This is the fresh directory created by this invocation, never a deployment volume.
  rmSync(directory, { recursive: true, force: true });
  rmSync(codexHome, { recursive: true, force: true });
}
