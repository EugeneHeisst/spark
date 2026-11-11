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

test('two independent desktops: chat, files, actual RTP/video/screen, reconnect and durable history', async ({}, info) => {
  const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'spark-e2e-'));
  const clients = [];
  const errors = [];
  let a, b;
  try {
    a = await launch(path.join(temporary, 'alice'), path.join(temporary, 'saved-alice'), errors); clients.push(a.app);
    b = await launch(path.join(temporary, 'bob'), path.join(temporary, 'saved-bob'), errors); clients.push(b.app);
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

    await send(a.page, 'Привет из первого процесса! <script>это просто текст</script>');
    await expect(b.page.locator('#message-list')).toContainText('Привет из первого процесса!');
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
    expect((await snapshot(b.page))?.channelId ?? null).toBeNull();
    await id(b.page, 'accept-invite').click();
    await expect.poll(async () => (await snapshot(b.page))?.channelId).toBe('lounge');
    await expect.poll(async () => inbound(await stats(a.page), 'audio')).toBeGreaterThan(200);
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(200);
    const first = inbound(await stats(b.page), 'audio');
    await expect.poll(async () => inbound(await stats(b.page), 'audio')).toBeGreaterThan(first + 200);
    await id(a.page, 'mute').click();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.enabled).toBe(false);
    await id(a.page, 'mute').click();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.find(t => t.kind === 'audio')?.enabled).toBe(true);
    await id(b.page, 'deafen').click();
    await expect.poll(async () => (await snapshot(b.page)).deafened).toBe(true);
    await expect.poll(() => b.page.locator('audio').evaluateAll(els => els.length > 0 && els.every(e => e.srcObject?.getAudioTracks().every(t => !t.enabled)))).toBe(true);
    await id(b.page, 'deafen').click();
    await expect.poll(() => b.page.locator('audio').evaluateAll(els => els.length > 0 && els.every(e => e.srcObject?.getAudioTracks().every(t => t.enabled)))).toBe(true);

    await id(a.page, 'camera').click();
    await assertRemoteVideo(b.page, aliceId, 'camera');
    await id(b.page, 'camera').click();
    await assertRemoteVideo(a.page, bobId, 'camera');
    await retainCapture(a.page, 'camera');
    await retainCapture(b.page, 'camera');
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
