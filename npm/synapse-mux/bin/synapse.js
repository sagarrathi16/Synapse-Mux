#!/usr/bin/env node

const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const platform = process.platform;
const arch = process.arch;

const isWin = platform === 'win32';
const ext = isWin ? '.exe' : '';
const binaryName = `synapse${ext}`;

// Candidate locations in priority order
const candidates = [
  // 1. Explicit platform-named binary in bin directory (bundled release)
  path.join(__dirname, `synapse-${platform}-${arch}${ext}`),
  // 2. Generic binary name in bin directory
  path.join(__dirname, binaryName),
  // 3. Local cargo workspace target/release (during development)
  path.resolve(__dirname, '..', '..', '..', 'target', 'release', binaryName),
  // 4. Local cargo workspace target/debug (during development)
  path.resolve(__dirname, '..', '..', '..', 'target', 'debug', binaryName),
];

let targetBin = null;
for (const cand of candidates) {
  if (fs.existsSync(cand)) {
    targetBin = cand;
    break;
  }
}

if (!targetBin) {
  console.error(`\x1b[31m✖ Error: Synapse binary not found for ${platform}-${arch}.\x1b[0m`);
  console.error(`Expected prebuilt binary or target/release build.`);
  console.error(`If building from source, run: cargo build --release -p synapse-cli`);
  process.exit(1);
}

const result = spawnSync(targetBin, process.argv.slice(2), {
  stdio: 'inherit',
  env: process.env,
});

if (result.error) {
  console.error(`\x1b[31m✖ Failed to execute ${targetBin}: ${result.error.message}\x1b[0m`);
  process.exit(1);
}

process.exit(result.status ?? 0);
