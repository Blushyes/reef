'use strict';

const { spawnSync } = require('child_process');
const { existsSync, realpathSync } = require('fs');
const path = require('path');

function runUpdate() {
  // Windows npm is a .cmd launcher. All arguments passed to its shell are fixed.
  const npm = process.platform === 'win32' ? 'npm.cmd' : 'npm';
  const shell = process.platform === 'win32';
  const root = spawnSync(npm, ['root', '--global'], {
    shell,
    encoding: 'utf8',
    stdio: ['inherit', 'pipe', 'inherit'],
  });
  if (root.error) {
    console.error(`reef: cannot run npm: ${root.error.message}`);
    return 1;
  }
  if (root.status !== 0) return root.status ?? 1;

  const globalPackage = path.join(root.stdout.trim(), '@reef-tui', 'cli');
  const thisPackage = realpathSync(path.join(__dirname, '..'));
  if (!existsSync(globalPackage) || realpathSync(globalPackage) !== thisPackage) {
    console.error('reef: this launcher is not in the active npm global installation.');
    console.error('For a global install, activate its Node/npm environment and retry.');
    console.error('For a local install, run npm install @reef-tui/cli@latest in that project.');
    console.error('For a temporary run, use npx @reef-tui/cli@latest.');
    return 1;
  }

  console.log('Running: npm install --global @reef-tui/cli@latest');
  const result = spawnSync(npm, ['install', '--global', '@reef-tui/cli@latest'], {
    shell,
    stdio: 'inherit',
  });
  if (result.error) {
    console.error(`reef: cannot run npm: ${result.error.message}`);
    return 1;
  }
  if (result.status === 0) {
    console.log('Reef update completed. Run reef to start the installed version.');
  }
  return result.status ?? 1;
}

module.exports = { runUpdate };
