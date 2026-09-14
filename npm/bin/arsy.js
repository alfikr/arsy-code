#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const binaryName = process.platform === 'win32' ? 'arsy.exe' : 'arsy';
const binaryPath = path.join(__dirname, '..', 'vendor', binaryName);

if (!fs.existsSync(binaryPath)) {
  console.error('The ARSY CODE binary is missing. Reinstall @suiflex/arsy-code.');
  process.exit(1);
}

const result = spawnSync(binaryPath, process.argv.slice(2), {
  stdio: 'inherit',
  windowsHide: false
});

if (result.error) {
  console.error(`Unable to start ARSY CODE: ${result.error.message}`);
  process.exit(1);
}

if (result.signal) {
  const signalExitCodes = { SIGINT: 130, SIGTERM: 143 };
  process.exit(signalExitCodes[result.signal] || 1);
}

process.exit(result.status ?? 1);
