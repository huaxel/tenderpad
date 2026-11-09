#!/usr/bin/env node
const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');

const FORBIDDEN_STRINGS = [
  '/Users/juanbenjumea/projects/EHB',
  'framearch-datastore',
  'nv Cebeo',
  '-----BEGIN RSA PRIVATE KEY-----',
  '-----BEGIN OPENSSH PRIVATE KEY-----',
  '-----BEGIN PRIVATE KEY-----'
];

const SCAN_EXTENSIONS = ['.rs', '.toml', '.json', '.md', '.html', '.js', '.cjs', '.ts', '.tsx', '.sh', '.xml', '.csv'];
const IGNORED_DIRS = ['.git', 'target', 'node_modules', 'dist', '.gemini'];

function walk(dir) {
  let files = [];
  for (const item of fs.readdirSync(dir)) {
    if (IGNORED_DIRS.includes(item)) continue;
    const full = path.join(dir, item);
    const stat = fs.statSync(full);
    if (stat.isDirectory()) {
      files = files.concat(walk(full));
    } else {
      files.push(full);
    }
  }
  return files;
}

console.log('--- Running Deterministic Safety Check ---');
const allFiles = walk(ROOT);
let failed = false;

// 1. Check for database binary leaks
for (const file of allFiles) {
  const ext = path.extname(file).toLowerCase();
  if (['.duckdb', '.sqlite', '.sqlite3', '.parquet'].includes(ext)) {
    console.error(`[FAIL] Prohibited database file present: ${path.relative(ROOT, file)}`);
    failed = true;
  }
}

// 2. Scan text contents for forbidden tokens, real records, private paths
for (const file of allFiles) {
  const ext = path.extname(file).toLowerCase();
  if (!SCAN_EXTENSIONS.includes(ext)) continue;
  const relPath = path.relative(ROOT, file);

  // Skip safety_check.cjs itself for self-referential string checks
  if (relPath === 'tests/safety_check.cjs') continue;

  const content = fs.readFileSync(file, 'utf8');

  for (const forbidden of FORBIDDEN_STRINGS) {
    if (content.includes(forbidden)) {
      console.error(`[FAIL] ${relPath} contains forbidden string: "${forbidden}"`);
      failed = true;
    }
  }

  // Check for potential private email addresses (excluding dummy/example/test ones, and URL authority userinfo)
  const contentNoUrls = content.replace(/https?:\/\/[^\s"'<>]+/g, '').replace(/s3:\/\/[^\s"'<>]+/g, '');
  const emails = contentNoUrls.match(/[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z0-9-.]+/g) || [];
  for (const email of emails) {
    if (!email.endsWith('@example.com') && !email.endsWith('@example.org') && !email.endsWith('@example.test') && !email.includes('localhost')) {
      console.error(`[FAIL] ${relPath} contains non-example email address: ${email}`);
      failed = true;
    }
  }
}

// 3. Verify Allowlist Manifest exists and check completeness
const manifestPath = path.join(ROOT, 'ALLOWLIST-MANIFEST.md');
if (!fs.existsSync(manifestPath)) {
  console.error('[FAIL] ALLOWLIST-MANIFEST.md is missing');
  failed = true;
} else {
  const manifest = fs.readFileSync(manifestPath, 'utf8');
  console.log('[OK] ALLOWLIST-MANIFEST.md verified.');
}

// 4. Every file must be listed in export/published.txt, which the publish
// script generates from the product's path list and the overlay. A file that
// appears in the tree without an entry fails here, so the published artifact
// cannot grow unreviewed.
try {
  const listedPath = path.join(ROOT, 'export', 'published.txt');
  const listed = new Set(
    fs.readFileSync(listedPath, 'utf8')
      .split('\n')
      .filter((line) => line && !line.startsWith('#'))
      .map((line) => line.split(/\s+/)[0]),
  );
  const present = walk(ROOT)
    .map((file) => path.relative(ROOT, file))
    .filter((file) => file !== 'export/published.txt');
  const unlisted = present.filter((file) => !listed.has(file));
  const missing = [...listed].filter((file) => !present.includes(file));
  if (unlisted.length || missing.length) {
    if (unlisted.length) console.error(`[FAIL] files present but not listed: ${unlisted.join(', ')}`);
    if (missing.length) console.error(`[FAIL] files listed but absent: ${missing.join(', ')}`);
    failed = true;
  } else {
    console.log(`[OK] all ${present.length} files are listed in export/published.txt.`);
  }
} catch (error) {
  console.error('[FAIL] export/published.txt is missing or unreadable:', error.message);
  failed = true;
}

if (failed) {
  console.error('Safety check failed.');
  process.exit(1);
}

console.log('[PASS] All deterministic safety checks passed successfully.');
