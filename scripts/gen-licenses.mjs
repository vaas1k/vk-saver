// Usage: node scripts/gen-licenses.mjs
// Needs crate sources in ~/.cargo/registry/src (run `cargo fetch` in src-tauri first) and node_modules (npm ci).
// Writes THIRD-PARTY-NOTICES.md (every crate in Cargo.lock) and THIRD-PARTY-LICENSES.txt (crates of the Windows build).
import assert from 'node:assert';
import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const repo = path.resolve(import.meta.dirname, '..');
const reg = path.join(os.homedir(), '.cargo/registry/src');
const regDirs = fs.readdirSync(reg).map((d) => path.join(reg, d));
const LICENSE_FILE = /^(licen[cs]e|copying|notice|unlicense)/i;

function crateDir(name, version) {
  return regDirs.map((d) => path.join(d, `${name}-${version}`)).find((d) => fs.existsSync(d));
}

function licenseFiles(dir) {
  const out = [];
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    if (!LICENSE_FILE.test(e.name)) continue;
    const p = path.join(dir, e.name);
    if (e.isFile()) out.push([e.name, p]);
    else if (e.isDirectory()) for (const f of fs.readdirSync(p)) if (fs.statSync(path.join(p, f)).isFile()) out.push([`${e.name}/${f}`, path.join(p, f)]);
  }
  return out.sort().map(([name, p]) => [name, fs.readFileSync(p, 'utf8').replace(/\r\n/g, '\n').trim()]);
}

const crates = [];
for (const block of fs.readFileSync(path.join(repo, 'src-tauri/Cargo.lock'), 'utf8').split('[[package]]').slice(1)) {
  const get = (k) => block.match(new RegExp(`^${k} = "([^"]*)"`, 'm'))?.[1];
  if (!get('source')) continue;
  const name = get('name'), version = get('version');
  const dir = crateDir(name, version);
  let license = 'not checked: crate source not downloaded locally';
  if (dir) {
    const t = fs.readFileSync(path.join(dir, 'Cargo.toml'), 'utf8');
    license = t.match(/^license = "([^"]*)"/m)?.[1] ?? (t.match(/^license-file = /m) ? 'see license file' : 'UNKNOWN');
  }
  crates.push({ name, version, license, dir });
}

const lock = JSON.parse(fs.readFileSync(path.join(repo, 'package-lock.json'), 'utf8'));
const npm = [];
for (const [p, v] of Object.entries(lock.packages)) {
  if (!p || v.dev) continue;
  const dir = path.join(repo, p);
  const pj = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
  const license = typeof pj.license === 'string' ? pj.license : pj.license?.type ?? 'UNKNOWN';
  npm.push({ name: pj.name, version: pj.version, license, dir });
}

const table = (rows) => ['| Name | Version | License |', '|---|---|---|', ...rows.toSorted((a, b) => a.name.localeCompare(b.name)).map((r) => `| ${r.name} | ${r.version} | ${r.license} |`)].join('\n');
const mpl = [...new Set(crates.filter((c) => c.license.includes('MPL')).map((c) => c.name))].sort();
fs.writeFileSync(path.join(repo, 'THIRD-PARTY-NOTICES.md'), `# Third-party notices

vk-saver is MIT licensed (see LICENSE). It is built from the third-party packages below, each under its own license as declared by the package. The list is generated from \`src-tauri/Cargo.lock\` and \`package-lock.json\`; it covers every crate in the lock file for all target platforms and the npm packages bundled into the window (development tools are not listed). The full license texts of the packages in the Windows build are in THIRD-PARTY-LICENSES.txt.

The crates under the Mozilla Public License 2.0 (${mpl.join(', ')}) are used unchanged; their source code is available from crates.io (https://crates.io/crates/<name>) at the versions listed.

## Rust crates

${table(crates)}

## npm packages

${table(npm)}
`);

const tree = execFileSync('cargo', ['tree', '-p', 'vk-saver', '--target', 'x86_64-pc-windows-msvc', '-e', 'normal,build', '--prefix', 'none', '--offline'], { cwd: path.join(repo, 'src-tauri'), encoding: 'utf8' });
const inBuild = new Set(tree.split('\n').filter((l) => l && !l.includes('(/')).map((l) => l.replace(/ \(.*$/, '').replace(' v', '@')));
const pkgs = [...crates.filter((c) => inBuild.has(`${c.name}@${c.version}`)), ...npm];
assert.equal(pkgs.length - npm.length, inBuild.size, 'every crate of the Windows build must be in Cargo.lock');

const groups = new Map();
const missing = [];
const standard = new Map();
for (const p of pkgs) {
  assert(p.dir, `${p.name} ${p.version}: source not found, run cargo fetch`);
  p.files = licenseFiles(p.dir);
  if (!p.files.length) missing.push(p);
  for (const [file, text] of p.files) {
    const h = crypto.createHash('sha256').update(text).digest('hex');
    if (!groups.has(h)) groups.set(h, { text, users: [] });
    groups.get(h).users.push(`${p.name} ${p.version} (${file})`);
    const id = /mit/i.test(file) ? 'MIT' : /apache/i.test(file) ? 'Apache-2.0' : p.files.length === 1 ? p.license : null;
    if (id && !standard.has(id)) standard.set(id, text);
  }
}

const spdxIds = (expr) => expr.split(/\s+(?:OR|AND|WITH)\s+|[()\/\s]+/).filter(Boolean);
const neededIds = [...new Set(missing.flatMap((p) => spdxIds(p.license)))].sort();
for (const id of neededIds) assert(standard.has(id), `no package ships a standard text for ${id}`);

const rule = '='.repeat(78);
const out = [
  'Third-party licenses',
  '',
  'vk-saver.exe includes the third-party packages listed below. This file reproduces the license, copyright and notice files they ship, grouped by identical text: each text is preceded by the packages that ship it. Source code of the packages under the Mozilla Public License 2.0 is available from crates.io (https://crates.io/crates/<name>) at the versions listed.',
];
for (const { text, users } of [...groups.values()].sort((a, b) => a.users[0].localeCompare(b.users[0]))) {
  out.push('', rule, ...users.sort(), rule, '', text);
}
if (missing.length) {
  out.push('', rule, 'Packages that ship no license file; the standard text of the declared license (SPDX id) applies, see below:', rule, '');
  for (const p of missing.sort((a, b) => a.name.localeCompare(b.name))) out.push(`${p.name} ${p.version}: ${p.license}`);
  for (const id of neededIds) out.push('', rule, `Standard text: ${id}`, rule, '', standard.get(id));
}
const txt = out.join('\n') + '\n';
for (const p of pkgs) assert(txt.includes(`${p.name} ${p.version}`), `${p.name} ${p.version} missing from the output`);
fs.writeFileSync(path.join(repo, 'THIRD-PARTY-LICENSES.txt'), txt);
console.error(`notices: ${crates.length} crates, ${npm.length} npm; licenses: ${pkgs.length} packages, ${groups.size} texts, ${missing.length} without license file (${neededIds.join(', ') || 'none'}), ${txt.length} bytes`);
