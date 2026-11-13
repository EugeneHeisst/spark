'use strict';
const { spawn, execFile } = require('node:child_process');
const { promisify } = require('node:util');
const run = promisify(execFile);
const appId = 'dev.eugeneheisst.spark';
function createShareAudio() {
  let current;
  function stop() {
    const state = current; current = undefined;
    if (!state) return;
    clearTimeout(state.timer);
    state.child.stdin.end(); // Parent death also closes stdin; the watchdog stops capture.
    state.child.kill(); // Removing the private ports also removes all their links.
  }
  async function graph() {
    return JSON.parse((await run('pw-dump', [], { timeout: 3000, maxBuffer: 8 * 1024 * 1024 })).stdout);
  }
  async function reconcile(state) {
    const nodes = await graph();
    if (current !== state) return;
    const props = n => n.info?.props || {};
    const input = nodes.find(n => n.type.endsWith(':Node') && props(n)['node.name'] === state.input);
    if (!input) return;
    const ports = nodes.filter(n => n.type.endsWith(':Port'));
    const inputs = ports.filter(n => String(props(n)['node.id']) === String(input.id) && props(n)['port.direction'] === 'in');
    const allowed = new Set(nodes.filter(n => n.type.endsWith(':Node') && props(n)['media.class'] === 'Stream/Output/Audio'
      && props(n)['application.id'] !== appId && props(n)['node.name'] !== state.output).map(n => String(n.id)));
    const links = new Set(nodes.filter(n => n.type.endsWith(':Link')).map(n => `${n.info['output-port-id']}:${n.info['input-port-id']}`));
    for (const output of ports.filter(n => allowed.has(String(props(n)['node.id'])) && props(n)['port.direction'] === 'out')) {
      const targets = inputs.filter(n => props(n)['audio.channel'] === props(output)['audio.channel'] || props(output)['audio.channel'] === 'MONO');
      for (const inputPort of targets) {
        if (links.has(`${output.id}:${inputPort.id}`) || current !== state) continue;
        await run('pw-link', [String(output.id), String(inputPort.id)], { timeout: 2000 });
      }
    }
  }
  async function start() {
    if (current) throw new Error('Захват звука уже запущен');
    const id = require('node:crypto').randomUUID();
    const label = `Spark Share ${id}`;
    const state = { input: `spark_share_in_${id}`, output: `spark_share_out_${id}` };
    state.child = spawn('sh', ['-c', `"$@" & child=$!; trap 'kill "$child" 2>/dev/null; wait "$child"' EXIT; trap 'exit 0' TERM INT; read -r stop || :`, 'spark-share-audio', 'pw-loopback', '--channels=2', '--latency=30',
      '--capture-props', JSON.stringify({ 'node.name': state.input, 'node.autoconnect': false, 'node.passive': true }),
      '--playback-props', JSON.stringify({ 'node.name': state.output, 'node.description': label, 'media.class': 'Audio/Source', 'node.virtual': true, 'priority.session': 0, 'node.autoconnect': false })],
    { stdio: ['pipe', 'ignore', 'pipe'] });
    current = state;
    let failed = false;
    state.child.stdin.on('error', () => {});
    state.child.on('error', () => { failed = true; });
    state.child.stderr.on('data', () => {});
    try {
      for (let i = 0; i < 40; i++) {
        if (current !== state || failed || state.child.exitCode !== null) throw new Error('Для исключения звука Spark нужны PipeWire, pw-loopback, pw-link и pactl.');
        const { stdout } = await run('pactl', ['--format=json', 'list', 'sources'], { timeout: 2000, maxBuffer: 1024 * 1024 });
        if (JSON.parse(stdout).some(s => s.description === label)) {
          await reconcile(state);
          const tick = async () => {
            try { await reconcile(state); }
            catch { if (current === state) stop(); return; }
            if (current === state) state.timer = setTimeout(tick, 500);
          };
          state.timer = setTimeout(tick, 500);
          return label;
        }
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      throw new Error('Источник звука демонстрации не появился');
    } catch (error) { if (current === state) stop(); throw error; }
  }
  return { start, stop };
}
module.exports = { createShareAudio, appId };
