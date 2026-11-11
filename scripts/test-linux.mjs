import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
if (process.platform !== 'linux') throw new Error('Use npm run test:e2e on Windows');
const temporary = mkdtempSync(path.join(os.tmpdir(), 'spark-desktop-test-'));
const env = { ...process.env, WAYLAND_DISPLAY: '', XDG_SESSION_TYPE: 'x11',
  XDG_CURRENT_DESKTOP: 'SparkTest', QT_QPA_PLATFORM: 'xcb', NO_AT_BRIDGE: '1',
  SPARK_ISOLATED_DISPLAY: '1' };
for (const [key, directory] of Object.entries({ XDG_RUNTIME_DIR: 'runtime', XDG_CONFIG_HOME: 'config',
  XDG_CACHE_HOME: 'cache', XDG_DATA_HOME: 'data' })) {
  env[key] = path.join(temporary, directory);
  mkdirSync(env[key], { mode: 0o700 });
}
try {
  const result = spawnSync('dbus-run-session', ['--', 'xvfb-run', '-a', '-s', '-screen 0 1600x1000x24',
    'bash', 'scripts/e2e-linux.sh'], { cwd: path.resolve(import.meta.dirname, '..'), env, stdio: 'inherit' });
  if (result.error) throw result.error;
  process.exitCode = result.status || (result.signal ? 1 : 0);
} finally {
  rmSync(temporary, { recursive: true, force: true, maxRetries: 5 });
}
