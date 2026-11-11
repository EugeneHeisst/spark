'use strict';
// Only this main-process module talks to GitHub. The renderer receives status,
// never URLs, paths, installer arguments, or an API that accepts them.
// Electron's regular fs presents .asar files as virtual directories. Updates
// must write/hash/delete the physical archives, including on rollback cleanup.
let fs;
try { fs = require('original-fs'); } catch { fs = require('node:fs'); }
const fsp = fs.promises;
const path = require('node:path');
const crypto = require('node:crypto');
const { Readable, Transform } = require('node:stream');
const { pipeline } = require('node:stream/promises');
const { createGunzip } = require('node:zlib');
const { spawn } = require('node:child_process');
const tar = require('tar-stream');
const yauzl = require('yauzl');
const asar = require('@electron/asar');
const { powershellScript } = require('./update-helper.cjs');

const REPOSITORY = 'EugeneHeisst/spark';
const API = `https://api.github.com/repos/${REPOSITORY}/releases/latest`;
const MAX_ARCHIVE = 2 * 1024 ** 3;
const MAX_EXPANDED = 3 * 1024 ** 3;
const MAX_ENTRIES = 30_000;
const SIX_HOURS = 6 * 60 * 60 * 1000;

function versionParts(value) {
  if (typeof value !== 'string' || !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(value)) throw new Error('Некорректная версия обновления');
  const parts = value.split('.').map(Number);
  if (parts.some(v => !Number.isSafeInteger(v) || v > 1_000_000)) throw new Error('Некорректная версия обновления');
  return parts;
}
function newer(a, b) {
  const x = versionParts(a), y = versionParts(b);
  for (let i = 0; i < 3; i++) if (x[i] !== y[i]) return x[i] > y[i];
  return false;
}
function safeName(name) {
  if (typeof name !== 'string' || name.length > 512 || /[\\:\x00-\x1f]/.test(name) || name.startsWith('/')) throw new Error('Небезопасный путь в архиве');
  while (name.startsWith('./')) name = name.slice(2);
  name = name.replace(/\/$/, '');
  if (!name || name.split('/').some(p => !p || p === '.' || p === '..' || /[. ]$/.test(p))) throw new Error('Небезопасный путь в архиве');
  // Windows device names and NTFS alternate streams must not be materialised.
  if (name.split('/').some(p => /^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(p))) throw new Error('Небезопасный путь в архиве');
  return name;
}
async function hashFile(file, expected) {
  const stat = await fsp.lstat(file);
  if (!stat.isFile() || stat.size !== expected.size || stat.size > MAX_ARCHIVE) throw new Error('Размер обновления не совпадает');
  const hash = crypto.createHash('sha256');
  for await (const chunk of fs.createReadStream(file)) hash.update(chunk);
  if (hash.digest('hex') !== expected.sha256) throw new Error('Контрольная сумма обновления не совпадает');
}
function expectedEntry(value, name) {
  if (!value || value.name !== name || !/^[a-f0-9]{64}$/.test(value.sha256)
    || !Number.isSafeInteger(value.size) || value.size < 1 || value.size > MAX_ARCHIVE) throw new Error('Некорректный манифест обновления');
  return { name, sha256: value.sha256, size: value.size };
}
function destination(root, name) { return path.join(root, ...safeName(name).split('/')); }

async function extractTar(archive, output) {
  const extract = tar.extract();
  let count = 0, expanded = 0;
  extract.on('entry', (header, stream, next) => {
    (async () => {
      if (++count > MAX_ENTRIES || !Number.isSafeInteger(header.size) || header.size < 0 || (expanded += header.size) > MAX_EXPANDED) throw new Error('Архив обновления слишком большой');
      if (!['file', 'directory'].includes(header.type)) throw new Error('Ссылки и специальные файлы в обновлении запрещены');
      const target = destination(output, header.name);
      if (header.type === 'directory') { await fsp.mkdir(target, { recursive: true }); stream.resume(); }
      else {
        await fsp.mkdir(path.dirname(target), { recursive: true });
        await pipeline(stream, fs.createWriteStream(target, { flags: 'wx', mode: header.mode & 0o777 }));
      }
      next();
    })().catch(error => extract.destroy(error));
  });
  await pipeline(fs.createReadStream(archive), createGunzip(), extract);
}
async function extractZip(archive, output) {
  const zip = await new Promise((resolve, reject) => yauzl.open(archive, { lazyEntries: true, validateEntrySizes: true, autoClose: true }, (e, z) => e ? reject(e) : resolve(z)));
  await new Promise((resolve, reject) => {
    let count = 0, expanded = 0;
    const fail = error => { zip.close(); reject(error); };
    zip.on('error', fail);
    zip.on('end', resolve);
    zip.on('entry', entry => {
      (async () => {
        if (++count > MAX_ENTRIES || (expanded += entry.uncompressedSize) > MAX_EXPANDED) throw new Error('Архив обновления слишком большой');
        const type = (entry.externalFileAttributes >>> 16) & 0o170000;
        if (entry.generalPurposeBitFlag & 1 || ![0, 0o100000, 0o040000].includes(type)) throw new Error('Ссылки и зашифрованные файлы в обновлении запрещены');
        const target = destination(output, entry.fileName);
        if (entry.fileName.endsWith('/')) await fsp.mkdir(target, { recursive: true });
        else {
          await fsp.mkdir(path.dirname(target), { recursive: true });
          const stream = await new Promise((res, rej) => zip.openReadStream(entry, (e, s) => e ? rej(e) : res(s)));
          await pipeline(stream, fs.createWriteStream(target, { flags: 'wx', mode: 0o644 }));
        }
        zip.readEntry();
      })().catch(fail);
    });
    zip.readEntry();
  });
}
async function packageRoot(unpacked, platform, version) {
  const candidates = [unpacked];
  for (const entry of await fsp.readdir(unpacked, { withFileTypes: true })) if (entry.isDirectory()) candidates.push(path.join(unpacked, entry.name));
  const executable = platform === 'win32' ? 'Spark.exe' : 'spark-lan';
  for (const candidate of candidates) {
    const binary = path.join(candidate, executable);
    const archive = path.join(candidate, 'resources', 'app.asar');
    try {
      if (!(await fsp.lstat(binary)).isFile() || !(await fsp.lstat(archive)).isFile()) continue;
      let metadata;
      const noAsar = process.noAsar;
      try {
        process.noAsar = true;
        metadata = JSON.parse(asar.extractFile(archive, 'package.json').toString('utf8'));
      } finally { process.noAsar = noAsar; }
      if (metadata.name !== 'spark-lan' || metadata.version !== version) throw new Error('Версия приложения в архиве не совпадает с манифестом');
      const core = path.join(candidate, 'resources', 'bin', platform === 'win32' ? 'spark-service.exe' : 'spark-service');
      const wasm = path.join(candidate, 'resources', 'ui', 'pkg', 'spark_ui_bg.wasm');
      for (const file of [core, wasm, path.join(candidate, 'resources', 'ui', 'pkg', 'spark_ui.js'), path.join(candidate, 'resources', 'ui', 'index.html')]) {
        const stat = await fsp.lstat(file);
        if (!stat.isFile() || !stat.size) throw new Error('В обновлении отсутствуют файлы Spark');
      }
      const header = async (file, count) => {
        const handle = await fsp.open(file, 'r');
        try { const data = Buffer.alloc(count); await handle.read(data, 0, count, 0); return data; }
        finally { await handle.close(); }
      };
      if (!(await header(wasm, 8)).equals(Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]))
        || !(await header(core, platform === 'win32' ? 2 : 4)).equals(Buffer.from(platform === 'win32' ? [77, 90] : [127, 69, 76, 70]))) throw new Error('Повреждены Rust/WASM-файлы обновления');
      if (platform === 'linux') await fsp.chmod(binary, 0o755);
      return candidate;
    } catch (error) { if (error.code !== 'ENOENT') throw error; }
  }
  throw new Error('В архиве нет приложения Spark');
}
async function prepare(archive, directory, platform, version) {
  await fsp.rm(directory, { recursive: true, force: true });
  await fsp.mkdir(directory, { recursive: true, mode: 0o700 });
  try {
    if (platform === 'linux') await extractTar(archive, directory);
    else await extractZip(archive, directory);
    return await packageRoot(directory, platform, version);
  } catch (error) { await fsp.rm(directory, { recursive: true, force: true }); throw error; }
}

function productionTransport() {
  return { api: API, fetch: globalThis.fetch, allowed: url => {
    const u = new URL(url);
    return u.protocol === 'https:' && !u.username && !u.password && !u.port
      && ['api.github.com', 'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com'].includes(u.hostname);
  }, assetUrl: (tag, name) => `https://github.com/${REPOSITORY}/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(name)}` };
}
async function responseFor(transport, initial, signal) {
  let url = initial;
  for (let redirects = 0; redirects < 6; redirects++) {
    if (!transport.allowed(url)) throw new Error('Недоверенный адрес обновления');
    const response = await transport.fetch(url, { redirect: 'manual', signal, headers: { 'user-agent': 'Spark-Updater', accept: 'application/vnd.github+json' } });
    if ([301, 302, 303, 307, 308].includes(response.status)) {
      const next = response.headers.get('location');
      await response.body?.cancel();
      if (!next) throw new Error('Некорректное перенаправление обновления');
      url = new URL(next, url).href;
      continue;
    }
    if (!response.ok) { await response.body?.cancel(); throw new Error(`Сервер обновлений: HTTP ${response.status}`); }
    return response;
  }
  throw new Error('Слишком много перенаправлений обновления');
}
async function json(transport, url, limit) {
  const response = await responseFor(transport, url, AbortSignal.timeout(30_000));
  const chunks = []; let size = 0;
  for await (const chunk of response.body) {
    size += chunk.length;
    if (size > limit) { await response.body.cancel().catch(() => {}); throw new Error('Ответ сервера обновлений слишком большой'); }
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}
async function download(transport, url, file, expected, progress) {
  await fsp.mkdir(path.dirname(file), { recursive: true, mode: 0o700 });
  const partial = file + '.part';
  await fsp.rm(partial, { force: true });
  let response;
  try {
    response = await responseFor(transport, url, AbortSignal.timeout(15 * 60_000));
    const length = response.headers.get('content-length');
    if (length !== null && Number(length) !== expected.size) throw new Error('Размер загрузки не совпадает с манифестом');
    let bytes = 0; const hash = crypto.createHash('sha256');
    const meter = new Transform({ transform(chunk, _, callback) {
      bytes += chunk.length;
      if (bytes > expected.size) return callback(new Error('Обновление превышает ожидаемый размер'));
      hash.update(chunk); progress(Math.floor(bytes / expected.size * 100)); callback(null, chunk);
    } });
    await pipeline(Readable.fromWeb(response.body), meter, fs.createWriteStream(partial, { flags: 'wx', mode: 0o600 }));
    if (bytes !== expected.size || hash.digest('hex') !== expected.sha256) throw new Error('Контрольная сумма обновления не совпадает');
    await fsp.rename(partial, file);
  } finally { await response?.body?.cancel().catch(() => {}); await fsp.rm(partial, { force: true }); }
}

function createInternal(options, transport, test = {}) {
  const { app, platform = process.platform, arch = process.arch, userData = app.getPath('userData'), getPreferences = () => ({}), beforeInstall = async () => {} } = options;
  const currentVersion = app.getVersion();
  const execPath = test.execPath || process.execPath;
  const installDir = path.dirname(execPath);
  const cache = path.join(userData, 'updates');
  const supported = app.isPackaged && arch === 'x64' && ['linux', 'win32'].includes(platform) && !process.env.APPIMAGE;
  let state = { state: supported ? 'idle' : 'unsupported', currentVersion, message: supported ? '' : 'Обновление доступно в распакованной сборке Spark для Linux и Windows' };
  let busy, plan, timer, initial;
  const set = next => { state = { currentVersion, ...next }; };
  const status = () => ({ ...state });
  // A failed helper runs after this process exited. Report its result next time
  // the restored app starts, without putting network I/O on the startup path.
  if (supported) {
    void fsp.readFile(path.join(cache, 'pending.json.error'), 'utf8').then(async message => {
      if (state.state === 'idle') set({ state: 'error', message: message.slice(0, 1000) });
      await fsp.rm(path.join(cache, 'pending.json.error'), { force: true });
    }).catch(() => {});
  }
  function exclusive(task, propagate = false) {
    if (busy) return busy;
    busy = task().catch(error => {
      set({ state: 'error', message: error.message || 'Не удалось обновить Spark' });
      if (propagate) throw error;
      return status();
    }).finally(() => { busy = undefined; });
    return busy;
  }
  const asset = (release, name, expected) => {
    const matches = release.assets?.filter(a => a.name === name);
    if (matches?.length !== 1 || matches[0].browser_download_url !== transport.assetUrl(release.tag_name, name)
      || (expected && matches[0].size !== expected.size)) throw new Error('В релизе нет доверенного файла обновления');
    return matches[0].browser_download_url;
  };
  async function check() {
    if (!supported) return status();
    return exclusive(async () => {
      set({ state: 'checking', message: 'Проверка обновлений…' });
      const release = await json(transport, transport.api, 1024 * 1024);
      if (release.draft !== false || release.prerelease !== false || typeof release.tag_name !== 'string') throw new Error('Некорректный стабильный релиз');
      const version = release.tag_name.replace(/^v/, '');
      versionParts(version);
      if (!newer(version, currentVersion)) { plan = undefined; set({ state: 'idle', message: 'Установлена последняя версия' }); return status(); }
      const manifest = await json(transport, asset(release, 'spark-update.json'), 64 * 1024);
      if (manifest.version !== version) throw new Error('Версия манифеста не совпадает с релизом');
      const name = `Spark-${version}-${platform === 'linux' ? 'linux-x64.tar.gz' : 'windows-x64.zip'}`;
      const entry = expectedEntry(manifest.platforms?.[`${platform}-x64`], name);
      const url = asset(release, name, entry);
      const directory = path.join(cache, version);
      const archive = path.join(directory, name);
      set({ state: 'available', version, message: `Доступен Spark ${version}` });
      const obtain = async (destination, value, address) => {
        try { await hashFile(destination, value); }
        catch {
          set({ state: 'downloading', version, percent: 0, message: 'Загрузка обновления…' });
          await download(transport, address, destination, value, percent => set({ state: 'downloading', version, percent, message: 'Загрузка обновления…' }));
        }
      };
      await obtain(archive, entry, url);
      await prepare(archive, path.join(directory, 'verify'), platform, version);
      await fsp.rm(path.join(directory, 'verify'), { recursive: true, force: true });
      let installer;
      if (platform === 'win32' && fs.existsSync(path.join(installDir, 'Uninstall Spark.exe'))) {
        const metadata = expectedEntry(manifest.windowsInstaller, `Spark-${version}-windows-setup.exe`);
        const file = path.join(directory, metadata.name);
        await obtain(file, metadata, asset(release, metadata.name, metadata));
        installer = { file, ...metadata };
      }
      plan = { archive, entry, version, installer };
      set({ state: 'ready', version, percent: 100, message: `Spark ${version} загружен. Установите обновление, когда закончите звонок.` });
      return status();
    });
  }
  async function install() {
    if (!supported || !plan || state.state !== 'ready' || busy) return status();
    return exclusive(async () => {
      const pending = plan;
      set({ state: 'installing', version: pending.version, message: 'Подготовка установки…' });
      await hashFile(pending.archive, pending.entry);
      if (pending.installer) await hashFile(pending.installer.file, pending.installer);
      const rootStat = await fsp.lstat(installDir);
      if (!rootStat.isDirectory() || rootStat.isSymbolicLink() || path.parse(installDir).root === installDir
        || userData === installDir || userData.startsWith(installDir + path.sep)) throw new Error('Переместите Spark в отдельную папку, а данные храните вне папки приложения');
      await fsp.access(path.dirname(installDir), fs.constants.W_OK);
      const token = crypto.randomBytes(24).toString('hex');
      // A sibling guarantees rename remains on the same filesystem.
      const stage = path.join(path.dirname(installDir), `.spark-update-${token}`);
      await fsp.mkdir(stage, { mode: 0o700 });
      let jobFile;
      try {
        const source = await prepare(pending.archive, path.join(stage, 'unpacked'), platform, pending.version);
        const job = { token, version: pending.version, parentPid: process.pid, installDir, source,
          backup: path.join(path.dirname(installDir), `.spark-backup-${token}`), stage,
          executable: path.basename(execPath), ack: path.join(cache, `${token}.ack`),
          started: path.join(cache, `${token}.started`), cancel: path.join(cache, `${token}.cancel`),
          timeout: 60_000, installer: pending.installer?.file || null };
        await fsp.mkdir(cache, { recursive: true, mode: 0o700 });
        jobFile = path.join(cache, 'pending.json');
        await fsp.writeFile(jobFile, JSON.stringify(job), { mode: 0o600 });
        const helper = path.join(cache, platform === 'win32' ? 'install.ps1' : 'update-helper.cjs');
        await fsp.writeFile(helper, platform === 'win32' ? powershellScript : await fsp.readFile(path.join(__dirname, 'update-helper.cjs')), { mode: 0o600 });
        const processHandle = platform === 'win32'
          ? spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', helper, '-JobFile', jobFile], { detached: true, windowsHide: true, stdio: 'ignore', cwd: cache })
          : spawn(execPath, [helper, jobFile], { detached: true, stdio: 'ignore', cwd: cache, env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' } });
        let spawnError;
        processHandle.on('error', error => { spawnError = error; });
        processHandle.unref();
        for (let i = 0; !fs.existsSync(job.started); i++) {
          if (spawnError) throw spawnError;
          if (i > 100) throw new Error('Помощник обновления не запустился');
          await new Promise(resolve => setTimeout(resolve, 50));
        }
        await beforeInstall();
        app.quit();
        return status();
      } catch (error) {
        if (jobFile) {
          const job = JSON.parse(await fsp.readFile(jobFile, 'utf8'));
          await fsp.writeFile(job.cancel, 'cancel');
        }
        await fsp.rm(stage, { recursive: true, force: true });
        throw error;
      }
    }, true);
  }
  async function acknowledge() {
    try {
      const job = JSON.parse(await fsp.readFile(path.join(cache, 'pending.json'), 'utf8'));
      if (job.version !== currentVersion || job.installDir !== installDir || !/^[a-f0-9]{48}$/.test(job.token)
        || job.ack !== path.join(cache, `${job.token}.ack`)) return false;
      await fsp.writeFile(job.ack, job.token, { mode: 0o600 });
      return true;
    } catch { return false; }
  }
  function stop() { clearInterval(timer); clearTimeout(initial); timer = undefined; initial = undefined; }
  function schedule() {
    stop();
    if (!supported || getPreferences().auto_updates === false) return;
    const automatic = () => { if (getPreferences().auto_updates !== false) void check(); };
    initial = setTimeout(automatic, 3000); initial.unref?.();
    timer = setInterval(automatic, SIX_HOURS); timer.unref?.();
  }
  return { status, check, install, schedule, stop, acknowledge };
}
function createUpdater(options) { return createInternal(options, productionTransport()); }
module.exports = { createUpdater, _testing: { createUpdater: (options, transport, test) => createInternal(options, transport, test),
  safeName, newer, hashFile, prepare, download, responseFor } };
