#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import fs from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';
import { createRequire } from 'node:module';
import { randomUUID } from 'node:crypto';

const root = path.resolve(import.meta.dirname, '..');
if (process.platform !== 'linux') throw new Error('Linux-пакет нужно собирать на Linux');
const { version } = JSON.parse(await fs.readFile(path.join(root, 'package.json'), 'utf8'));
if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error('Нужна стабильная версия X.Y.Z');
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: 'inherit', shell: false });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} завершился с кодом ${result.status ?? result.signal}`);
}

run(process.execPath, [createRequire(import.meta.url).resolve('electron-builder/cli.js'), '--linux', '--dir', '--x64']);
const unpacked = path.join(root, 'dist/linux-unpacked');
for (const name of ['spark-lan', 'resources/bin/spark-service', 'resources/app.asar',
  'resources/ui/index.html', 'resources/ui/pkg/spark_ui.js', 'resources/ui/pkg/spark_ui_bg.wasm']) {
  if (!(await fs.stat(path.join(unpacked, name))).isFile()) throw new Error(`В пакете нет ${name}`);
}
const asar = createRequire(import.meta.url)('@electron/asar');
if (JSON.parse(asar.extractFile(path.join(unpacked, 'resources/app.asar'), 'package.json')).version !== version) {
  throw new Error('Версия упакованного приложения не совпадает с package.json');
}
await fs.writeFile(path.join(unpacked, 'spark'), '#!/bin/sh\nset -eu\nspark_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\nexec "$spark_dir/spark-lan" "$@"\n', { mode: 0o755 });
await fs.chmod(path.join(unpacked, 'spark'), 0o755);
const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-linux-package-'));
const destination = path.join(root, `dist/Spark-${version}-linux-x64.tar.gz`);
const partial = `${destination}.partial-${randomUUID()}`;
try {
  await fs.cp(unpacked, path.join(temporary, 'Spark'), { recursive: true, preserveTimestamps: true });
  run('tar', ['-czf', partial, '-C', temporary, 'Spark']);
  await fs.rename(partial, destination);
  console.log(`Linux: ${destination} (папка Spark/, запуск ./Spark/spark)`);
} finally {
  await fs.rm(temporary, { recursive: true, force: true });
  await fs.rm(partial, { force: true });
}
