'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { test } = require('node:test');

function installation(t, options = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'reef update '));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const modules = path.join(root, 'node_modules');
  const packageDir = path.join(modules, '@reef-tui', 'cli');
  fs.mkdirSync(packageDir, { recursive: true });
  fs.cpSync(path.join(__dirname, '..', 'bin'), path.join(packageDir, 'bin'), { recursive: true });
  const tools = path.join(root, 'tools');
  fs.mkdirSync(tools);
  const log = path.join(root, 'npm.log');
  const fakeNpm = path.join(tools, 'npm.js');
  fs.writeFileSync(fakeNpm, `
const fs = require('node:fs');
const args = process.argv.slice(2);
fs.appendFileSync(process.env.REEF_TEST_LOG, JSON.stringify(args) + '\\n');
if (args[0] === 'root') {
  console.log(process.env.REEF_TEST_ROOT);
  process.exit(Number(process.env.REEF_TEST_ROOT_STATUS));
}
process.exit(Number(process.env.REEF_TEST_INSTALL_STATUS));
`);
  if (process.platform === 'win32') {
    fs.writeFileSync(path.join(tools, 'npm.cmd'), `@"${process.execPath}" "${fakeNpm}" %*\r\n`);
  } else {
    fs.writeFileSync(path.join(tools, 'npm'), `#!/bin/sh\nexec "${process.execPath}" "${fakeNpm}" "$@"\n`, { mode: 0o755 });
  }
  const env = {
    ...process.env,
    PATH: tools + path.delimiter + process.env.PATH,
    REEF_TEST_LOG: log,
    REEF_TEST_ROOT: options.otherRoot ? path.join(root, 'other-node-modules') : modules,
    REEF_TEST_ROOT_STATUS: String(options.rootStatus || 0),
    REEF_TEST_INSTALL_STATUS: String(options.installStatus || 0),
  };
  // Node treats Windows environment keys case-insensitively; pass a single PATH.
  for (const key of Object.keys(env)) {
    if (key !== 'PATH' && key.toLowerCase() === 'path') delete env[key];
  }
  return {
    run: () => spawnSync(process.execPath, [path.join(packageDir, 'bin', 'reef.js'), 'update'], { env, encoding: 'utf8' }),
    calls: () => fs.readFileSync(log, 'utf8').trim().split('\n').map(line => JSON.parse(line)),
  };
}

test('global npm updates without starting or resolving a native binary', t => {
  const installed = installation(t);
  const result = installed.run();
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(installed.calls(), [
    ['root', '--global'],
    ['install', '--global', '@reef-tui/cli@latest'],
  ]);
});

test('local or different-prefix npm installations are not upgraded globally', t => {
  const installed = installation(t, { otherRoot: true });
  const result = installed.run();
  assert.equal(result.status, 1);
  assert.match(result.stderr, /npx @reef-tui\/cli@latest/);
  assert.deepEqual(installed.calls(), [['root', '--global']]);
});

test('npm query failure prevents installation', t => {
  const installed = installation(t, { rootStatus: 23 });
  assert.equal(installed.run().status, 23);
  assert.deepEqual(installed.calls(), [['root', '--global']]);
});

test('npm install failure is returned to the caller', t => {
  const installed = installation(t, { installStatus: 17 });
  assert.equal(installed.run().status, 17);
});
