#!/usr/bin/env node
import { chmod, copyFile, mkdir, rm, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const targets = {
  'aarch64-apple-darwin': {
    name: '@suiflex/arsy-code-darwin-arm64',
    os: ['darwin'],
    cpu: ['arm64'],
    binary: 'arsy'
  },
  'x86_64-apple-darwin': {
    name: '@suiflex/arsy-code-darwin-x64',
    os: ['darwin'],
    cpu: ['x64'],
    binary: 'arsy'
  },
  'aarch64-unknown-linux-gnu': {
    name: '@suiflex/arsy-code-linux-arm64-gnu',
    os: ['linux'],
    cpu: ['arm64'],
    binary: 'arsy'
  },
  'x86_64-unknown-linux-gnu': {
    name: '@suiflex/arsy-code-linux-x64-gnu',
    os: ['linux'],
    cpu: ['x64'],
    binary: 'arsy'
  },
  'x86_64-pc-windows-msvc': {
    name: '@suiflex/arsy-code-win32-x64-msvc',
    os: ['win32'],
    cpu: ['x64'],
    binary: 'arsy.exe'
  }
};

function readOption(name) {
  const index = process.argv.indexOf(`--${name}`);
  if (index === -1 || !process.argv[index + 1]) {
    throw new Error(`missing --${name}`);
  }
  return process.argv[index + 1];
}

const target = readOption('target');
const binary = resolve(readOption('binary'));
const version = readOption('version');
const output = resolve(readOption('output'));
const metadata = targets[target];

if (!metadata) {
  throw new Error(`unsupported Rust target: ${target}`);
}
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`invalid npm version: ${version}`);
}

const binaryOutput = resolve(output, 'bin', metadata.binary);
const packageJson = {
  name: metadata.name,
  version,
  description: 'Native ARSY CODE executable for this platform.',
  license: 'MIT',
  repository: {
    type: 'git',
    url: 'git+https://github.com/suiflex/arsy-code.git'
  },
  os: metadata.os,
  cpu: metadata.cpu,
  files: ['bin'],
  publishConfig: {
    access: 'public'
  }
};

await rm(output, { recursive: true, force: true });
await mkdir(resolve(output, 'bin'), { recursive: true });
await copyFile(binary, binaryOutput);
if (metadata.binary === 'arsy') {
  await chmod(binaryOutput, 0o755);
}
await writeFile(resolve(output, 'package.json'), `${JSON.stringify(packageJson, null, 2)}\n`);
console.log(`Staged ${metadata.name}@${version} from ${binary}`);
