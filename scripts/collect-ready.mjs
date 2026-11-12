#!/usr/bin/env node
// Collect already built and verified packages; never build or publish here.
import fs from 'node:fs/promises';
import { createReadStream, createWriteStream } from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { createHash, randomUUID } from 'node:crypto';
import { createRequire } from 'node:module';
import { pipeline } from 'node:stream/promises';

const root = path.resolve(import.meta.dirname, '..');
const usage = `Сбор готовых пакетов обеих платформ без сборки и публикации.

node scripts/collect-ready.mjs [--windows-input DIR] [--linux-input DIR]
                              [--output DIR] [--replace]

По умолчанию: Windows — dist, Linux — dist/linux-unpacked,
результат — Готовые файлы. Вместо --windows-input можно указать DIR первым
аргументом. Windows-каталог может содержать скачанные артефакты CI во вложенных
папках. Принимаются только установщик и ZIP текущей версии из package.json.
--replace заменяет существующий комплект, созданный этим скриптом.`;

function options(args) {
  const result = { windows: path.join(root, 'dist'), linux: path.join(root, 'dist/linux-unpacked'),
    output: path.join(root, 'Готовые файлы'), replace: false };
  let positional = false;
  for (let i = 0; i < args.length; i++) {
    const arg = args[i];
    if (arg === '--help' || arg === '-h') { console.log(usage); process.exit(0); }
    if (arg === '--replace') { result.replace = true; continue; }
    const key = { '--windows-input': 'windows', '--linux-input': 'linux', '--output': 'output' }[arg];
    if (key) {
      if (!args[i + 1] || args[i + 1].startsWith('--')) throw new Error(`После ${arg} нужен каталог`);
      result[key] = path.resolve(args[++i]);
    } else if (!arg.startsWith('-') && !positional) {
      result.windows = path.resolve(arg); positional = true;
    } else throw new Error(`Неизвестный аргумент: ${arg}\n${usage}`);
  }
  return result;
}

function within(parent, child) {
  const relative = path.relative(parent, child);
  return relative === '' || (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative));
}

async function isPresent(file) {
  try { await fs.lstat(file); return true; } catch (error) { if (error.code === 'ENOENT') return false; throw error; }
}

async function directory(file) {
  if (!(await fs.lstat(file)).isDirectory()) throw new Error(`Нужен обычный каталог: ${file}`);
  return fs.realpath(file);
}

async function filesIn(dir) {
  const result = [];
  for (const entry of (await fs.readdir(dir, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name))) {
    const file = path.join(dir, entry.name);
    if (entry.isSymbolicLink()) throw new Error(`Символическая ссылка в пакете не поддерживается: ${file}`);
    if (entry.isDirectory()) result.push(...await filesIn(file));
    else if (entry.isFile()) result.push(file);
    else throw new Error(`Необычный тип файла: ${file}`);
  }
  return result;
}

async function readPrefix(file, length) {
  const handle = await fs.open(file, 'r');
  try {
    const buffer = Buffer.alloc(length);
    const { bytesRead } = await handle.read(buffer, 0, length, 0);
    return buffer.subarray(0, bytesRead);
  } finally { await handle.close(); }
}

function magic(bytes, expected, label) {
  if (!bytes.subarray(0, expected.length).equals(expected)) throw new Error(`Неверный формат файла: ${label}`);
}

const wasmMagic = Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]);
const soundNames = ['peer-join', 'peer-leave', 'connect', 'disconnect', 'notification', 'invite', 'mic-on', 'mic-off', 'sound-on', 'sound-off'];
const sharedFiles = ['resources/app.asar', 'resources/ui/index.html', 'resources/ui/style.css',
  'resources/ui/pkg/spark_ui.js', 'resources/ui/pkg/spark_ui_bg.wasm',
  ...soundNames.map(name => `resources/ui/sounds/${name}.wav`)];

function checkWav(bytes, label) {
  magic(bytes, Buffer.from('RIFF'), label);
  if (bytes.subarray(8, 12).toString() !== 'WAVE') throw new Error(`Неверный WAV: ${label}`);
}

async function checkAsar(file, expected, asar) {
  const metadata = JSON.parse(asar.extractFile(file, 'package.json').toString());
  if (metadata.version !== expected.version || metadata.name !== expected.name) {
    throw new Error(`Устаревшая или чужая сборка ${file}: ${metadata.name} ${metadata.version}; нужна ${expected.name} ${expected.version}`);
  }
  for (const entry of ['desktop/main.cjs', 'desktop/preload.cjs']) {
    if (asar.extractFile(file, entry).length === 0) throw new Error(`Пустой ${entry} в ${file}`);
  }
}

async function checkLinux(dir, expected, asar) {
  // Traverse first, so a copied package cannot contain an unnoticed external link.
  await filesIn(dir);
  for (const relative of ['spark-lan', 'resources.pak', 'chrome_100_percent.pak',
    'chrome_200_percent.pak', 'icudtl.dat', 'resources/bin/spark-service', ...sharedFiles]) {
    const file = path.join(dir, relative);
    const stat = await fs.lstat(file);
    if (!stat.isFile() || stat.size === 0) throw new Error(`Отсутствует или пустой файл Linux: ${file}`);
  }
  for (const relative of ['spark-lan', 'resources/bin/spark-service']) {
    const file = path.join(dir, relative);
    magic(await readPrefix(file, 4), Buffer.from([127, 69, 76, 70]), file);
    if (process.platform !== 'win32' && ((await fs.stat(file)).mode & 0o111) === 0) {
      throw new Error(`Нет права запуска: ${file}`);
    }
  }
  const wasm = path.join(dir, 'resources/ui/pkg/spark_ui_bg.wasm');
  magic(await readPrefix(wasm, 8), wasmMagic, wasm);
  for (const name of soundNames) {
    const file = path.join(dir, `resources/ui/sounds/${name}.wav`);
    checkWav(await readPrefix(file, 12), file);
  }
  await checkAsar(path.join(dir, 'resources/app.asar'), expected, asar);
}

async function zipPrefix(entry, length) {
  let result = Buffer.alloc(0);
  for await (const chunk of entry.stream()) {
    result = Buffer.concat([result, chunk.subarray(0, length - result.length)]);
    if (result.length >= length) break;
  }
  return result;
}

async function checkWindows(zip, setup, expected, temp, asar, unzipper) {
  magic(await readPrefix(setup, 2), Buffer.from('MZ'), setup);
  const archive = await unzipper.Open.file(zip);
  const entries = new Map();
  for (const entry of archive.files) {
    const name = entry.path.replaceAll('\\', '/');
    if (name.startsWith('/') || /^[A-Za-z]:/.test(name) || name.split('/').includes('..') || entries.has(name)) {
      throw new Error(`Небезопасный или повторяющийся путь в ZIP: ${entry.path}`);
    }
    entries.set(name, entry);
  }
  const executables = [...entries.keys()].filter(name => /(^|\/)Spark\.exe$/.test(name));
  if (executables.length !== 1) throw new Error(`В ${zip} должен быть ровно один Spark.exe`);
  const prefix = executables[0].slice(0, -'Spark.exe'.length);
  const get = relative => {
    const entry = entries.get(prefix + relative);
    if (!entry || entry.type !== 'File' || entry.uncompressedSize === 0) {
      throw new Error(`Отсутствует или пустой файл Windows ZIP: ${relative}`);
    }
    return entry;
  };
  for (const relative of ['Spark.exe', 'resources.pak', 'resources/bin/spark-service.exe', ...sharedFiles]) get(relative);
  for (const relative of ['Spark.exe', 'resources/bin/spark-service.exe']) {
    magic(await zipPrefix(get(relative), 2), Buffer.from('MZ'), `${zip}: ${relative}`);
  }
  magic(await zipPrefix(get('resources/ui/pkg/spark_ui_bg.wasm'), 8), wasmMagic, `${zip}: WASM`);
  for (const name of soundNames) {
    checkWav(await zipPrefix(get(`resources/ui/sounds/${name}.wav`), 12), `${zip}: ${name}.wav`);
  }
  const asarPath = path.join(temp, 'windows-app.asar');
  await pipeline(get('resources/app.asar').stream(), createWriteStream(asarPath));
  await checkAsar(asarPath, expected, asar);
}

async function sha256(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}

async function collect() {
  const opts = options(process.argv.slice(2));
  const expected = JSON.parse(await fs.readFile(path.join(root, 'package.json'), 'utf8'));
  if (!/^\d+\.\d+\.\d+$/.test(expected.version)) throw new Error('В package.json нужна обычная версия X.Y.Z');
  opts.linux = await directory(opts.linux);
  opts.windows = await directory(opts.windows);
  // Resolve the parent too, to prevent an output symlink from bypassing the overlap checks.
  await fs.mkdir(path.dirname(opts.output), { recursive: true });
  opts.output = path.join(await fs.realpath(path.dirname(opts.output)), path.basename(opts.output));
  if (within(opts.output, root) || [opts.linux, opts.windows].some(input => within(input, opts.output) || within(opts.output, input))) {
    throw new Error('Каталог результата не должен пересекаться с исходниками или входными пакетами');
  }
  const existed = await isPresent(opts.output);
  if (existed) {
    if (!opts.replace) throw new Error(`${opts.output} уже существует; для замены используйте --replace`);
    await directory(opts.output);
    const allowed = new Set(['Windows', 'Linux', 'КАК ЗАПУСТИТЬ.txt', 'SHA256SUMS.txt']);
    const names = await fs.readdir(opts.output);
    if (names.some(name => !allowed.has(name)) || ![...allowed].every(name => names.includes(name))) {
      throw new Error('Отказ от замены: существующий каталог не похож на комплект collect-ready');
    }
  }
  const require = createRequire(import.meta.url);
  const asar = require('@electron/asar');
  const unzipper = require('unzipper');
  const windowsFiles = await filesIn(opts.windows);
  const select = name => {
    const matches = windowsFiles.filter(file => path.basename(file) === name);
    if (matches.length !== 1) throw new Error(`Нужен ровно один ${name} в ${opts.windows}; найдено ${matches.length}. Старые версии не используются.`);
    return matches[0];
  };
  const setup = select(`Spark-${expected.version}-windows-setup.exe`);
  const zip = select(`Spark-${expected.version}-windows-x64.zip`);
  const temp = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-ready-check-'));
  let stage;
  try {
    await checkLinux(opts.linux, expected, asar);
    await checkWindows(zip, setup, expected, temp, asar, unzipper);
    stage = await fs.mkdtemp(path.join(path.dirname(opts.output), '.spark-ready-'));
    const linux = path.join(stage, 'Linux/Spark');
    const windows = path.join(stage, 'Windows');
    await fs.mkdir(windows, { recursive: true });
    await fs.mkdir(path.dirname(linux), { recursive: true });
    await fs.cp(opts.linux, linux, { recursive: true, preserveTimestamps: true });
    await fs.writeFile(path.join(linux, 'spark'), '#!/bin/sh\nset -eu\nspark_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)\nexec "$spark_dir/spark-lan" "$@"\n', { mode: 0o755 });
    for (const file of ['spark', 'spark-lan', 'resources/bin/spark-service', 'chrome_crashpad_handler', 'chrome-sandbox']) {
      const target = path.join(linux, file);
      if (await isPresent(target)) await fs.chmod(target, (await fs.stat(target)).mode | 0o111);
    }
    await fs.copyFile(setup, path.join(windows, path.basename(setup)));
    await fs.copyFile(zip, path.join(windows, path.basename(zip)));
    await fs.writeFile(path.join(stage, 'КАК ЗАПУСТИТЬ.txt'), `Spark ${expected.version}\n\nWindows\nЗапустите Windows/${path.basename(setup)} и установите Spark.\nЛибо распакуйте Windows/${path.basename(zip)} целиком и запустите Spark.exe.\n\nLinux x86_64\nОткройте Linux/Spark и запустите ./spark или ./spark-lan.\nПереносите папку Spark целиком: рядом лежат необходимые библиотеки и ресурсы.\nЕсли файловый менеджер снял права запуска после копирования, выполните:\nchmod +x Linux/Spark/spark Linux/Spark/spark-lan Linux/Spark/resources/bin/spark-service Linux/Spark/chrome_crashpad_handler Linux/Spark/chrome-sandbox\nAppImage не требуется.\n\nСоздайте комнату на одном компьютере, на остальных подключитесь по адресу и коду.\nЗакрытие окна по умолчанию скрывает Spark в трей. Полный выход — «Выйти из Spark».\n\nПроверка целостности из этой папки (Linux): sha256sum -c SHA256SUMS.txt\nSHA256SUMS.txt содержит хеши всех файлов комплекта, кроме самого списка.\n`);
    const delivered = (await filesIn(stage)).sort();
    const sums = [];
    for (const file of delivered) sums.push(`${await sha256(file)}  ${path.relative(stage, file).split(path.sep).join('/')}`);
    await fs.writeFile(path.join(stage, 'SHA256SUMS.txt'), sums.join('\n') + '\n');
    let backup;
    if (existed) {
      backup = `${opts.output}.previous-${randomUUID()}`;
      await fs.rename(opts.output, backup);
    }
    try { await fs.rename(stage, opts.output); stage = undefined; }
    catch (error) { if (backup) await fs.rename(backup, opts.output); throw error; }
    if (backup) await fs.rm(backup, { recursive: true });
    console.log(`Spark ${expected.version}: ${opts.output}\nWindows: установщик + ZIP; Linux: Spark/spark-lan + все ресурсы.\nSHA-256: ${delivered.length} файлов.`);
  } finally {
    await fs.rm(temp, { recursive: true, force: true });
    if (stage) await fs.rm(stage, { recursive: true, force: true });
  }
}

collect().catch(error => { console.error(`Комплект не создан: ${error.message}`); process.exitCode = 1; });
