'use strict';
const { app, BrowserWindow, Menu, dialog, desktopCapturer, ipcMain, session, Tray, nativeImage } = require('electron');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const { createUpdater } = require('./updater.cjs');

app.setName('Spark');
app.setAppUserModelId('dev.eugeneheisst.spark');
if (process.env.SPARK_DATA_DIR) app.setPath('userData', path.resolve(process.env.SPARK_DATA_DIR));
// Synthetic microphone/camera input is opt-in and unavailable in packaged apps.
const testing = !app.isPackaged && process.env.SPARK_E2E_MEDIA === '1';
if (testing) {
  app.commandLine.appendSwitch('use-fake-device-for-media-stream');
  app.commandLine.appendSwitch('auto-accept-camera-and-microphone-capture');
  app.commandLine.appendSwitch('autoplay-policy', 'no-user-gesture-required');
  app.commandLine.appendSwitch('mute-audio');
}
app.commandLine.appendSwitch('enable-features', 'WebRTCPipeWireCapturer');

let child;
let mainWindow;
let boot;
let tray;
let preferences;
let updater;
let updateScheduled = false;
let activity = { inCall: false, transferring: false, hosting: false };
let nativeTransfers = 0;
function normalizeActivity(input) {
  if (!input || ['inCall', 'transferring', 'hosting'].some(key => typeof input[key] !== 'boolean')) {
    throw new Error('Некорректное состояние клиента');
  }
  return { inCall: input.inCall, transferring: input.transferring, hosting: input.hosting };
}
async function prepareUpdateRestart() {
  if (activity.inCall || activity.transferring || activity.hosting || nativeTransfers) {
    throw new Error('Перед обновлением завершите звонок и передачу файлов и остановите созданную комнату.');
  }
  quitting = true;
  const service = child;
  if (!service || service.exitCode !== null) return;
  try {
    await new Promise((resolve, reject) => {
      const timeout = setTimeout(() => { cleanup(); reject(new Error('Не удалось остановить ядро перед обновлением')); }, 10_000);
      const done = () => { cleanup(); resolve(); };
      const cleanup = () => { clearTimeout(timeout); service.removeListener('exit', done); };
      service.once('exit', done);
      service.kill(process.platform === 'win32' ? undefined : 'SIGINT');
      if (service.exitCode !== null) done();
    });
  } catch (error) { quitting = false; throw error; }
}
const defaults = { theme: 'dark', accent: '#ffb251', zoom: 100, sounds: true, volume: 55, close_to_tray: true, compact: false, auto_updates: true };
function normalizePreferences(input) {
  const p = input || {};
  return { theme: ['dark', 'light', 'midnight'].includes(p.theme) ? p.theme : defaults.theme,
    accent: /^#[0-9a-f]{6}$/i.test(p.accent) ? p.accent : defaults.accent,
    zoom: Number.isInteger(p.zoom) ? Math.min(150, Math.max(75, p.zoom)) : defaults.zoom,
    sounds: typeof p.sounds === 'boolean' ? p.sounds : defaults.sounds,
    volume: Number.isInteger(p.volume) ? Math.min(100, Math.max(0, p.volume)) : defaults.volume,
    close_to_tray: typeof p.close_to_tray === 'boolean' ? p.close_to_tray : defaults.close_to_tray,
    compact: typeof p.compact === 'boolean' ? p.compact : defaults.compact,
    auto_updates: typeof p.auto_updates === 'boolean' ? p.auto_updates : defaults.auto_updates };
}
function showWindow() {
  if (!mainWindow || mainWindow.isDestroyed()) return;
  if (mainWindow.isMinimized()) mainWindow.restore();
  mainWindow.show();
  mainWindow.focus();
}
function createTray(assets) {
  try {
    tray = new Tray(nativeImage.createFromPath(path.join(assets, 'icon.png')).resize({ width: 24, height: 24 }));
    tray.setToolTip('Spark — чаты и звонки');
    tray.setContextMenu(Menu.buildFromTemplate([
      { label: 'Открыть Spark', click: showWindow },
      { type: 'separator' },
      { label: 'Выйти из Spark', click: () => app.quit() },
    ]));
    tray.on('click', showWindow);
    tray.on('double-click', showWindow);
  } catch (error) { console.error('Spark tray:', error.message); }
}
let quitting = false;
const sourcesByContents = new Map();
const approvedSources = new Map();
const wayland = process.platform === 'linux' && (process.env.XDG_SESSION_TYPE === 'wayland'
  || (!process.env.XDG_SESSION_TYPE && !!process.env.WAYLAND_DISPLAY));
const ownOrigin = () => boot ? new URL(boot.url).origin : '';
const trustedUrl = url => {
  try { return new URL(url).origin === ownOrigin(); } catch { return false; }
};
const trustedIpc = event => {
  if (!event.senderFrame || !trustedUrl(event.senderFrame.url)) throw new Error('Недоверенный источник запроса');
};

function servicePaths() {
  const root = app.isPackaged ? process.resourcesPath : path.join(__dirname, '..');
  return {
    binary: app.isPackaged ? path.join(root, 'bin', process.platform === 'win32' ? 'spark-service.exe' : 'spark-service')
      : path.join(root, 'desktop', 'binaries', process.platform === 'win32' ? 'spark-service.exe' : 'spark-service'),
    assets: app.isPackaged ? path.join(root, 'ui') : path.join(root, 'assets'),
  };
}

function startService() {
  const { binary, assets } = servicePaths();
  if (!fs.existsSync(binary)) throw new Error('Rust-ядро не собрано. Выполните npm run build.');
  return new Promise((resolve, reject) => {
    let settled = false;
    let buffer = '';
    let stderr = '';
    child = spawn(binary, ['--data-dir', path.join(app.getPath('userData'), 'core'), '--ui-dir', assets,
      '--gateway-port', '0'], { stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
    const timeout = setTimeout(() => fail(new Error('Rust-ядро не запустилось за 20 секунд')), 20_000);
    function fail(error) {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      reject(error);
    }
    child.stderr.on('data', chunk => {
      stderr = (stderr + chunk.toString()).slice(-4000);
      process.stderr.write(chunk);
    });
    child.stdout.on('data', chunk => {
      buffer += chunk.toString();
      if (buffer.length > 64 * 1024) return fail(new Error('Некорректный ответ Rust-ядра'));
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        try {
          const message = JSON.parse(line);
          if (message.kind !== 'ready') continue;
          const url = new URL(message.url);
          if (url.protocol !== 'http:' || url.hostname !== '127.0.0.1' || !url.port
              || typeof message.token !== 'string' || message.token.length < 16) {
            return fail(new Error('Некорректный адрес Rust-ядра'));
          }
          if (!settled) {
            settled = true;
            clearTimeout(timeout);
            resolve(message);
          }
        } catch { /* Rust diagnostics belong on stderr; ignore non-ready lines. */ }
      }
    });
    child.on('error', fail);
    child.on('exit', code => {
      if (!settled) fail(new Error(`Rust-ядро завершилось (${code}). ${stderr}`));
      else if (!quitting) {
        dialog.showErrorBox('Spark — соединение остановлено', 'Rust-ядро завершилось. Перезапустите приложение.');
        app.quit();
      }
    });
  });
}

function installPermissions() {
  const ses = session.defaultSession;
  const permitted = permission => ['media', 'display-capture', 'speaker-selection', 'fullscreen'].includes(permission);
  ses.setPermissionCheckHandler((contents, permission, origin) =>
    !!contents && trustedUrl(contents.getURL()) && trustedUrl(origin) && permitted(permission));
  ses.setPermissionRequestHandler((contents, permission, callback, details) => {
    callback(trustedUrl(contents.getURL()) && (!details.requestingUrl || trustedUrl(details.requestingUrl))
      && permitted(permission));
  });
  ses.setDisplayMediaRequestHandler((request, callback) => {
    const frame = request.frame;
    if (!frame || !trustedUrl(frame.url)) return callback(null);
    const entry = approvedSources.get(frame.processId + ':' + frame.routingId);
    approvedSources.delete(frame.processId + ':' + frame.routingId);
    if (!entry || Date.now() - entry.at > 60_000) return callback(null);
    callback({ video: entry.source });
  });
}

function installIpc() {
  ipcMain.handle('spark:update-status', event => { trustedIpc(event); return updater.status(); });
  ipcMain.handle('spark:check-updates', event => { trustedIpc(event); return updater.check(); });
  ipcMain.handle('spark:activity', (event, input) => { trustedIpc(event); activity = normalizeActivity(input); });
  ipcMain.handle('spark:install-update', async (event, input) => {
    trustedIpc(event);
    activity = normalizeActivity(input);
    if (activity.inCall || activity.transferring || activity.hosting || nativeTransfers) {
      throw new Error('Перед обновлением завершите звонок и передачу файлов и остановите созданную комнату.');
    }
    try { return await updater.install(); }
    catch (error) {
      quitting = false;
      // A failed update preparation must keep the current client usable.
      if (child && child.exitCode !== null) {
        dialog.showErrorBox('Spark — обновление не установлено', error.message + '\nПерезапустите Spark, чтобы восстановить подключение.');
        app.quit();
      }
      throw error;
    }
  });
  ipcMain.handle('spark:ready', async event => {
    trustedIpc(event);
    await updater.acknowledge();
    if (!updateScheduled) { updateScheduled = true; updater.schedule(); }
    return true;
  });
  ipcMain.handle('spark:get-settings', event => { trustedIpc(event); return preferences; });
  ipcMain.handle('spark:set-settings', (event, input) => {
    trustedIpc(event);
    const next = normalizePreferences(input);
    const destination = path.join(app.getPath('userData'), 'preferences.json');
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.writeFileSync(destination + '.tmp', JSON.stringify(next), { mode: 0o600 });
    fs.renameSync(destination + '.tmp', destination);
    const changedUpdates = preferences.auto_updates !== next.auto_updates;
    preferences = next;
    if (changedUpdates && updateScheduled) { updater.stop(); if (next.auto_updates) updater.schedule(); }
    mainWindow.webContents.setZoomFactor(next.zoom / 100);
    return next;
  });
  ipcMain.handle('spark:quit', event => { trustedIpc(event); app.quit(); });
  ipcMain.handle('spark:version', event => { trustedIpc(event); return app.getVersion(); });
  ipcMain.handle('spark:sources', async event => {
    trustedIpc(event);
    // Enumerating PipeWire sources opens the portal itself. Request the portal
    // once at getDisplayMedia instead of selecting the source twice on Wayland.
    if (wayland) {
      const source = { id: 'screen:0:0', name: 'Выбрать окно или экран в системе' };
      sourcesByContents.set(event.sender.id, { at: Date.now(), sources: [source] });
      const icon = fs.readFileSync(path.join(servicePaths().assets, 'spark.svg'), 'utf8');
      return [{ ...source, thumbnail: 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(icon) }];
    }
    const sources = await desktopCapturer.getSources({ types: ['screen', 'window'],
      thumbnailSize: { width: 320, height: 180 }, fetchWindowIcons: false });
    sourcesByContents.set(event.sender.id, { at: Date.now(), sources });
    return sources.map(source => ({ id: source.id, name: source.name,
      thumbnail: source.thumbnail.isEmpty() ? '' : source.thumbnail.toDataURL() }));
  });
  ipcMain.handle('spark:select-source', (event, id) => {
    trustedIpc(event);
    if (typeof id !== 'string' || id.length > 512) throw new Error('Некорректный источник экрана');
    const offered = sourcesByContents.get(event.sender.id);
    const source = offered?.sources.find(source => source.id === id);
    if (!source || Date.now() - offered.at > 60_000) throw new Error('Выберите источник заново');
    approvedSources.set(event.senderFrame.processId + ':' + event.senderFrame.routingId,
      { source, at: Date.now() });
  });
  ipcMain.handle('spark:save-file', async (event, input) => {
    trustedIpc(event);
    if (!input || typeof input.id !== 'string' || !/^[a-zA-Z0-9_-]{1,128}$/.test(input.id)
        || typeof input.name !== 'string') throw new Error('Некорректный файл');
    const fileName = path.basename(input.name.replaceAll('\\', '/')).replace(/[<>:"|?*\x00-\x1f]/g, '_') || 'file';
    let destination;
    if (testing && process.env.SPARK_E2E_DOWNLOAD_DIR) {
      fs.mkdirSync(process.env.SPARK_E2E_DOWNLOAD_DIR, { recursive: true });
      destination = path.join(process.env.SPARK_E2E_DOWNLOAD_DIR, fileName);
    } else {
      const result = await dialog.showSaveDialog(mainWindow, { title: 'Сохранить файл из Spark',
        defaultPath: path.join(app.getPath('downloads'), fileName), properties: ['showOverwriteConfirmation'] });
      if (result.canceled || !result.filePath) return { canceled: true };
      destination = result.filePath;
    }
    nativeTransfers++;
    try {
    const response = await fetch(new URL('/api/save', boot.url), { method: 'POST',
      headers: { 'content-type': 'application/json', 'x-spark-token': boot.token },
      body: JSON.stringify({ file_id: input.id, path: destination, overwrite: fs.existsSync(destination) }) });
    const value = await response.json();
    if (!response.ok) throw new Error(value.error || 'Не удалось сохранить файл');
    return value;
    } finally { nativeTransfers--; }
  });
}

if (!app.requestSingleInstanceLock()) app.quit();
else {
  app.on('second-instance', () => {
    showWindow();
  });
  app.whenReady().then(async () => {
    try {
      Menu.setApplicationMenu(null);
      try { preferences = normalizePreferences(JSON.parse(fs.readFileSync(path.join(app.getPath('userData'), 'preferences.json'), 'utf8'))); }
      catch { preferences = { ...defaults }; }
      updater = createUpdater({ app, userData: app.getPath('userData'), getPreferences: () => preferences,
        beforeInstall: prepareUpdateRestart });
      boot = await startService();
      if (quitting) return;
      installPermissions();
      installIpc();
      const { assets } = servicePaths();
      mainWindow = new BrowserWindow({ width: 1360, height: 870, minWidth: 950, minHeight: 640,
        title: 'Spark', backgroundColor: '#111318', icon: path.join(assets, 'icon.png'), show: false,
        webPreferences: { preload: path.join(__dirname, 'preload.cjs'), nodeIntegration: false,
          contextIsolation: true, sandbox: true, spellcheck: false, backgroundThrottling: false } });
      createTray(assets);
      mainWindow.on('close', event => {
        if (!quitting && tray && preferences.close_to_tray) { event.preventDefault(); mainWindow.hide(); }
      });
      mainWindow.webContents.on('did-finish-load', () => mainWindow.webContents.setZoomFactor(preferences.zoom / 100));
      mainWindow.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
      mainWindow.webContents.on('will-navigate', (event, url) => { if (!trustedUrl(url)) event.preventDefault(); });
      mainWindow.once('ready-to-show', () => mainWindow.show());
      mainWindow.webContents.on('destroyed', () => { sourcesByContents.clear(); approvedSources.clear(); });
      await mainWindow.loadURL(boot.url);
    } catch (error) {
      dialog.showErrorBox('Не удалось запустить Spark', error.message);
      app.quit();
    }
  });
}
app.on('activate', showWindow);
app.on('window-all-closed', () => app.quit());
app.on('before-quit', () => {
  quitting = true;
  updater?.stop();
  tray?.destroy();
  tray = undefined;
  if (child && child.exitCode === null) child.kill(process.platform === 'win32' ? undefined : 'SIGINT');
});
process.on('SIGTERM', () => app.quit());
process.on('SIGINT', () => app.quit());
