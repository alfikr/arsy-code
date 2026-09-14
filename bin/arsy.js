#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const { resolve } = require('node:path');

const platformPackages = {
  'darwin-arm64': '@suiflex/arsy-code-darwin-arm64',
  'darwin-x64': '@suiflex/arsy-code-darwin-x64',
  'linux-arm64': '@suiflex/arsy-code-linux-arm64-gnu',
  'linux-x64': '@suiflex/arsy-code-linux-x64-gnu',
  'win32-x64': '@suiflex/arsy-code-win32-x64-msvc'
};

const platformKey = `${process.platform}-${process.arch}`;
const packageName = platformPackages[platformKey];

if (!packageName) {
  console.error(
    `ARSY CODE does not provide a native npm package for ${platformKey}. ` +
      'Supported targets: macOS arm64/x64, Linux arm64/x64 (glibc), and Windows x64.'
  );
  process.exit(1);
}

const binaryName = process.platform === 'win32' ? 'arsy.exe' : 'arsy';
let binaryPath;
try {
  binaryPath = require.resolve(`${packageName}/bin/${binaryName}`, {
    paths: [resolve(__dirname, '..')]
  });
} catch {
  console.error(
    `The native package ${packageName} is not installed. ` +
      'Reinstall @suiflex/arsy-code for the current platform.'
  );
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), { stdio: 'inherit' });

if (result.error) {
  console.error(`Unable to start ARSY CODE: ${result.error.message}`);
  process.exit(1);
}

if (result.signal) {
  const signalExitCodes = { SIGINT: 130, SIGTERM: 143 };
  process.exit(signalExitCodes[result.signal] || 1);
}

process.exit(result.status ?? 1);
