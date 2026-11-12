// Private PipeWire/Pulse server: no hardware modules or real user audio socket.
import { spawn, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, openSync, closeSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
if (process.platform !== 'linux') throw new Error('Linux only');
const temporary = mkdtempSync(path.join(os.tmpdir(), 'spark-loopback-'));
const runtime = path.join(temporary, 'runtime'); mkdirSync(runtime, { mode: 0o700 });
mkdirSync(path.join(runtime, 'pulse'), { mode: 0o700 });
const env = { ...process.env, XDG_RUNTIME_DIR: runtime, PIPEWIRE_RUNTIME_DIR: runtime,
  PULSE_RUNTIME_PATH: path.join(runtime, 'pulse'), PULSE_SERVER: `unix:${runtime}/pulse/native`,
  SPARK_ISOLATED_AUDIO: '1', XDG_CONFIG_HOME: path.join(temporary, 'config'),
  XDG_STATE_HOME: path.join(temporary, 'state') };
const config = path.join(temporary, 'pipewire.conf');
writeFileSync(config, `
context.properties = { core.daemon = true core.name = pipewire-0 }
context.spa-libs = { audio.convert.* = audioconvert/libspa-audioconvert support.* = support/libspa-support }
context.modules = [
 { name = libpipewire-module-protocol-native }
 { name = libpipewire-module-metadata }
 { name = libpipewire-module-spa-node-factory }
 { name = libpipewire-module-client-node }
 { name = libpipewire-module-access }
 { name = libpipewire-module-adapter }
 { name = libpipewire-module-link-factory }
]
context.objects = [
 { factory = metadata args = { metadata.name = default } }
 { factory = spa-node-factory args = { factory.name = support.node.driver node.name = Dummy-Driver priority.driver = 20000 } }
]
`);
const children = [];
function start(cmd, args) {
  mkdirSync('artifacts', { recursive: true });
  const fd = openSync(`artifacts/loopback-${cmd}.log`, 'w');
  const child = spawn(cmd, args, { env, stdio: ['ignore', fd, fd] }); closeSync(fd);
  children.push(child); return child;
}
async function run(cmd, args) {
  const child = spawn(cmd, args, { env, stdio: 'inherit' }); children.push(child);
  return new Promise((resolve, reject) => { child.on('error', reject); child.on('exit', code => resolve(code)); });
}
try {
  start('pipewire', ['-c', config]);
  start('wireplumber', ['--profile=policy']); // policy only: no ALSA/Bluetooth/cameras
  start('pipewire-pulse', []);
  let ready = false;
  for (let i = 0; i < 50; i++) {
    if (spawnSync('pactl', ['info'], { env, stdio: 'ignore' }).status === 0) { ready = true; break; }
    await delay(100);
  }
  if (!ready) throw new Error('Private Pulse server did not start');
  const loaded = spawnSync('pactl', ['load-module', 'module-null-sink', 'sink_name=spark_test', 'channels=2'], { env, encoding: 'utf8' });
  if (loaded.status !== 0) throw new Error(loaded.stderr);
  if (spawnSync('pactl', ['set-default-sink', 'spark_test'], { env }).status !== 0) throw new Error('Cannot select test sink');
  process.exitCode = await run(process.execPath, ['scripts/test-linux.mjs', '--grep', 'isolated system loopback', '--output', 'artifacts/loopback', '--reporter', 'list']);
} finally {
  for (const child of children.reverse()) {
    if (child.exitCode === null) {
      child.kill('SIGTERM');
      await Promise.race([new Promise(resolve => child.once('exit', resolve)), delay(1500)]);
      if (child.exitCode === null) child.kill('SIGKILL');
    }
  }
  rmSync(temporary, { recursive: true, force: true });
}
