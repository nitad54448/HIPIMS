// End-to-end UI test: real service (simulator) + headless Edge/Chrome driven over the DevTools protocol.
//   node tests/ui_e2e.mjs [--shots <dir>] [--browser <path>]
// Fails on any console error / uncaught exception / failed assertion. Needs Node >= 22.
import { spawn } from 'node:child_process';
import { mkdtempSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const argv = process.argv.slice(2);
const arg = (k, d) => { const i = argv.indexOf(k); return i >= 0 ? argv[i + 1] : d; };
const shots = arg('--shots', null);
if (shots) mkdirSync(shots, { recursive: true });
const browserPath = arg('--browser', [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe', 'C:/Program Files/Google/Chrome/Application/chrome.exe',
  '/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/microsoft-edge'].find(existsSync));
if (!browserPath) { console.log('no Chrome/Edge found: UI test skipped'); process.exit(0); }

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const work = mkdtempSync(join(tmpdir(), 'hq-e2e-'));
const PORT = 18765, DBG = 19333;
writeFileSync(join(work, 'cfg.toml'), `data_dir = ${JSON.stringify(join(work, 'data'))}\n[server]\nport = ${PORT}\nopen_browser = false\n[sim]\nrealtime = false\n`);

const exe = resolve(process.cwd(), 'target/debug/hipims-qcm' + (process.platform === 'win32' ? '.exe' : ''));
const svc = spawn(exe, ['--config', join(work, 'cfg.toml'), '--sim', '--no-browser'], { stdio: ['ignore', 'pipe', 'pipe'] });
let svcOut = '', svcExited = null;
svc.stdout.on('data', (d) => { svcOut += d; });
svc.stderr.on('data', (d) => { svcOut += d; });
svc.on('exit', (c) => { svcExited = c; });

const failures = [];
const fail = (m) => { failures.push(m); console.log('  FAIL:', m); };
const ok = (c, m) => { if (!c) fail(m); else console.log('  ok  :', m); };
let browser;
const cleanup = () => { try { browser && browser.kill(); } catch (e) { /* */ } try { svc.kill(); } catch (e) { /* */ } };
process.on('exit', cleanup);

try {
  let url = null;
  for (let i = 0; i < 100 && !url; i++) { await sleep(100); const m = svcOut.match(/http:\/\/127\.0\.0\.1:\d+\/#token=\w+(&sim=1)?/); if (m) url = m[0]; }
  if (!url) throw new Error('service did not start:\n' + svcOut);
  console.log('service up:', url.replace(/token=\w+/, 'token=…'));

  browser = spawn(browserPath, [`--remote-debugging-port=${DBG}`, '--headless=new', '--disable-gpu', '--no-first-run', `--user-data-dir=${join(work, 'profile')}`, '--window-size=1500,1000', 'about:blank'], { stdio: 'ignore' });
  let target = null;
  for (let i = 0; i < 100 && !target; i++) { await sleep(150); try { const t = await (await fetch(`http://127.0.0.1:${DBG}/json`)).json(); target = t.find((x) => x.type === 'page'); } catch (e) { /* not yet */ } }
  if (!target) throw new Error('browser did not start');
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r, j) => { ws.onopen = r; ws.onerror = j; });
  let id = 0; const pending = new Map(); const consoleErrors = [];
  ws.onmessage = (m) => {
    const d = JSON.parse(m.data);
    if (d.id && pending.has(d.id)) { const p = pending.get(d.id); pending.delete(d.id); d.error ? p.j(new Error(JSON.stringify(d.error))) : p.r(d.result); }
    else if (d.method === 'Runtime.exceptionThrown') consoleErrors.push('exception: ' + (d.params.exceptionDetails.exception?.description || d.params.exceptionDetails.text));
    else if (d.method === 'Runtime.consoleAPICalled' && d.params.type === 'error') consoleErrors.push('console.error: ' + d.params.args.map((a) => a.value ?? a.description).join(' '));
    else if (d.method === 'Log.entryAdded' && d.params.entry.level === 'error') consoleErrors.push('log: ' + d.params.entry.text + ' ' + (d.params.entry.url || ''));
  };
  const send = (method, params = {}) => new Promise((r, j) => { const i = ++id; pending.set(i, { r, j }); ws.send(JSON.stringify({ id: i, method, params })); });
  const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); if (r.exceptionDetails) throw new Error('eval: ' + (r.exceptionDetails.exception?.description || r.exceptionDetails.text) + '\n' + expr); return r.result.value; };
  const waitFor = async (expr, what, ms = 20000) => { const t0 = Date.now(); while (Date.now() - t0 < ms) { try { if (await ev(expr)) return true; } catch (e) { /* page busy */ } await sleep(150); } fail('timeout waiting for ' + what + ' | message: ' + await ev("document.getElementById('msg').textContent + ' | hint: ' + document.getElementById('qHint').textContent + ' | qCal.disabled=' + document.getElementById('qCal').disabled + ' | busy=' + (S.status && S.status.busy) + ' | log: ' + [...document.getElementById('log').children].slice(-4).map(x => x.textContent).join(' // ')").catch(() => '?')); return false; };
  const click = (id_) => ev(`document.getElementById(${JSON.stringify(id_)}).click()`);
  const arm = async () => { if (!(await ev(`document.getElementById('armBtn').classList.contains('armed')`))) await click('armBtn'); };
  const tab = (t) => ev(`document.getElementById('tab-${t}').click()`);
  const shot = async (name) => { if (!shots) return; const r = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(join(shots, name + '.png'), Buffer.from(r.data, 'base64')); };
  const text = (id_) => ev(`document.getElementById(${JSON.stringify(id_)}).textContent`);
  await send('Page.enable'); await send('Runtime.enable'); await send('Log.enable');
  await send('Emulation.setDeviceMetricsOverride', { width: 1500, height: 1000, deviceScaleFactor: 1, mobile: false });
  await send('Page.navigate', { url });
  await waitFor(`document.getElementById('connTxt') && document.getElementById('connTxt').textContent !== 'Service…'`, 'UI boot');

  console.log('1 experiment / connect');
  ok(await ev(`!!document.getElementById('exitBtn')`), 'Exit button present');
  ok(await ev(`!document.getElementById('estopBtn').classList.contains('live')`), 'STOP is not red when nothing is connected');
  ok(await ev(`location.hash === ''`), 'token removed from the address bar');
  ok((await ev(`document.querySelectorAll('.tip').length`)) > 20, 'label tooltips generated');
  ok(await ev(`document.getElementById('swRun').disabled`), 'hardware button disabled before connect/arm');
  ok((await ev(`document.getElementById('devId').value`)) !== '' && (await ev(`document.getElementById('devPort').value`)) === '8004', 'device address fields are prefilled from the configuration');
  await ev(`document.getElementById('devId').value='dev 1'; document.getElementById('devId').dispatchEvent(new Event('input'))`);
  ok(await ev(`document.getElementById('connectSimBtn').disabled && document.getElementById('devHint').textContent.includes('device ID')`), 'invalid device ID disables Connect with an inline reason');
  await ev(`document.getElementById('devId').value='dev7777'; document.getElementById('devId').dispatchEvent(new Event('input'))`);
  await click('connectSimBtn');
  await waitFor(`document.getElementById('connTxt').textContent.includes('SIMULATOR')`, 'simulator connected');
  ok((await text('connTxt')).includes('dev7777') && await ev(`document.getElementById('devId').disabled`), 'connected to the typed address, field locked while connected');
  ok(await ev(`document.getElementById('swRun').disabled`), 'still disabled while not armed');
  ok((await text('swRun')) !== '' && /arm/.test(await ev(`document.querySelector('#p-sweep [data-reason]').textContent`)), 'reason shown: arm the outputs first');
  await ev(`document.getElementById('sampleId').value='Ti-E2E'; document.getElementById('operator').value='<i>e2e</i>'`);
  await click('metaSave');
  await shot('01-experiment');

  console.log('2 sweep');
  await click('armBtn');
  ok(await ev(`document.getElementById('armBtn').classList.contains('armed')`), 'armed state visible');
  ok(await ev(`!document.getElementById('estopBtn').classList.contains('live')`), 'STOP still neutral: connected but all outputs off');
  await tab('sweep');
  await ev(`document.getElementById('swDrive').value='1.2'; document.getElementById('swDrive').dispatchEvent(new Event('input'))`);
  ok(await ev(`document.getElementById('swRun').disabled && document.getElementById('swHint').textContent.includes('limit')`), 'over-limit drive disables Run with an inline reason');
  await ev(`document.getElementById('swDrive').value='0.05'; document.getElementById('swDrive').dispatchEvent(new Event('input'))`);
  ok(await ev(`!document.getElementById('swRun').disabled`), 'valid parameters enable Run');
  await click('swRun');
  await waitFor(`/Q/.test(document.getElementById('resInfo').textContent) && S.sweep`, 'sweep result');
  const resTxt = await text('resInfo');
  console.log('  resonance:', resTxt.replace(/\s+/g, ' ').slice(0, 160));
  ok(await ev(`Math.abs(S.sweep.lorentz.q/50000-1) < 0.1`), 'Lorentz Q within 10 % of 50 000');
  ok(await ev(`P.R.series.length >= 2 && P.Ny.series.length >= 2`), 'R and Nyquist plots populated (with fit overlays)');
  ok(await ev(`!document.getElementById('plTh').parentElement.hidden && document.getElementById('plNy').parentElement.hidden`), 'Lorentz selected: amplitude + phase plots');
  await ev(`document.querySelector('input[name=fit][value=Circle]').click()`);
  ok(await ev(`document.getElementById('plTh').parentElement.hidden && !document.getElementById('plNy').parentElement.hidden`), 'Circle selected: amplitude + Nyquist plots');
  await click('fitApply');
  await waitFor(`S.status.estimate && S.status.estimate.method === 'Circle'`, 'circle fit selected');
  await waitFor(`document.getElementById('estopBtn').classList.contains('live')`, 'STOP turns red once the drive output is on');
  await shot('02-sweep');

  console.log('3 PLL + recording');
  await tab('pll'); await arm();
  await click('pllAdv');
  await waitFor(`document.getElementById('pllAdvice').textContent.includes('Advisor')`, 'advisor result');
  await click('pllStart');
  await waitFor(`document.getElementById('pllTxt').textContent === 'PLL locked'`, 'PLL lock');
  ok(await ev(`document.getElementById('pllLed').classList.contains('ok')`), 'PLL LED green');
  await click('trStart');
  await waitFor(`S.live.t.length > 50`, 'live samples over the WebSocket');
  await sleep(800);
  await shot('03-pll');

  console.log('4 Q control');
  await tab('q'); await arm();
  await click('pllStop'); // the calibration sweeps move the oscillator: PLL must be off
  await waitFor(`!S.status.pll_active`, 'PLL off');
  await ev(`document.getElementById('qGains').value='-4, -2, 0, 2, 4'; document.getElementById('qPts').value='200'; document.getElementById('qGains').dispatchEvent(new Event('input'))`);
  await click('qCal');
  await waitFor(`S.qcal && /Lasing/.test(document.getElementById('qInfo').textContent)`, 'Q calibration', 60000);
  ok(await ev(`S.qcal.slope < 0 && S.qcal.points.filter(p => p.gamma).length === 5`), 'five effective-resonance sweeps fitted, negative slope (gain > 0 raises Q)');
  ok(await ev(`Math.abs(S.qcal.q_native / 50000 - 1) < 0.1`), 'native Q from the calibration within 10 % of 50 000');
  ok(await ev(`P.qD.series.length === 5`), 'effective resonance curves plotted');
  await ev(`document.getElementById('qTarget').value='70000'`);
  await arm(); await click('pllStart'); // PLL back on: engage rescales its gains and checks the bandwidth rule
  await waitFor(`S.status.pll_active && S.status.pll.state === 'locked'`, 'PLL locked again');
  await click('qEngage');
  await waitFor(`S.status.q_engaged`, 'Q engaged');
  ok(await ev(`document.getElementById('qWarn').textContent.includes('PLL bandwidth')`), 'blog rule PLL BW > Q-control BW is flagged');
  await shot('04-qcontrol');
  await click('qOff');

  console.log('5 HiPIMS');
  await tab('hip'); await arm();
  for (const s of [-400, -330, -250, -150, -50, 0]) {
    await ev(`document.getElementById('hStart').value=${s}; document.getElementById('hPulses').value=100`);
    const n = await ev(`S.steps.length`);
    await click('hStep');
    await waitFor(`S.steps.length === ${n + 1}`, 'step at ' + s);
  }
  await click('hProfile');
  await waitFor(`S.profile && S.profile.points.length === 6`, 'profile');
  ok(await ev(`Math.abs(S.profile.points[S.profile.points.length-1].delta_f_hz + 25) < 4`), 'full-overlap shift ≈ -25 Hz (simulated pulse)');
  await shot('05-hipims');

  console.log('6 analysis');
  await click('trStop');
  await waitFor(`!S.status.tracking`, 'recording stopped');
  await tab('ana');
  await waitFor(`document.getElementById('anTrack').options.length > 0`, 'track listed');
  await click('anLoad');
  await waitFor(`S.series && S.series.t_s.length > 5`, 'track loaded');
  await click('anCalc');
  await waitFor(`document.getElementById('outRate').textContent !== '—'`, 'rate computed');
  ok(await ev(`parseFloat(document.getElementById('outRate').textContent) > 0`), 'positive deposition rate from the simulated drift');
  await ev(`document.getElementById('iffRef').value='1'`);
  await click('iffCalc');
  await waitFor(`document.getElementById('iffOut').textContent.includes('%')`, 'IFF');
  await shot('06-analysis');

  console.log('7 log, theme, project');
  await tab('log');
  ok((await ev(`document.getElementById('log').children.length`)) > 5, 'session log populated');
  ok(await ev(`!document.getElementById('history').innerHTML.includes('<i>e2e</i>')`), 'untrusted text is not rendered as markup');
  await click('themeBtn'); await sleep(300);
  const theme = await ev(`document.documentElement.getAttribute('data-theme')`);
  ok(theme === 'dark' || theme === 'light', 'theme toggled: ' + theme);
  await shot('07-log-' + theme);
  await tab('sweep'); await sleep(300); await shot('08-sweep-' + theme);
  await click('projSave');
  await waitFor(`document.getElementById('saveTxt').textContent.startsWith('All changes saved')`, 'project saved');

  ok(consoleErrors.length === 0, 'no console errors / exceptions' + (consoleErrors.length ? ': ' + consoleErrors.join(' | ') : ''));

  console.log('8 exit');
  await click('exitBtn');
  await waitFor(`document.getElementById('dlg').open`, 'confirmation dialog');
  await click('dlgOk');
  for (let i = 0; i < 100 && svcExited === null; i++) await sleep(100);
  ok(svcExited === 0, 'service exited cleanly after Exit (code ' + svcExited + ')');
  ok(await ev(`document.body.textContent.includes('Service stopped')`).catch(() => true), 'UI shows "Service stopped"');
} catch (e) {
  fail('exception: ' + e.message);
} finally {
  cleanup();
}
console.log(failures.length ? `\n${failures.length} FAILURE(S)` : '\nUI E2E PASSED');
process.exit(failures.length ? 1 : 0);
