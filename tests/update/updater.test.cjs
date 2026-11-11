'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
let fs;
try { fs = require('original-fs'); } catch { fs = require('node:fs'); }
const fsp = fs.promises;
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const { gzipSync } = require('node:zlib');
const tar = require('tar-stream');
const asar = require('@electron/asar');
const { _testing: updater, createUpdater } = require('../../desktop/updater.cjs');
const { powershellScript } = require('../../desktop/update-helper.cjs');

async function temporary(t) {
  const root = await fsp.mkdtemp(path.join(os.tmpdir(), 'Spark update test '));
  t.after(() => fsp.rm(root, { recursive: true, force: true }));
  return root;
}
async function tarBytes(entries) {
  const pack = tar.pack(); const chunks = [];
  const done = (async () => { for await (const chunk of pack) chunks.push(chunk); })();
  for (const entry of entries) await new Promise((resolve, reject) => pack.entry({ name: entry.name, type: entry.type || 'file', mode: entry.mode || 0o644,
    linkname: entry.linkname }, entry.data || '', error => error ? reject(error) : resolve()));
  pack.finalize(); await done;
  return gzipSync(Buffer.concat(chunks));
}
async function fixtureArchive(root, version = '0.3.0') {
  const app = path.join(root, 'asar-input');
  await fsp.mkdir(app, { recursive: true });
  await fsp.writeFile(path.join(app, 'package.json'), JSON.stringify({ name: 'spark-lan', version }));
  const archive = path.join(root, 'fixture.asar');
  await asar.createPackage(app, archive);
  return tarBytes([{ name: 'Spark/spark-lan', data: '#!/bin/sh\nexit 0\n', mode: 0o755 },
    { name: 'Spark/resources/app.asar', data: await fsp.readFile(archive) },
    { name: 'Spark/resources/bin/spark-service', data: Buffer.from([127, 69, 76, 70]), mode: 0o755 },
    { name: 'Spark/resources/ui/index.html', data: '<html></html>' },
    { name: 'Spark/resources/ui/pkg/spark_ui.js', data: '// wasm loader' },
    { name: 'Spark/resources/ui/pkg/spark_ui_bg.wasm', data: Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]) }]);
}
function zipBytes(entries) {
  const locals = [], central = []; let offset = 0;
  function crc32(data) {
    let value = 0xffffffff;
    for (const byte of data) { value ^= byte; for (let bit = 0; bit < 8; bit++) value = (value >>> 1) ^ (value & 1 ? 0xedb88320 : 0); }
    return (value ^ 0xffffffff) >>> 0;
  }
  for (const entry of entries) {
    const name = Buffer.from(entry.name), data = Buffer.from(entry.data || '');
    const local = Buffer.alloc(30); local.writeUInt32LE(0x04034b50); local.writeUInt16LE(20, 4);
    local.writeUInt32LE(crc32(data), 14); local.writeUInt32LE(data.length, 18); local.writeUInt32LE(data.length, 22); local.writeUInt16LE(name.length, 26);
    const header = Buffer.alloc(46); header.writeUInt32LE(0x02014b50); header.writeUInt16LE((3 << 8) | 20, 4); header.writeUInt16LE(20, 6);
    header.writeUInt32LE(crc32(data), 16); header.writeUInt32LE(data.length, 20); header.writeUInt32LE(data.length, 24); header.writeUInt16LE(name.length, 28);
    header.writeUInt32LE(((entry.mode || 0o100644) << 16) >>> 0, 38); header.writeUInt32LE(offset, 42);
    locals.push(local, name, data); central.push(header, name); offset += local.length + name.length + data.length;
  }
  const directory = Buffer.concat(central), footer = Buffer.alloc(22); footer.writeUInt32LE(0x06054b50);
  footer.writeUInt16LE(entries.length, 8); footer.writeUInt16LE(entries.length, 10); footer.writeUInt32LE(directory.length, 12); footer.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, footer]);
}
async function fixtureZip(root) {
  await fixtureArchive(root);
  return zipBytes([{ name: 'Spark.exe', data: 'fixture executable' }, { name: 'resources/app.asar', data: await fsp.readFile(path.join(root, 'fixture.asar')) },
    { name: 'resources/bin/spark-service.exe', data: 'MZ' }, { name: 'resources/ui/index.html', data: '<html></html>' },
    { name: 'resources/ui/pkg/spark_ui.js', data: '// wasm loader' }, { name: 'resources/ui/pkg/spark_ui_bg.wasm', data: Buffer.from([0, 97, 115, 109, 1, 0, 0, 0]) }]);
}
function entry(name, data) { return { name, size: data.length, sha256: crypto.createHash('sha256').update(data).digest('hex') }; }
async function releaseServer(t, bytes, change = () => {}) {
  let base, release, manifest, requests = 0;
  const name = 'Spark-0.3.0-linux-x64.tar.gz';
  const files = new Map([[name, bytes]]);
  const server = http.createServer((req, res) => {
    requests++;
    if (req.url === '/releases/latest') { res.setHeader('content-type', 'application/json'); res.end(JSON.stringify(release)); }
    else if (req.url.endsWith('/spark-update.json')) res.end(JSON.stringify(manifest));
    else {
      const data = files.get(decodeURIComponent(req.url.split('/').at(-1)));
      if (data) { res.setHeader('content-length', data.length); res.end(data); } else { res.statusCode = 404; res.end(); }
    }
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  t.after(() => { server.closeAllConnections(); server.close(); });
  base = `http://127.0.0.1:${server.address().port}`;
  const transport = { api: base + '/releases/latest', fetch, allowed: url => new URL(url).origin === base,
    assetUrl: (tag, file) => `${base}/EugeneHeisst/spark/releases/download/${encodeURIComponent(tag)}/${encodeURIComponent(file)}` };
  const metadata = entry(name, bytes);
  manifest = { version: '0.3.0', platforms: { 'linux-x64': metadata } };
  release = { tag_name: 'v0.3.0', draft: false, prerelease: false, assets: [
    { name: 'spark-update.json', browser_download_url: transport.assetUrl('v0.3.0', 'spark-update.json') },
    { name, size: bytes.length, browser_download_url: transport.assetUrl('v0.3.0', name) },
  ] };
  change({ release, manifest, files, base });
  return { transport, requests: () => requests, server };
}
function app(root) { return { isPackaged: true, getVersion: () => '0.2.0', getPath: () => path.join(root, 'profile'), quit() { throw new Error('A check must never quit the app'); } }; }
async function filesBelow(directory) {
  try { return (await fsp.readdir(directory, { recursive: true })).map(String); } catch { return []; }
}

test('stable semver compares numerically and rejects prereleases/downgrades', () => {
  assert.equal(updater.newer('0.10.0', '0.9.9'), true);
  assert.equal(updater.newer('0.2.0', '0.3.0'), false);
  assert.equal(updater.newer('0.3.0', '0.3.0'), false);
  for (const value of ['0.3.0-beta.1', 'v0.3.0', '01.2.3', '999999999999.0.0']) assert.throws(() => updater.newer(value, '0.2.0'));
});

test('checks a real HTTP release, streams verified archive, validates asar, and never installs automatically', async t => {
  const root = await temporary(t), bytes = await fixtureArchive(root);
  const server = await releaseServer(t, bytes);
  const instance = updater.createUpdater({ app: app(root), platform: 'linux', getPreferences: () => ({ auto_updates: false }) }, server.transport);
  instance.schedule();
  const first = instance.check(), concurrent = instance.check();
  await Promise.all([first, concurrent]);
  assert.equal(instance.status().state, 'ready');
  assert.equal(instance.status().version, '0.3.0');
  assert.equal(server.requests(), 3, 'concurrent checks must share one request sequence');
  assert.equal((await filesBelow(path.join(root, 'profile'))).some(file => file.endsWith('.part')), false);
});

test('a corrupted download is rejected and .part is removed', async t => {
  const root = await temporary(t), bytes = await fixtureArchive(root);
  const server = await releaseServer(t, bytes, ({ manifest }) => { manifest.platforms['linux-x64'].sha256 = '0'.repeat(64); });
  const instance = updater.createUpdater({ app: app(root), platform: 'linux' }, server.transport);
  await instance.check();
  assert.equal(instance.status().state, 'error');
  assert.match(instance.status().message, /сумма/);
  assert.equal((await filesBelow(path.join(root, 'profile'))).some(file => file.endsWith('.part')), false);
});

test('manifest size mismatch and foreign GitHub asset URLs are rejected before download', async t => {
  const root = await temporary(t), bytes = await fixtureArchive(root);
  for (const change of [({ manifest }) => { manifest.platforms['linux-x64'].size++; },
    ({ release }) => { release.assets[1].browser_download_url = 'https://evil.example/update.tar.gz'; },
    ({ release }) => { release.prerelease = true; }]) {
    const server = await releaseServer(t, bytes, change);
    const instance = updater.createUpdater({ app: app(root), platform: 'linux' }, server.transport);
    await instance.check();
    assert.equal(instance.status().state, 'error');
    assert.ok(server.requests() <= 2);
  }
});

test('offline release failure stays asynchronous and leaves the app running', async t => {
  const root = await temporary(t);
  const transport = { api: 'http://127.0.0.1:1/latest', allowed: () => true, fetch, assetUrl: () => '' };
  const instance = updater.createUpdater({ app: app(root), platform: 'linux' }, transport);
  assert.equal(instance.status().state, 'idle');
  await instance.check();
  assert.equal(instance.status().state, 'error');
});

test('production constructor ignores attempted endpoint overrides', async t => {
  const root = await temporary(t);
  const instance = createUpdater({ app: { ...app(root), isPackaged: false }, api: 'http://evil.test/', platform: 'linux' });
  assert.equal(instance.status().state, 'unsupported');
  assert.equal((await instance.check()).state, 'unsupported');
});

test('tar extraction refuses traversal, absolute paths, symlinks, and duplicated files', async t => {
  const root = await temporary(t);
  const cases = [
    [{ name: '../outside', data: 'bad' }],
    [{ name: '/tmp/outside', data: 'bad' }],
    [{ name: 'Spark/link', type: 'symlink', linkname: '../outside' }],
    [{ name: 'Spark/file', data: 'a' }, { name: 'Spark/file', data: 'b' }],
  ];
  for (const [index, entries] of cases.entries()) {
    const archive = path.join(root, `bad-${index}.tar.gz`), unpacked = path.join(root, `unpacked-${index}`);
    await fsp.writeFile(archive, await tarBytes(entries));
    await assert.rejects(updater.prepare(archive, unpacked, 'linux', '0.3.0'));
    assert.equal(fs.existsSync(unpacked), false);
  }
  assert.equal(fs.existsSync(path.join(root, 'outside')), false);
});

test('archive asar version must equal the verified release manifest', async t => {
  const root = await temporary(t), archive = path.join(root, 'wrong.tar.gz');
  await fsp.writeFile(archive, await fixtureArchive(root, '0.9.0'));
  await assert.rejects(updater.prepare(archive, path.join(root, 'unpack'), 'linux', '0.3.0'), /Версия/);
});

test('ZIP extraction validates a real Windows app and rejects traversal and Unix symlinks', async t => {
  const root = await temporary(t), archive = path.join(root, 'windows.zip');
  await fsp.writeFile(archive, await fixtureZip(root));
  const extracted = await updater.prepare(archive, path.join(root, 'good'), 'win32', '0.3.0');
  assert.equal(await fsp.readFile(path.join(extracted, 'Spark.exe'), 'utf8'), 'fixture executable');
  for (const entries of [[{ name: '../escape', data: 'bad' }], [{ name: 'link', mode: 0o120777, data: '../escape' }]]) {
    await fsp.writeFile(archive, zipBytes(entries));
    await assert.rejects(updater.prepare(archive, path.join(root, 'bad'), 'win32', '0.3.0'));
  }
});

test('Windows installed copies fetch the verified NSIS installer; portable copies only fetch ZIP', async t => {
  const root = await temporary(t), bytes = await fixtureZip(root), installDir = path.join(root, 'installed Spark');
  await fsp.mkdir(installDir); await fsp.writeFile(path.join(installDir, 'Uninstall Spark.exe'), 'fixture');
  const name = 'Spark-0.3.0-windows-x64.zip', installer = Buffer.from('NSIS fixture bytes');
  const server = await releaseServer(t, bytes, ({ release, manifest, files, base }) => {
    const url = name => `${base}/EugeneHeisst/spark/releases/download/v0.3.0/${name}`;
    release.assets[1] = { name, size: bytes.length, browser_download_url: url(name) };
    files.set(name, bytes);
    manifest.platforms = { 'win32-x64': entry(name, bytes) };
    manifest.windowsInstaller = entry('Spark-0.3.0-windows-setup.exe', installer);
    files.set(manifest.windowsInstaller.name, installer);
    release.assets.push({ name: manifest.windowsInstaller.name, size: installer.length, browser_download_url: url(manifest.windowsInstaller.name) });
  });
  const instance = updater.createUpdater({ app: app(root), platform: 'win32' }, server.transport, { execPath: path.join(installDir, 'Spark.exe') });
  await instance.check(); assert.equal(instance.status().state, 'ready'); assert.equal(server.requests(), 4);
  assert.equal(await fsp.readFile(path.join(root, 'profile', 'updates', '0.3.0', 'Spark-0.3.0-windows-setup.exe'), 'utf8'), installer.toString());
  await fsp.rm(path.join(installDir, 'Uninstall Spark.exe'));
  const portable = updater.createUpdater({ app: app(root), platform: 'win32' }, server.transport, { execPath: path.join(installDir, 'Spark.exe') });
  await portable.check(); assert.equal(portable.status().state, 'ready'); assert.equal(server.requests(), 6, 'portable uses existing cached ZIP and never requests an installer');
});

test('install rehashes cached archives before applying any files or stopping the service', async t => {
  const root = await temporary(t), bytes = await fixtureArchive(root), server = await releaseServer(t, bytes);
  let stopped = false;
  const instance = updater.createUpdater({ app: app(root), platform: 'linux', beforeInstall: async () => { stopped = true; } }, server.transport);
  await instance.check(); assert.equal(instance.status().state, 'ready');
  await fsp.appendFile(path.join(root, 'profile', 'updates', '0.3.0', 'Spark-0.3.0-linux-x64.tar.gz'), 'tampered');
  await assert.rejects(instance.install());
  assert.equal(instance.status().state, 'error'); assert.equal(stopped, false);
});

async function runHelperFixture(t, success) {
  const root = await temporary(t), installDir = path.join(root, 'Spark installed with spaces');
  const token = 'a'.repeat(48), stage = path.join(root, `.spark-update-${token}`), source = path.join(stage, 'Spark');
  await fsp.mkdir(installDir); await fsp.mkdir(source, { recursive: true });
  const ack = path.join(root, token + '.ack');
  const marker = path.join(root, 'old relaunched');
  const windows = process.platform === 'win32';
  const executable = windows ? 'Spark.exe' : 'spark-lan';
  if (windows) {
    const cmd = path.join(process.env.SystemRoot, 'System32', 'cmd.exe');
    await fsp.copyFile(cmd, path.join(installDir, executable)); await fsp.copyFile(cmd, path.join(source, executable));
    await fsp.writeFile(path.join(installDir, 'fixture.cmd'), `@echo old>"${marker}"\r\n`);
    await fsp.writeFile(path.join(source, 'fixture.cmd'), success ? `@<nul set /p =${token}>"${ack}"\r\n` : '@exit /b 1\r\n');
  } else {
    const quote = s => "'" + s.replaceAll("'", "'\\''") + "'";
    await fsp.writeFile(path.join(installDir, executable), `#!/bin/sh\nprintf old > ${quote(marker)}\n`, { mode: 0o755 });
    await fsp.writeFile(path.join(source, executable), success ? `#!/bin/sh\nprintf %s ${quote(token)} > ${quote(ack)}\n` : '#!/bin/sh\nexit 1\n', { mode: 0o755 });
  }
  await fsp.writeFile(path.join(installDir, 'version'), 'old'); await fsp.writeFile(path.join(source, 'version'), 'new');
  const job = { token, version: '0.3.0', parentPid: 2147483647, installDir, source, stage, executable,
    launchArgs: windows ? ['/c', 'fixture.cmd'] : [], backup: path.join(root, 'old backup'), ack,
    started: path.join(root, token + '.started'), cancel: path.join(root, token + '.cancel'), timeout: success ? 3000 : 500, installer: null };
  job.backup = path.join(root, `.spark-backup-${token}`);
  const jobFile = path.join(root, 'pending.json'); await fsp.writeFile(jobFile, JSON.stringify(job));
  let helper;
  if (windows) {
    const script = path.join(root, 'helper with spaces.ps1'); await fsp.writeFile(script, powershellScript);
    helper = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script, '-JobFile', jobFile]);
  } else helper = spawn(process.execPath, [path.resolve(__dirname, '../../desktop/update-helper.cjs'), jobFile]);
  let diagnostics = ''; helper.stderr.on('data', b => { diagnostics += b; });
  const [code] = await once(helper, 'exit'); assert.equal(code, 0, diagnostics);
  assert.equal(await fsp.readFile(path.join(installDir, 'version'), 'utf8'), success ? 'new' : 'old');
  assert.equal(fs.existsSync(job.backup), false); assert.equal(fs.existsSync(stage), false);
  if (!success) {
    assert.match(await fsp.readFile(jobFile + '.error', 'utf8'), /подтвердила|acknowledge/);
    for (let i = 0; i < 50 && !fs.existsSync(marker); i++) await new Promise(r => setTimeout(r, 20));
    assert.equal(fs.existsSync(marker), true, 'old application is relaunched after rollback');
  }
}
test('native helper atomically replaces an actual directory after startup acknowledgement (paths with spaces)', async t => runHelperFixture(t, true));
test('native helper restores and relaunches actual old files if startup fails', async t => runHelperFixture(t, false));

test('real Electron fs validates physical asar archives and helper replaces its mapped runtime', { skip: process.platform !== 'linux' || !!process.versions.electron }, async t => {
  const electron = require('electron');
  const testEnv = { ...process.env, ELECTRON_RUN_AS_NODE: '1' };
  delete testEnv.NODE_TEST_CONTEXT;
  const underElectron = spawn(electron, ['--test', __filename], { env: testEnv, stdio: ['ignore', 'pipe', 'pipe'] });
  let diagnostics = '';
  underElectron.stdout.on('data', b => { diagnostics += b; }); underElectron.stderr.on('data', b => { diagnostics += b; });
  assert.equal((await once(underElectron, 'exit'))[0], 0, diagnostics);
  assert.match(diagnostics, /tests 14/, 'the nested suite must actually execute under Electron');
  const root = await temporary(t), token = 'b'.repeat(48), installDir = path.join(root, 'old Electron with spaces');
  const stage = path.join(root, `.spark-update-${token}`), source = path.join(stage, 'Spark');
  await fsp.cp(path.dirname(electron), installDir, { recursive: true, mode: fs.constants.COPYFILE_FICLONE });
  await fsp.mkdir(source, { recursive: true });
  const ack = path.join(root, token + '.ack'), executable = path.basename(electron);
  const quote = s => "'" + s.replaceAll("'", "'\\''") + "'";
  await fsp.writeFile(path.join(source, executable), `#!/bin/sh\nprintf %s ${quote(token)} > ${quote(ack)}\n`, { mode: 0o755 });
  const job = { token, version: '0.3.0', parentPid: 2147483647, installDir, stage, source, executable,
    backup: path.join(root, `.spark-backup-${token}`), ack, started: path.join(root, token + '.started'),
    cancel: path.join(root, token + '.cancel'), timeout: 3000, installer: null };
  const jobFile = path.join(root, 'pending.json'); await fsp.writeFile(jobFile, JSON.stringify(job));
  const helper = path.join(root, 'outside helper.cjs'); await fsp.copyFile(path.resolve(__dirname, '../../desktop/update-helper.cjs'), helper);
  const child = spawn(path.join(installDir, executable), [helper, jobFile], { env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' }, cwd: root, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = ''; child.stderr.on('data', b => { output += b; });
  assert.equal((await once(child, 'exit'))[0], 0, output);
  assert.equal(fs.existsSync(jobFile + '.error'), false); assert.equal(fs.existsSync(job.backup), false, 'physical .asar archives must be removed with the old mapped runtime');
  assert.equal(fs.existsSync(jobFile), false); assert.equal(fs.existsSync(path.join(installDir, executable)), true);
});
