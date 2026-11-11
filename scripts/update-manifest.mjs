#!/usr/bin/env node
// Run only after both platform jobs pass. This script does not build or publish.
import fs from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';

const root = path.resolve(import.meta.dirname, '..');
const directory = path.resolve(process.argv[2] || path.join(root, 'dist'));
const { version } = JSON.parse(await fs.readFile(path.join(root, 'package.json'), 'utf8'));
if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)) throw new Error('Нужна стабильная версия X.Y.Z');
if (process.argv.length > 3) throw new Error('Usage: node scripts/update-manifest.mjs [package-directory]');
const names = [`Spark-${version}-linux-x64.tar.gz`, `Spark-${version}-windows-x64.zip`, `Spark-${version}-windows-setup.exe`];
const packageFiles = (await fs.readdir(directory, { withFileTypes: true }))
  .filter(entry => entry.isFile() && /\.(?:AppImage|tar\.gz|exe|zip)$/i.test(entry.name)).map(entry => entry.name).sort();
if (JSON.stringify(packageFiles) !== JSON.stringify([...names].sort())) {
  throw new Error(`Нужны ровно три пакета версии ${version}: ${names.join(', ')}; получены: ${packageFiles.join(', ')}`);
}
async function digest(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}
async function asset(name, signature) {
  const file = path.join(directory, name);
  const stat = await fs.lstat(file);
  if (!stat.isFile() || stat.size <= signature.length) throw new Error(`Пустой/необычный пакет: ${name}`);
  const handle = await fs.open(file, 'r');
  try {
    const bytes = Buffer.alloc(signature.length);
    await handle.read(bytes, 0, bytes.length, 0);
    if (!bytes.equals(signature)) throw new Error(`Неверный формат: ${name}`);
  } finally { await handle.close(); }
  return { name, sha256: await digest(file), size: stat.size };
}
const linux = await asset(names[0], Buffer.from([0x1f, 0x8b]));
const windows = await asset(names[1], Buffer.from('PK\x03\x04', 'binary'));
const installer = await asset(names[2], Buffer.from('MZ'));
const manifest = { version, platforms: { 'linux-x64': linux, 'win32-x64': windows }, windowsInstaller: installer };
await fs.writeFile(path.join(directory, 'spark-update.json'), JSON.stringify(manifest, null, 2) + '\n');
const checksums = [];
for (const name of [...names, 'spark-update.json'].sort()) checksums.push(`${await digest(path.join(directory, name))}  ${name}`);
await fs.writeFile(path.join(directory, 'SHA256SUMS'), checksums.join('\n') + '\n');
console.log(`Spark ${version}: spark-update.json и SHA256SUMS созданы из трёх пакетов.`);
