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
  page.on('pageerror', e => errors.push(e.message));
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
      await id(a.page, 'screen').click();
      const choice = a.page.locator('.source').filter({ hasText: 'Spark Capture Test' });
      await expect(choice).toBeVisible();
      await choice.click();
      const screen = await assertRemoteVideo(b.page, aliceId, 'screen');
      // Decode actual received pixels: lower centre must be green, not a black/video placeholder.
      await expect.poll(() => screen.evaluate(video => {
        const canvas = document.createElement('canvas'); canvas.width = 16; canvas.height = 16;
        const ctx = canvas.getContext('2d'); ctx.drawImage(video, 0, 0, 16, 16);
        const [r, g, b] = ctx.getImageData(8, 12, 1, 1).data;
        return g > 120 && g > r * 1.6 && g > b * 1.4;
      })).toBe(true);
      await expect(id(b.page, `video-${aliceId}-camera`)).toBeVisible();
      await b.page.screenshot({ path: info.outputPath('04-real-screen-and-camera.png') });
      await id(a.page, 'screen').click();
      await expect(id(b.page, `video-${aliceId}-screen`)).toHaveCount(0);
      await a.app.evaluate(() => { global.sparkCaptureTest?.destroy(); delete global.sparkCaptureTest; });
    }

    await fs.writeFile(info.outputPath('webrtc-stats.json'), JSON.stringify({ alice: await stats(a.page), bob: await stats(b.page) }, null, 2));
    await id(a.page, 'leave-voice').click();
    await expect.poll(async () => (await snapshot(a.page)).localTracks.length).toBe(0);
    await expect.poll(async () => (await snapshot(b.page)).peers.length).toBe(0);
    await id(b.page, 'leave-voice').click();
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
    expect(errors).toEqual([]);
  } finally {
    for (const client of clients.reverse()) {
      try { const windows = client.windows(); if (windows[0]) await windows[0].screenshot({ path: info.outputPath(`final-${clients.indexOf(client)}.png`) }); } catch {}
      await client.close().catch(() => {});
    }
    await fs.writeFile(info.outputPath('renderer-errors.json'), JSON.stringify(errors, null, 2));
    await fs.rm(temporary, { recursive: true, force: true });
  }
});
