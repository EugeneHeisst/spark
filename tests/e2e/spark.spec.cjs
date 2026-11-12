const { test, expect, _electron: electron } = require('@playwright/test');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { randomBytes, createHash } = require('node:crypto');

const root = path.resolve(__dirname, '../..');
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const id = (page, value) => page.getByTestId(value);
const snapshot = page => page.evaluate(() => window.sparkMediaDebug?.snapshot());
const stats = page => page.evaluate(async () => await window.sparkMediaDebug?.stats() || []);
const inbound = (reports, kind) => reports.flatMap(p => p.reports)
  .filter(r => r.type === 'inbound-rtp' && r.kind === kind)
  .reduce((sum, r) => sum + (r.bytesReceived || 0), 0);

async function launch(directory, downloads, errors) {
  const app = await electron.launch({ args: [root, ...(process.platform === 'linux' ? ['--ozone-platform=x11'] : [])],
    env: { ...process.env, WAYLAND_DISPLAY: '', XDG_SESSION_TYPE: process.platform === 'linux' ? 'x11' : '',
      SPARK_DATA_DIR: directory, SPARK_E2E_MEDIA: '1', SPARK_E2E_DOWNLOAD_DIR: downloads },
    timeout: 45_000 });
  const page = await app.firstWindow();
  page.on('pageerror', e => errors.push(e.stack || e.message));
  page.on('console', message => {
    if (/panicked at|already borrowed|BorrowMutError/.test(message.text())) errors.push(message.text());
  });
  app.process().stderr.on('data', data => {
    if (/panicked at|RuntimeError:|unreachable executed/.test(data.toString())) errors.push(data.toString());
  });
  await expect(id(page, 'display-name')).toBeVisible();
  return { app, page };
}

async function connect(page, name, hosting, code = 'spark-test-2026') {
  await id(page, 'display-name').fill(name);
  if (hosting) {
    await id(page, 'host-tab').click();
    await id(page, 'room-name').fill('Лаборатория Spark');
  } else {
    await id(page, 'join-tab').click();
    await id(page, 'room-address').fill('127.0.0.1:39393');
  }
  await id(page, 'room-code').fill(code);
  await id(page, 'connect').click();
  if (code === 'spark-test-2026') await expect(page.locator('#workspace')).toBeVisible();
}

async function send(page, text) {
  await id(page, 'message-input').fill(text);
  await id(page, 'message-input').press('Enter');
}

async function assertRemoteVideo(page, peer, kind) {
  const video = id(page, `video-${peer}-${kind}`);
  await expect(video).toBeVisible();
  await expect.poll(() => video.evaluate(v => ({ width: v.videoWidth, time: v.currentTime })))
    .toMatchObject({ width: expect.any(Number) });
  await expect.poll(() => video.evaluate(v => v.videoWidth > 0 && v.currentTime > 0.1)).toBe(true);
  return video;
}

async function retainCapture(page, kind) {
  await expect.poll(() => page.evaluate(kind => {
    const track = document.querySelector(`[data-testid="video-local-${kind}"]`)?.srcObject?.getVideoTracks()[0];
    const state = window.sparkMediaDebug.snapshot();
    return !!track && state.localTracks.find(t => t.kind === kind)?.id === track.id
      && state.peers.every(p => p.senders.find(t => t.kind === kind)?.id === track.id);
  }, kind)).toBe(true);
  await page.evaluate(kind => {
    window.sparkTestTracks ||= {};
    window.sparkTestTracks[kind] = document.querySelector(`[data-testid="video-local-${kind}"]`).srcObject.getVideoTracks()[0];
  }, kind);
}

async function captureEnded(page, kind) {
  await expect.poll(() => page.evaluate(kind => window.sparkTestTracks[kind].readyState, kind)).toBe('ended');
}

async function setPreference(page, field, value) {
  // Color/range inputs use the same bubbling input event as a real picker/slider.
  await id(page, field).evaluate((input, value) => {
    input.value = String(value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
  }, value);
}

async function observeEventAudio(page) {
  await page.evaluate(() => {
    const originalPlay = HTMLMediaElement.prototype.play;
    window.sparkTestSounds = [];
    // Observe the original browser decoder/playback, without replacing it with
    // a resolved promise or a fake audio object.
    HTMLMediaElement.prototype.play = function (...args) {
      const name = new URL(this.src || location.href).pathname.match(/^\/sounds\/([^/]+)\.wav$/)?.[1];
      if (!name) return originalPlay.apply(this, args);
      const record = { name, volume: this.volume, started: false, ended: false, error: null };
      window.sparkTestSounds.push(record);
      this.addEventListener('ended', () => {
        record.ended = true;
        record.duration = this.duration;
        record.time = this.currentTime;
      }, { once: true });
      const result = originalPlay.apply(this, args);
      result.then(() => { record.started = true; }, error => { record.error = error.message; });
      return result;
    };
  });
}

async function assertPlayedCue(page, name) {
  await expect.poll(() => page.evaluate(name =>
    window.sparkTestSounds.some(sound => sound.name === name && sound.started && !sound.error), name)).toBe(true);
}

async function assertFullscreenVideo(page, peer, kind, info, close = 'escape') {
  await page.evaluate(({ peer, kind }) => {
    window.sparkTestViewerTrack = document.querySelector(`[data-testid="video-${peer}-${kind}"]`).srcObject.getVideoTracks()[0];
  }, { peer, kind });
  await id(page, `expand-${peer}-${kind}`).click();
  const expanded = id(page, 'expanded-video');
  await expect(expanded).toBeVisible();
  await expect.poll(() => expanded.evaluate(v => v.videoWidth > 0 && v.currentTime > 0.1)).toBe(true);
  await id(page, 'fullscreen-video').click();
  await expect.poll(() => page.evaluate(() => !!document.fullscreenElement)).toBe(true);
  await expect.poll(() => expanded.evaluate(video => {
    const bounds = video.getBoundingClientRect();
    return bounds.width >= innerWidth - 30 && bounds.height >= innerHeight - 90;
  })).toBe(true);
  await page.screenshot({ path: info.outputPath(`fullscreen-${kind}.png`) });
  if (close === 'button') await page.locator('.modal-close').click();
  else await page.keyboard.press('Escape');
  await expect.poll(() => page.evaluate(() => !!document.fullscreenElement)).toBe(false);
  // Chromium can consume the first Escape to leave native fullscreen before
  // forwarding a later Escape to the viewer itself.
  if (await expanded.count()) await page.keyboard.press('Escape');
  await expect(expanded).toHaveCount(0);
  expect(await page.evaluate(({ peer, kind }) => {
    const track = document.querySelector(`[data-testid="video-${peer}-${kind}"]`).srcObject.getVideoTracks()[0];
    return track === window.sparkTestViewerTrack && track.readyState === 'live';
  }, { peer, kind })).toBe(true);
}

test('preferences: themes, accent, native zoom, event audio, durable settings and explicit quit', async ({}, info) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-preferences-'));
  const directory = path.join(temporary, 'client');
  const errors = [];
  let client;
  try {
    client = await launch(directory, path.join(temporary, 'saved'), errors);
    let { app, page } = client;
    // Synthetic tests still decode/play real media, with Chromium output muted.
    expect(await app.evaluate(({ app }) => app.commandLine.hasSwitch('mute-audio'))).toBe(true);
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
    await id(page, 'settings').click();
    await expect(id(page, 'pref-updates')).toBeChecked();
    await expect(id(page, 'updates-status')).toBeVisible();
    const updateState = await page.evaluate(() => window.sparkDesktop.checkUpdates());
    expect(updateState.state).toBe('unsupported');
    await expect(id(page, 'check-updates')).toBeDisabled();
    await id(page, 'pref-theme').selectOption('light');
    await expect(page.locator('body')).toHaveCSS('background-color', 'rgb(244, 245, 248)');
    await id(page, 'pref-theme').selectOption('midnight');
    await expect(page.locator('body')).toHaveCSS('background-color', 'rgb(9, 14, 25)');
    await setPreference(page, 'pref-accent', '#5ca9ff');
    await expect(page.locator('.onboarding .brand-mark')).toHaveCSS('background-color', 'rgb(92, 169, 255)');
    await setPreference(page, 'pref-zoom', 125);
    await expect(page.locator('#zoom-label')).toHaveText('125%');
    await expect.poll(() => app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.getZoomFactor())).toBe(1.25);
    await page.keyboard.press('Control+-');
    await expect.poll(() => app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.getZoomFactor())).toBe(1.2);
    await page.keyboard.press('Control+0');
    await expect.poll(() => app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.getZoomFactor())).toBe(1);
    await setPreference(page, 'pref-zoom', 125);
    await id(page, 'pref-compact').check();
    await setPreference(page, 'pref-volume', 35);

    const cues = ['peer-join', 'peer-leave', 'connect', 'disconnect', 'notification', 'invite', 'mic-on', 'mic-off', 'sound-on', 'sound-off'];
    const decoded = await page.evaluate(async cues => {
      const context = new AudioContext();
      try {
        return await Promise.all(cues.map(async name => {
          const response = await fetch(`/sounds/${name}.wav`);
          if (!response.ok) throw new Error(`${name}: HTTP ${response.status}`);
          const bytes = await response.arrayBuffer();
          const hash = [...new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))]
            .map(value => value.toString(16).padStart(2, '0')).join('');
          const audio = await context.decodeAudioData(bytes);
          return { name, hash, duration: audio.duration, channels: audio.numberOfChannels,
            peak: Math.max(...audio.getChannelData(0).map(Math.abs)) };
        }));
      } finally { await context.close(); }
    }, cues);
    expect(new Set(decoded.map(audio => audio.hash)).size).toBe(cues.length);
    for (const audio of decoded) {
      expect(audio.duration).toBeGreaterThan(0.05);
      expect(audio.duration).toBeLessThan(3);
      expect(audio.channels).toBeGreaterThan(0);
      expect(audio.peak).toBeGreaterThan(0.01);
    }
    await fs.writeFile(info.outputPath('decoded-event-audio.json'), JSON.stringify(decoded, null, 2));
    await observeEventAudio(page);
    for (const cue of cues) await page.locator(`[data-action="preview-sound"][data-id="${cue}"]`).click();
    await expect.poll(() => page.evaluate(() => window.sparkTestSounds.filter(sound => sound.started && sound.ended).length)).toBe(cues.length);
    const played = await page.evaluate(() => window.sparkTestSounds);
    expect(played.map(sound => sound.name)).toEqual(cues);
    for (const sound of played) {
      expect(sound.volume).toBeCloseTo(0.35);
      expect(sound.error).toBeNull();
      expect(sound.duration).toBeGreaterThan(0.05);
      expect(sound.time).toBeGreaterThan(0.05);
    }
    await id(page, 'pref-sounds').uncheck();
    for (const cue of cues) await page.locator(`[data-action="preview-sound"][data-id="${cue}"]`).click();
    expect(await page.evaluate(() => window.sparkTestSounds.length)).toBe(cues.length);
    await id(page, 'pref-sounds').check();
    await setPreference(page, 'pref-volume', 0);
    await page.locator('[data-action="preview-sound"][data-id="notification"]').click();
    expect(await page.evaluate(() => window.sparkTestSounds.length)).toBe(cues.length);
    await setPreference(page, 'pref-volume', 35);
    await id(page, 'pref-sounds').uncheck();
    await id(page, 'pref-tray').uncheck();
    await id(page, 'pref-updates').uncheck();
    await id(page, 'pref-noise').uncheck();
    await id(page, 'pref-share-audio').uncheck();
    const saved = { noise_suppression: false, share_audio: false, peer_volumes: {}, theme: 'midnight', accent: '#5ca9ff', zoom: 125, sounds: false,
      volume: 35, close_to_tray: false, compact: true, auto_updates: false };
    await expect.poll(async () => {
      try { return JSON.parse(await fs.readFile(path.join(directory, 'preferences.json'), 'utf8')); }
      catch { return null; }
    }).toEqual(saved);
    expect(await page.evaluate(() => window.sparkDesktop.getSettings())).toEqual(saved);
    await page.screenshot({ path: info.outputPath('personalized-settings.png') });

    // Disabling the tray makes the native close action really quit.
    const closed = app.waitForEvent('close');
    await app.evaluate(({ BrowserWindow }) => { setImmediate(() => BrowserWindow.getAllWindows()[0].close()); });
    await closed;
    client = undefined;
    client = await launch(directory, path.join(temporary, 'saved'), errors);
    ({ app, page } = client);
    expect(await page.evaluate(() => window.sparkDesktop.getSettings())).toEqual(saved);
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'midnight');
    await expect(page.locator('html')).toHaveAttribute('data-compact', 'true');
    await expect.poll(() => app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.getZoomFactor())).toBe(1.25);
    await id(page, 'settings').click();
    await expect(id(page, 'pref-theme')).toHaveValue('midnight');
    await expect(id(page, 'pref-accent')).toHaveValue('#5ca9ff');
    await expect(id(page, 'pref-zoom')).toHaveValue('125');
    await expect(id(page, 'pref-volume')).toHaveValue('35');
    await expect(id(page, 'pref-sounds')).not.toBeChecked();
    await expect(id(page, 'pref-tray')).not.toBeChecked();
    await expect(id(page, 'pref-updates')).not.toBeChecked();
    await expect(id(page, 'pref-compact')).toBeChecked();
    await id(page, 'settings-name').fill('Новое имя из настроек');
    await id(page, 'save-settings').click();
    await expect(id(page, 'display-name')).toHaveValue('Новое имя из настроек');
    await connect(page, 'Новое имя из настроек', true);
    await send(page, 'Компактное сообщение после перезапуска');
    await expect(page.locator('#message-list .message').last()).toHaveCSS('padding-top', '5px');
    await id(page, 'settings').click();
    await id(page, 'pref-tray').check();
    await expect.poll(() => page.evaluate(() => window.sparkDesktop.getSettings())).toMatchObject({ close_to_tray: true });
    const quit = app.waitForEvent('close');
    await id(page, 'quit-app').click();
    await quit;
    client = undefined;
    expect(errors).toEqual([]);
  } finally {
    await client?.app.close().catch(() => {});
    await fs.writeFile(info.outputPath('renderer-errors.json'), JSON.stringify(errors, null, 2));
    await fs.rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

test('two independent desktops: chat, files, actual RTP/video/screen, reconnect and durable history', async ({}, info) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-e2e-'));
  const clients = [];
  const errors = [];
  let a, b;
  try {
    a = await launch(path.join(temporary, 'alice'), path.join(temporary, 'saved-alice'), errors); clients.push(a.app);
    b = await launch(path.join(temporary, 'bob'), path.join(temporary, 'saved-bob'), errors); clients.push(b.app);
    await observeEventAudio(a.page); await observeEventAudio(b.page);
    await a.page.screenshot({ path: info.outputPath('01-onboarding.png') });
    await connect(a.page, 'Алиса', true);
    await id(b.page, 'refresh-discovery').click();
    await expect(b.page.locator('.discovery-item').filter({ hasText: 'Лаборатория Spark' })).toBeVisible();
    await connect(b.page, 'Борис', false, 'wrong-code');
    await expect(id(b.page, 'error')).toBeVisible();
    await expect(b.page.locator('#workspace')).toHaveCount(0);
    await b.page.locator('[data-action="dismiss-error"]').click();
    await connect(b.page, 'Борис', false);
    await expect(a.page.locator('#members .member')).toHaveCount(2);
    await expect(b.page.locator('#members .member')).toHaveCount(2);
    await assertPlayedCue(a.page, 'connect'); await assertPlayedCue(b.page, 'connect');

    await send(a.page, 'Привет из первого процесса! <script>это просто текст</script>');
    await expect(b.page.locator('#message-list')).toContainText('Привет из первого процесса!');
    await assertPlayedCue(b.page, 'notification');
    await expect(b.page.locator('#message-list script')).toHaveCount(0);
    await send(b.page, 'Ответ по настоящему WebSocket');
    await expect(a.page.locator('#message-list')).toContainText('Ответ по настоящему WebSocket');
    await id(a.page, 'new-text-channel').click();
    await id(a.page, 'channel-name').fill('идеи');
    await id(a.page, 'create-channel').click();
    const aChannel = a.page.locator('[data-action="channel"]').filter({ hasText: 'идеи' });
    const bChannel = b.page.locator('[data-action="channel"]').filter({ hasText: 'идеи' });
    await expect(bChannel).toBeVisible();
    await aChannel.click(); await bChannel.click();
    await send(a.page, 'Сообщение нового канала');
    await expect(b.page.locator('#message-list')).toContainText('Сообщение нового канала');

    const original = randomBytes(1024 * 1024 + 379);
    const fileName = 'проверка байтов.bin';
    await a.page.locator('#file-input').setInputFiles({ name: fileName, mimeType: 'application/octet-stream', buffer: original });
    const attachment = b.page.locator('.file-card').filter({ hasText: fileName });
    await expect(attachment).toBeVisible();
    await attachment.click();
    const receivedPath = path.join(temporary, 'saved-bob', fileName);
    await expect.poll(async () => { try { return sha(await fs.readFile(receivedPath)); } catch { return ''; } }).toBe(sha(original));
    expect((await fs.stat(receivedPath)).size).toBe(original.length);
    await a.page.screenshot({ path: info.outputPath('02-chat-files.png') });

    await id(a.page, 'channel-lounge').click();
    await expect.poll(async () => (await snapshot(a.page))?.channelId).toBe('lounge');
    const aliceId = (await snapshot(a.page)).selfId;
    // Incoming invitation must not start a microphone before explicit acceptance.
    const bobId = await b.page.evaluate(async () => (await (await fetch('/api/bootstrap')).json()).profile.id);
    await id(a.page, `invite-${bobId}`).click();
    await expect(id(b.page, 'accept-invite')).toBeVisible();
    await assertPlayedCue(b.page, 'invite');
    expect((await snapshot(b.page))?.channelId ?? null).toBeNull();
    await id(b.page, 'accept-invite').click();
    await expect.poll(async () => (await snapshot(b.page))?.channelId).toBe('lounge');
    await expect.poll(async () => inbound(await stats(a.page), 'audio')).toBeGreaterThan(200);
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(200);
    const first = inbound(await stats(b.page), 'audio');
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(first + 200);
    await id(a.page, 'mute').click();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.enabled).toBe(false);
    await assertPlayedCue(a.page, 'mic-off');
    await id(a.page, 'mute').click();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.enabled).toBe(true);
    await assertPlayedCue(a.page, 'mic-on');
    await id(b.page, 'deafen').click();
    await expect.poll(async () => (await snapshot(b.page)).deafened).toBe(true);
    await assertPlayedCue(b.page, 'sound-off');
    await expect.poll(() => b.page.locator('audio').evaluateAll(els => els.length > 0 && els.every(e => e.srcObject?.getAudioTracks().every(t => !t.enabled)))).toBe(true);
    await id(b.page, 'deafen').click();
    await expect.poll(() => b.page.locator('audio').evaluateAll(els => els.length > 0 && els.every(e => e.srcObject?.getAudioTracks().every(t => t.enabled)))).toBe(true);
    await assertPlayedCue(b.page, 'sound-on');

    await id(a.page, 'camera').click();
    await assertRemoteVideo(b.page, aliceId, 'camera');
    await id(b.page, 'camera').click();
    await assertRemoteVideo(a.page, bobId, 'camera');
    await retainCapture(a.page, 'camera');
    await retainCapture(b.page, 'camera');
    await assertFullscreenVideo(b.page, aliceId, 'camera', info, 'button');
    expect(await a.page.evaluate(() => window.sparkTestTracks.camera.readyState)).toBe('live');
    expect(await b.page.evaluate(() => window.sparkTestTracks.camera.readyState)).toBe('live');
    await expect.poll(async () => (await stats(b.page)).flatMap(p => p.reports)
      .filter(r => r.type === 'inbound-rtp' && r.kind === 'video').reduce((s, r) => s + (r.framesDecoded || 0), 0)).toBeGreaterThan(3);
    await b.page.screenshot({ path: info.outputPath('03-video-call.png') });

    if (process.env.SPARK_SKIP_SCREEN_TEST !== '1') {
      // A real separate native window on the isolated test display, never the user's desktop.
      await a.app.evaluate(async ({ BrowserWindow }) => {
        const target = new BrowserWindow({ width: 700, height: 480, title: 'Spark Capture Test',
          backgroundColor: '#20cf50', webPreferences: { nodeIntegration: false, contextIsolation: true } });
        global.sparkCaptureTest = target;
        await target.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent('<html><head><title>Spark Capture Test</title></head><body style="margin:0;background:#20cf50;color:#111;font:40px sans-serif"><div style="height:90px;background:#ef3030">SPARK REAL SCREEN</div><div style="height:90px;background:#3050ef"></div><div id="time"></div><script>setInterval(()=>document.getElementById("time").textContent=Date.now(),100)</script></body></html>'));
      });
      // loadURL resolves before the native WM maps/titles the new window.
      await expect.poll(() => a.page.evaluate(async () =>
        (await window.sparkDesktop.sources()).some(s => s.name === 'Spark Capture Test'))).toBe(true);
      await id(a.page, 'screen').click();
      await expect(a.page.locator('.source').first()).toBeVisible();
      const offered = await a.page.locator('.source').allTextContents();
      await fs.writeFile(info.outputPath('screen-sources.json'), JSON.stringify(offered, null, 2));
      const windowChoice = a.page.locator('.source').filter({ hasText: 'Spark Capture Test' });
      await expect(windowChoice).toBeVisible();
      await windowChoice.click();
      const screen = await assertRemoteVideo(b.page, aliceId, 'screen');
      await retainCapture(a.page, 'screen');
      await assertFullscreenVideo(b.page, aliceId, 'screen', info);
      expect(await a.page.evaluate(() => window.sparkTestTracks.screen.readyState)).toBe('live');
      // Decode actual received pixels: count the target's specific green,
      // distinct from the fake camera's pure-green test pattern and black tiles.
      await expect.poll(() => screen.evaluate(video => {
        const canvas = document.createElement('canvas'); canvas.width = 32; canvas.height = 32;
        const ctx = canvas.getContext('2d'); ctx.drawImage(video, 0, 0, 32, 32);
        const pixels = ctx.getImageData(0, 0, 32, 32).data;
        let matches = 0;
        for (let i = 0; i < pixels.length; i += 4) {
          if (pixels[i] >= 10 && pixels[i] < 75 && pixels[i+1] > 160 && pixels[i+1] < 235 && pixels[i+2] > 45 && pixels[i+2] < 120) matches++;
        }
        return matches > 40;
      })).toBe(true);
      await expect(id(b.page, `video-${aliceId}-camera`)).toBeVisible();
      await b.page.screenshot({ path: info.outputPath('04-real-screen-and-camera.png') });
    }

    // Native close hides the host window; its existing renderer, Rust gateway,
    // room, original capture tracks and actual RTP must continue in the tray.
    const nativeWindow = await a.app.browserWindow(a.page);
    const beforeTray = await nativeWindow.evaluate(window => ({ id: window.id, contents: window.webContents.id,
      renderer: window.webContents.getOSProcessId(), url: window.webContents.getURL() }));
    const tracksBeforeTray = (await snapshot(a.page)).localTracks.map(track => track.id).sort();
    const audioBeforeTray = inbound(await stats(a.page), 'audio');
    await nativeWindow.evaluate(window => window.close());
    await expect.poll(() => nativeWindow.evaluate(window => window.isVisible())).toBe(false);
    expect(a.app.windows()).toHaveLength(process.env.SPARK_SKIP_SCREEN_TEST === '1' ? 1 : 2);
    expect(await nativeWindow.evaluate(window => window.isDestroyed())).toBe(false);
    expect(await a.page.evaluate(async () => (await fetch('/api/bootstrap')).ok)).toBe(true);
    await send(b.page, 'Создатель в трее, комната продолжает работать');
    await expect(a.page.locator('#message-list')).toContainText('Создатель в трее, комната продолжает работать');
    await expect.poll(async () => inbound(await stats(a.page), 'audio')).toBeGreaterThan(audioBeforeTray + 200);
    expect((await snapshot(a.page)).localTracks.map(track => track.id).sort()).toEqual(tracksBeforeTray);
    expect(await a.page.evaluate(() => Object.values(window.sparkTestTracks).every(track => track.readyState === 'live'))).toBe(true);
    await a.app.evaluate(({ app }) => { app.emit('second-instance'); });
    await expect.poll(() => nativeWindow.evaluate(window => window.isVisible())).toBe(true);
    expect(await nativeWindow.evaluate(window => ({ id: window.id, contents: window.webContents.id,
      renderer: window.webContents.getOSProcessId(), url: window.webContents.getURL() }))).toEqual(beforeTray);
    await expect(a.page.locator('#members .member')).toHaveCount(2);
    await a.page.screenshot({ path: info.outputPath('restored-from-tray.png') });
    if (process.env.SPARK_SKIP_SCREEN_TEST !== '1') {
      await id(a.page, 'screen').click();
      await captureEnded(a.page, 'screen');
      await expect(id(b.page, `video-${aliceId}-screen`)).toHaveCount(0);
      await a.app.evaluate(() => { global.sparkCaptureTest?.destroy(); delete global.sparkCaptureTest; });
    }

    await fs.writeFile(info.outputPath('webrtc-stats.json'), JSON.stringify({ alice: await stats(a.page), bob: await stats(b.page) }, null, 2));
    // Genuine negotiated ICE restart, including an upper-ID request to the
    // deterministic offerer. Media must resume with a different ICE generation.
    const beforeIce = (await snapshot(b.page)).peers[0].localUfrag;
    await b.page.evaluate(peer => window.sparkMediaDebug.restartIce(peer), aliceId);
    await expect.poll(async () => (await snapshot(b.page)).peers[0]?.localUfrag).not.toBe(beforeIce);
    const audioBeforeRestart = inbound(await stats(b.page), 'audio');
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(audioBeforeRestart + 200);
    await id(a.page, 'leave-voice').click();
    await assertPlayedCue(a.page, 'disconnect');
    await captureEnded(a.page, 'camera');
    await expect.poll(async () => (await snapshot(a.page)).localTracks.length).toBe(0);
    await expect.poll(async () => (await snapshot(b.page)).peers.length).toBe(0);
    await id(b.page, 'leave-voice').click();
    await captureEnded(b.page, 'camera');
    await expect.poll(async () => (await snapshot(b.page)).localTracks.length).toBe(0);
    await id(a.page, 'channel-lounge').click(); await id(b.page, 'channel-lounge').click();
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(200);
    await id(a.page, 'leave-voice').click(); await id(b.page, 'leave-voice').click();

    // Restart native processes and reconnect to the same SQLite data, not an in-memory fixture.
    await b.app.close(); clients.splice(clients.indexOf(b.app), 1);
    await a.app.close(); clients.splice(clients.indexOf(a.app), 1);
    a = await launch(path.join(temporary, 'alice'), path.join(temporary, 'saved-alice'), errors); clients.push(a.app);
    b = await launch(path.join(temporary, 'bob'), path.join(temporary, 'saved-bob'), errors); clients.push(b.app);
    await connect(a.page, 'Алиса', true); await connect(b.page, 'Борис', false);
    await expect(b.page.locator('#message-list')).toContainText('Привет из первого процесса!');
    await b.page.locator('[data-action="channel"]').filter({ hasText: 'идеи' }).click();
    await expect(b.page.locator('#message-list')).toContainText('Сообщение нового канала');
    await expect(b.page.locator('.file-card')).toContainText(fileName);
    await b.page.screenshot({ path: info.outputPath('05-durable-history.png') });
    await id(b.page, 'settings').click();
    await id(b.page, 'settings-name').fill('Борис Новый');
    await id(b.page, 'save-settings').click();
    await expect(a.page.locator('#members')).toContainText('Борис Новый');
    await id(a.page, 'channel-general').click();
    await id(b.page, 'channel-general').click();
    await send(b.page, 'Новое имя после переподключения');
    await expect(a.page.locator('#message-list')).toContainText('Новое имя после переподключения');
    await expect(a.page.locator('#members .member')).toHaveCount(2);
    expect(errors).toEqual([]);
  } finally {
    for (const client of clients.reverse()) {
      try { const windows = client.windows(); if (windows[0]) await windows[0].screenshot({ path: info.outputPath(`final-${clients.indexOf(client)}.png`) }); } catch {}
      await client.close().catch(() => {});
    }
    await fs.writeFile(info.outputPath('renderer-errors.json'), JSON.stringify(errors, null, 2));
    await fs.rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

test('packaged application uses bundled Rust service and isolated renderer', async ({}, info) => {
  test.skip(!process.env.SPARK_PACKAGED_BINARY, 'Set SPARK_PACKAGED_BINARY after packaging');
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-package-'));
  let app;
  try {
    app = await electron.launch({ executablePath: process.env.SPARK_PACKAGED_BINARY,
      args: process.platform === 'linux' ? ['--ozone-platform=x11'] : [],
      env: { ...process.env, SPARK_DATA_DIR: temporary, SPARK_E2E_MEDIA: '1', WAYLAND_DISPLAY: '', XDG_SESSION_TYPE: 'x11' } });
    expect(await app.evaluate(({ app }) => app.isPackaged)).toBe(true);
    const page = await app.firstWindow();
    await expect(id(page, 'display-name')).toBeVisible();
    expect(await page.evaluate(() => typeof window.require)).toBe('undefined');
    expect(await page.evaluate(() => typeof window.sparkDesktop.sources)).toBe('function');
    const updateStatus = await page.evaluate(() => window.sparkDesktop.updateStatus());
    expect(updateStatus.currentVersion).toBe(await app.evaluate(({ app }) => app.getVersion()));
    expect(await page.evaluate(() => typeof window.sparkDesktop.checkUpdates)).toBe('function');
    const blocked = await page.evaluate(async () => {
      try { await window.sparkDesktop.installUpdate({ inCall: true, transferring: false, hosting: false }); return false; }
      catch (error) { return error.message.includes('звонок'); }
      finally { await window.sparkDesktop.setActivity({ inCall: false, transferring: false, hosting: false }); }
    });
    expect(blocked).toBe(true);
    expect(await page.evaluate(async () => (await fetch('/api/bootstrap')).ok)).toBe(true);
    // The unpackaged-only synthetic input flags are ignored in a real package.
    expect(await app.evaluate(({ app }) => app.commandLine.hasSwitch('use-fake-device-for-media-stream'))).toBe(false);
    await connect(page, 'Проверка пакета', true);
    await send(page, 'Сообщение из установленной сборки');
    await expect(page.locator('#message-list')).toContainText('Сообщение из установленной сборки');
    await page.screenshot({ path: info.outputPath('packaged-desktop.png') });
  } finally {
    await app?.close().catch(() => {});
    await fs.rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

test('three desktops: all six voice paths and isolation between voice channels', async ({}, info) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-mesh-'));
  const clients = [];
  const errors = [];
  try {
    for (const name of ['Алиса', 'Борис', 'Вера']) {
      const client = await launch(path.join(temporary, name), path.join(temporary, 'saved', name), errors);
      clients.push(client);
      await connect(client.page, name, clients.length === 1);
    }
    for (const { page } of clients) await id(page, 'channel-lounge').click();
    for (const { page } of clients) {
      await expect.poll(async () => {
        const peers = await stats(page);
        return peers.length === 2 && peers.every(p => p.connectionState === 'connected'
          && p.reports.some(r => r.type === 'inbound-rtp' && r.kind === 'audio' && r.bytesReceived > 200));
      }).toBe(true);
    }
    const [a, b, c] = clients;
    await id(a.page, 'new-voice-channel').click();
    await id(a.page, 'channel-name').fill('Тихая комната');
    await id(a.page, 'create-channel').click();
    const isolated = c.page.locator('[data-action="channel"]').filter({ hasText: 'Тихая комната' });
    await expect(isolated).toBeVisible();
    await isolated.click();
    await expect.poll(async () => (await snapshot(c.page)).peers.length).toBe(0);
    for (const client of [a, b]) await expect.poll(async () => (await snapshot(client.page)).peers.length).toBe(1);
    await id(c.page, 'channel-lounge').click();
    for (const { page } of clients) await expect.poll(async () => (await snapshot(page)).peers.filter(p => p.connectionState === 'connected').length).toBe(2);
    await fs.writeFile(info.outputPath('three-peer-rtp.json'), JSON.stringify(await Promise.all(clients.map(c => stats(c.page))), null, 2));
    await c.page.screenshot({ path: info.outputPath('three-peer-voice.png') });
    for (const { page } of clients) await id(page, 'leave-voice').click();
    for (const { page } of clients) {
      await expect.poll(async () => (await snapshot(page)).localTracks.length).toBe(0);
      await expect.poll(async () => (await snapshot(page)).peers.length).toBe(0);
    }
    expect(errors).toEqual([]);
  } finally {
    for (const { app } of clients.reverse()) await app.close().catch(() => {});
    await fs.writeFile(info.outputPath('renderer-errors.json'), JSON.stringify(errors, null, 2));
    await fs.rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});


test('voice controls: participant cues, speech activity, live noise suppression and independent stream volume', async ({}, info) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-audio-controls-'));
  const clients = [], errors = [];
  try {
    const a = await launch(path.join(temporary, 'a'), path.join(temporary, 'saved-a'), errors); clients.push(a);
    const b = await launch(path.join(temporary, 'b'), path.join(temporary, 'saved-b'), errors); clients.push(b);
    await id(a.page, 'settings').click();
    await id(a.page, 'pref-noise').uncheck();
    await a.page.locator('.modal-close').click();
    await a.page.evaluate(() => {
      const capture = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
      window.sparkTestMicrophones = [];
      navigator.mediaDevices.getUserMedia = async constraints => {
        const stream = await capture(constraints);
        window.sparkTestMicrophones.push(...stream.getAudioTracks());
        return stream;
      };
    });
    await connect(a.page, 'Говорящий', true);
    await connect(b.page, 'Слушатель', false);
    await id(a.page, 'channel-lounge').click();
    await observeEventAudio(a.page);
    await id(b.page, 'channel-lounge').click();
    await assertPlayedCue(a.page, 'peer-join');
    const alice = (await snapshot(a.page)).selfId;
    const bob = (await snapshot(b.page)).selfId;
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(200);
    const localSpeaking = a.page.locator(`.audio-person[data-speaker="${alice}"]`);
    const remoteSpeaking = b.page.locator(`.audio-person[data-speaker="${alice}"]`);
    await expect(localSpeaking).toHaveClass(/speaking/);
    await expect(remoteSpeaking).toHaveClass(/speaking/);
    await id(a.page, 'mute').click();
    await expect(localSpeaking).not.toHaveClass(/speaking/);
    await expect(remoteSpeaking).not.toHaveClass(/speaking/);
    await id(a.page, 'mute').click();
    await expect(remoteSpeaking).toHaveClass(/speaking/);

    const micBefore = (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio');
    expect(micBefore.settings.noiseSuppression).toBe(false);
    await id(a.page, 'settings').click();
    await id(a.page, 'pref-noise').check();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.settings.noiseSuppression).toBe(true);
    await id(a.page, 'pref-noise').uncheck();
    await expect.poll(() => a.page.evaluate(() => window.sparkDesktop.getSettings())).toMatchObject({ noise_suppression: false });
    await fs.writeFile(info.outputPath('noise-disabled.json'), JSON.stringify(await snapshot(a.page), null, 2));
    expect(await a.page.evaluate(() => window.sparkTestMicrophones.slice(0, -1).every(t => t.readyState === 'ended'))).toBe(true);
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.settings.noiseSuppression).toBe(false);
    expect((await snapshot(a.page)).localTracks.find(t => t.kind === 'audio').enabled).toBe(true);
    expect((await snapshot(a.page)).peers[0].connectionState).toBe('connected');
    await id(a.page, 'pref-noise').check();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.settings.noiseSuppression).toBe(true);
    await a.page.locator('.modal-close').click();

    // Synthetic video + PCM at the device boundary, with real encoding, RTP,
    // remote decoding and playback. No desktop or system audio is captured.
    await a.page.evaluate(() => {
      const context = new AudioContext();
      const tone = context.createOscillator(); tone.frequency.value = 523;
      const gain = context.createGain(); gain.gain.value = 0.15;
      const destination = context.createMediaStreamDestination();
      tone.connect(gain).connect(destination); tone.start();
      const canvas = document.createElement('canvas'); canvas.width = 320; canvas.height = 180;
      canvas.getContext('2d').fillRect(0, 0, 320, 180);
      const stream = canvas.captureStream(10);
      stream.addTrack(destination.stream.getAudioTracks()[0]);
      window.sparkTestStream = { context, tone, stream };
      navigator.mediaDevices.getDisplayMedia = async () => stream;
    });
    await id(a.page, 'screen').click();
    await id(a.page, 'pref-share-audio').check();
    await a.page.locator('.source').first().click();
    const streamAudio = b.page.locator(`#media-${alice}-screen-audio`);
    await expect(streamAudio).toHaveCount(1);
    await b.page.evaluate(async peer => {
      const context = new AudioContext(); await context.resume();
      const stream = document.getElementById(`media-${peer}-screen-audio`).srcObject;
      const analyser = context.createAnalyser(); analyser.fftSize = 2048;
      context.createMediaStreamSource(stream).connect(analyser);
      window.sparkTestReceived = { context, analyser };
    }, alice);
    await expect.poll(() => b.page.evaluate(() => {
      const samples = new Float32Array(2048);
      window.sparkTestReceived.analyser.getFloatTimeDomainData(samples);
      return Math.sqrt(samples.reduce((sum, n) => sum+n*n, 0)/samples.length);
    })).toBeGreaterThan(0.03);
    await setPreference(b.page, `volume-${alice}-audio`, 25);
    await setPreference(b.page, `volume-${alice}-screen-audio`, 70);
    const volumes = () => b.page.evaluate(peer => ['audio', 'screen-audio'].map(kind => document.getElementById(`media-${peer}-${kind}`)?.volume), alice);
    expect(await volumes()).toEqual([0.25, 0.7]);
    await id(a.page, 'mute').click();
    await expect(b.page.locator(`.peer-mixer[data-speaker="${alice}"]`)).not.toHaveClass(/speaking/);
    // Stream continues to carry PCM while the person's microphone is muted.
    await expect.poll(() => b.page.evaluate(() => {
      const samples = new Float32Array(2048); window.sparkTestReceived.analyser.getFloatTimeDomainData(samples);
      return Math.max(...samples.map(Math.abs));
    })).toBeGreaterThan(0.05);
    await setPreference(b.page, `volume-${alice}-screen-audio`, 0);
    expect(await volumes()).toEqual([0.25, 0]);
    await setPreference(b.page, `volume-${alice}-screen-audio`, 70);
    await id(b.page, 'deafen').click();
    expect(await streamAudio.evaluate(el => el.srcObject.getAudioTracks()[0].enabled)).toBe(false);
    await id(b.page, 'deafen').click();
    expect(await volumes()).toEqual([0.25, 0.7]);
    expect(await streamAudio.evaluate(el => el.srcObject.getAudioTracks()[0].enabled)).toBe(true);
    await b.page.screenshot({ path: info.outputPath('independent-audio-controls.png') });
    await id(a.page, 'screen').click();
    await expect(streamAudio).toHaveCount(0);
    expect(await a.page.evaluate(() => window.sparkTestStream.stream.getTracks().every(t => t.readyState === 'ended'))).toBe(true);
    await id(b.page, 'leave-voice').click();
    await assertPlayedCue(a.page, 'peer-leave');
    await id(b.page, 'channel-lounge').click();
    await expect.poll(() => b.page.locator(`#media-${alice}-audio`).evaluate(el => el.volume)).toBe(0.25);
    await expect(id(b.page, `volume-${alice}-screen-audio`)).toHaveValue('70');
    expect((await b.page.evaluate(() => window.sparkDesktop.getSettings())).peer_volumes[alice]).toEqual([25, 70]);
    await id(a.page, 'leave-voice').click();
    await id(b.page, 'leave-voice').click();
    expect(await a.page.locator('.speaking').count()).toBe(0);
    expect(await a.page.evaluate(() => window.sparkTestMicrophones.every(t => t.readyState === 'ended'))).toBe(true);
    expect(await b.page.locator('.speaking').count()).toBe(0);
    await a.page.evaluate(() => { window.sparkTestStream.tone.stop(); return window.sparkTestStream.context.close(); });
    await b.page.evaluate(() => window.sparkTestReceived.context.close());
    expect(errors).toEqual([]);
  } finally {
    for (const client of clients.reverse()) await client.app.close().catch(() => {});
    await fs.writeFile(info.outputPath('renderer-errors.json'), JSON.stringify(errors, null, 2));
    await fs.rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});

test('isolated system loopback: actual Linux desktop audio from a private null sink', async ({}, info) => {
  test.skip(process.platform !== 'linux' || process.env.SPARK_ISOLATED_AUDIO !== '1', 'Use scripts/test-loopback-linux.mjs');
  expect(process.env.PULSE_SERVER).toMatch(/^unix:\/tmp\/spark-loopback-[^/]+\/runtime\/pulse\/native$/);
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-loopback-client-'));
  let app, player;
  try {
    app = await electron.launch({ args: [root, '--ozone-platform=x11'], env: { ...process.env,
      SPARK_DATA_DIR: temporary, SPARK_E2E_MEDIA: '0' }, timeout: 45_000 });
    const page = await app.firstWindow();
    await expect(id(page, 'display-name')).toBeVisible();
    await page.evaluate(async () => window.sparkDesktop.setSettings({ ...await window.sparkDesktop.getSettings(), share_audio: true }));
    // Explicitly approve only this isolated test window via the real source IPC.
    await app.evaluate(async ({ BrowserWindow }) => {
      global.sparkCaptureTest = new BrowserWindow({ width: 400, height: 300, title: 'Spark Loopback Test' });
      await global.sparkCaptureTest.loadURL('data:text/html,<title>Spark Loopback Test</title><h1>Synthetic audio test</h1>');
    });
    await expect.poll(() => page.evaluate(async () => (await window.sparkDesktop.sources()).some(s => s.name === 'Spark Loopback Test'))).toBe(true);
    // PCM played by a separate process: Chromium must capture the real null sink.
    const pcm = Buffer.alloc(48000 * 2 * 2 * 30);
    for (let i = 0; i < pcm.length / 4; i++) {
      const value = Math.round(Math.sin(i * 2 * Math.PI * 523 / 48000) * 8000);
      pcm.writeInt16LE(value, i * 4); pcm.writeInt16LE(value, i * 4 + 2);
    }
    const audioFile = path.join(temporary, 'tone.raw'); await fs.writeFile(audioFile, pcm);
    player = require('node:child_process').spawn('paplay', ['--raw', '--rate=48000', '--channels=2', '--format=s16le', audioFile], { env: process.env });
    const result = await page.evaluate(async () => {
      const source = (await window.sparkDesktop.sources()).find(s => s.name === 'Spark Loopback Test');
      await window.sparkDesktop.selectSource(source.id);
      const stream = await navigator.mediaDevices.getDisplayMedia({ video: true, audio: { restrictOwnAudio: true, echoCancellation: false, noiseSuppression: false, autoGainControl: false } });
      const context = new AudioContext(); await context.resume();
      const analyser = context.createAnalyser(); analyser.fftSize = 2048;
      context.createMediaStreamSource(stream).connect(analyser);
      window.sparkLoopback = { stream, context, analyser };
      return stream.getTracks().map(t => ({ kind: t.kind, state: t.readyState, settings: t.getSettings() }));
    });
    await fs.writeFile(info.outputPath('linux-loopback-tracks.json'), JSON.stringify(result, null, 2));
    expect(result.filter(t => t.kind === 'audio' && t.state === 'live')).toHaveLength(1);
    await expect.poll(() => page.evaluate(() => {
      const data = new Float32Array(2048); window.sparkLoopback.analyser.getFloatTimeDomainData(data);
      return Math.sqrt(data.reduce((sum, v) => sum+v*v, 0)/data.length);
    })).toBeGreaterThan(0.02);
    await fs.writeFile(info.outputPath('linux-loopback-tracks.json'), JSON.stringify(result, null, 2));
    player.kill();
    await page.evaluate(() => {
      const tone = window.sparkLoopback.context.createOscillator();
      const gain = window.sparkLoopback.context.createGain(); gain.gain.value = 0.2;
      tone.connect(gain).connect(window.sparkLoopback.context.destination); tone.start();
      window.sparkLoopback.ownTone = tone;
    });
    // Linux Chromium currently captures Spark playback too. Record and verify
    // that limitation: sharing is opt-in and the source picker explains it.
    const ownRms = () => page.evaluate(() => {
      const data = new Float32Array(2048); window.sparkLoopback.analyser.getFloatTimeDomainData(data);
      return Math.sqrt(data.reduce((sum, v) => sum+v*v, 0)/data.length);
    });
    const restrictsOwnAudio = result.find(t => t.kind === 'audio').settings.restrictOwnAudio === true;
    if (restrictsOwnAudio) await expect.poll(ownRms).toBeLessThan(0.005);
    else await expect.poll(ownRms).toBeGreaterThan(0.03);
    await fs.writeFile(info.outputPath('linux-loopback-own-audio.json'), JSON.stringify({ restrictsOwnAudio, rms: await ownRms() }));
    await page.evaluate(() => window.sparkLoopback.ownTone.stop());

    await page.evaluate(async () => { window.sparkLoopback.stream.getTracks().forEach(t => t.stop()); await window.sparkLoopback.context.close(); });
  } finally {
    player?.kill();
    await app?.close().catch(() => {});
    await fs.rm(temporary, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
});
