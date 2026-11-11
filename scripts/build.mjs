import { spawnSync } from 'node:child_process';
import { mkdirSync, copyFileSync, chmodSync, existsSync } from 'node:fs';
import path from 'node:path';
import os from 'node:os';
const root = path.resolve(import.meta.dirname, '..');
const env = { ...process.env, PATH: path.join(os.homedir(), '.cargo', 'bin') + path.delimiter + process.env.PATH };
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, env, stdio: 'inherit', shell: false });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status || 1);
}
run('cargo', ['build', '--release', '--locked', '-p', 'spark-ui', '--target', 'wasm32-unknown-unknown']);
run('wasm-bindgen', ['--target', 'web', '--out-dir', 'assets/pkg', '--out-name', 'spark_ui',
  'target/wasm32-unknown-unknown/release/spark_ui.wasm']);
run('cargo', ['build', '--release', '--locked', '-p', 'spark-service']);
mkdirSync(path.join(root, 'desktop', 'binaries'), { recursive: true });
const name = process.platform === 'win32' ? 'spark-service.exe' : 'spark-service';
const source = path.join(root, 'target', 'release', name);
if (!existsSync(source)) throw new Error('Native Rust binary missing: ' + source);
const destination = path.join(root, 'desktop', 'binaries', name);
copyFileSync(source, destination);
if (process.platform !== 'win32') chmodSync(destination, 0o755);
console.log('Spark: Rust service and Rust/WASM UI built.');
