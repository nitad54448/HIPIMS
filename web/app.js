'use strict';
/* QCM HiPIMS workstation UI. No external dependencies; all values from the service are
   rendered with textContent (never as markup). */

const $ = (id) => document.getElementById(id);
const el = (tag, attrs, ...kids) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'class') e.className = v; else if (v !== false && v != null) e.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids) e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  return e;
};
const num = (id) => { const s = $(id).value.trim(); return s !== '' && Number.isFinite(Number(s)) ? Number(s) : NaN; };
const fmt = (v, d = 3) => (Number.isFinite(v) ? v.toFixed(d) : '—');
const fmtSI = (v, unit = '') => {
  if (!Number.isFinite(v)) return '—';
  const a = Math.abs(v);
  if (a !== 0 && (a < 1e-3 || a >= 1e6)) return v.toExponential(3) + unit;
  return v.toPrecision(5).replace(/\.?0+$/, '') + unit;
};
const alignHead = (t) => {
  const rows = t.rows; if (rows.length > 1) [...rows[0].cells].forEach((c, i) => { const d = rows[1].cells[i]; if (d && d.classList.contains('num')) c.classList.add('num'); });
  return t;
};
const clockTime = (iso) => { try { return new Date(iso).toLocaleTimeString(); } catch (e) { return iso; } };

const S = {
  token: null, ws: null, status: null, project: null, config: null,
  armedUntil: 0, sweep: null, qcal: null, steps: [], profile: null,
  series: null, live: { t: [], f: [], ph: [] }, logLines: [], materials: [], job: null,
};

/* ------------------------------------------------------------------ token + API */
(function initToken() {
  const h = new URLSearchParams(location.hash.replace(/^#/, ''));
  let t = h.get('token');
  try {
    if (t) { sessionStorage.setItem('hq-token', t); history.replaceState(null, '', location.pathname); } else { t = sessionStorage.getItem('hq-token'); }
  } catch (e) { /* storage blocked: token only lives for this page */ }
  S.token = t;
})();

async function api(path, body, method) {
  const opt = { method: method || (body === undefined ? 'GET' : 'POST'), headers: { 'x-hq-token': S.token || '' } };
  if (body !== undefined) { opt.headers['Content-Type'] = 'application/json'; opt.body = JSON.stringify(body); }
  let r;
  try { r = await fetch(path, opt); } catch (e) { throw new Error('service unreachable'); }
  const ct = r.headers.get('content-type') || '';
  const data = ct.includes('json') ? await r.json() : await r.text();
  if (!r.ok) throw new Error((data && data.error) || r.statusText);
  return data;
}

/* ------------------------------------------------------------------ notices / log */
function notify(level, text) {
  const m = $('msg');
  m.textContent = text; m.className = level === 'error' ? 'err' : level === 'warn' ? 'warn' : level === 'ok' ? 'ok' : '';
}
function addLog(line, level) {
  S.logLines.push({ line, level });
  if (S.logLines.length > 1500) S.logLines.shift();
  const box = $('log'); if (!box) return;
  const atEnd = box.scrollTop + box.clientHeight >= box.scrollHeight - 20;
  box.append(el('div', { class: level || '' }, line));
  while (box.childElementCount > 1500) box.firstChild.remove();
  if (atEnd) box.scrollTop = box.scrollHeight;
}
async function act(btn, fn) {
  if (btn) btn.disabled = true;
  try { return await fn(); } catch (e) { notify('error', e.message); addLog(new Date().toISOString() + ' [error] ' + e.message, 'error'); return null; } finally { refreshControls(); }
}

/* ------------------------------------------------------------------ plots */
const cssv = (n) => getComputedStyle(document.documentElement).getPropertyValue(n).trim() || '#888';
function niceStep(range, target) {
  const raw = range / Math.max(1, target); const p = Math.pow(10, Math.floor(Math.log10(raw))); const f = raw / p;
  return (f < 1.5 ? 1 : f < 3 ? 2 : f < 7 ? 5 : 10) * p;
}
function tickLabel(v, step) {
  if (v === 0) return '0';
  const a = Math.abs(v);
  if (a < 1e-3 || a >= 1e9) return v.toExponential(2);
  const dec = Math.max(0, Math.min(8, -Math.floor(Math.log10(step)) + (step < 1 ? 0 : 0)));
  return v.toFixed(dec);
}
class Plot {
  constructor(id, opts) {
    this.c = $(id); this.box = this.c.parentElement; this.ro = this.box.querySelector('.readout');
    this.o = Object.assign({ cursors: false, onCursors: null, xlabel: '' }, opts || {});
    this.series = []; this.view = null; this.cur = null; this.drag = null;
    const c = this.c;
    c.addEventListener('mousemove', (e) => this.move(e));
    c.addEventListener('mouseleave', () => { if (this.ro) this.ro.textContent = ''; });
    c.addEventListener('wheel', (e) => this.wheel(e), { passive: false });
    c.addEventListener('mousedown', (e) => this.down(e));
    window.addEventListener('mouseup', (e) => this.up(e));
    c.addEventListener('dblclick', () => { this.view = null; this.draw(); });
    new ResizeObserver(() => this.draw()).observe(this.box);
  }
  set(series) { this.series = series.filter((s) => s && (s.vline !== undefined || (s.x && s.x.length))); this.draw(); }
  bounds() {
    let x0 = Infinity, x1 = -Infinity, y0 = Infinity, y1 = -Infinity;
    for (const s of this.series) {
      if (s.vline !== undefined) continue;
      for (let i = 0; i < s.x.length; i++) {
        const x = s.x[i], y = s.y[i]; if (!Number.isFinite(x) || !Number.isFinite(y)) continue;
        if (this.view && (x < this.view[0] || x > this.view[1])) continue;
        if (x < x0) x0 = x; if (x > x1) x1 = x; if (y < y0) y0 = y; if (y > y1) y1 = y;
      }
    }
    if (this.view) { x0 = this.view[0]; x1 = this.view[1]; }
    if (!(x1 > x0)) { x0 = Number.isFinite(x0) ? x0 - 1 : 0; x1 = x0 + 2; }
    if (!(y1 > y0)) { y0 = Number.isFinite(y0) ? y0 - 1 : 0; y1 = y0 + 2; }
    const pad = (y1 - y0) * 0.06; return { x0, x1, y0: y0 - pad, y1: y1 + pad };
  }
  geom() { const w = this.c.clientWidth, h = this.c.clientHeight; return { w, h, l: 62, r: 10, t: 20, b: 24 }; }
  draw() {
    const c = this.c, dpr = window.devicePixelRatio || 1, g = this.geom();
    if (!g.w || !g.h) return;
    if (c.width !== Math.round(g.w * dpr) || c.height !== Math.round(g.h * dpr)) { c.width = Math.round(g.w * dpr); c.height = Math.round(g.h * dpr); }
    const ctx = c.getContext('2d'); ctx.setTransform(dpr, 0, 0, dpr, 0, 0); ctx.clearRect(0, 0, g.w, g.h);
    const B = this.bounds(); this.B = B; this.g = g;
    const sx = (x) => g.l + (x - B.x0) / (B.x1 - B.x0) * (g.w - g.l - g.r);
    const sy = (y) => g.h - g.b - (y - B.y0) / (B.y1 - B.y0) * (g.h - g.t - g.b);
    this.sx = sx; this.sy = sy;
    ctx.font = '11px ' + cssv('--mono'); ctx.fillStyle = cssv('--mut'); ctx.strokeStyle = cssv('--grid'); ctx.lineWidth = 1;
    const xs = niceStep(B.x1 - B.x0, 6), ys = niceStep(B.y1 - B.y0, 5);
    ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
    for (let v = Math.ceil(B.y0 / ys) * ys; v <= B.y1; v += ys) { const y = Math.round(sy(v)) + 0.5; ctx.beginPath(); ctx.moveTo(g.l, y); ctx.lineTo(g.w - g.r, y); ctx.stroke(); ctx.fillText(tickLabel(v, ys), g.l - 5, y); }
    ctx.textAlign = 'center'; ctx.textBaseline = 'top';
    for (let v = Math.ceil(B.x0 / xs) * xs; v <= B.x1; v += xs) { const x = Math.round(sx(v)) + 0.5; ctx.beginPath(); ctx.moveTo(x, g.t); ctx.lineTo(x, g.h - g.b); ctx.stroke(); ctx.fillText(tickLabel(v, xs), x, g.h - g.b + 4); }
    ctx.save(); ctx.beginPath(); ctx.rect(g.l, g.t, g.w - g.l - g.r, g.h - g.t - g.b); ctx.clip();
    for (const s of this.series) {
      const col = cssv(s.color || '--s1');
      if (s.vline !== undefined) { ctx.strokeStyle = col; ctx.setLineDash([5, 4]); const x = sx(s.vline); ctx.beginPath(); ctx.moveTo(x, g.t); ctx.lineTo(x, g.h - g.b); ctx.stroke(); ctx.setLineDash([]); continue; }
      ctx.strokeStyle = col; ctx.fillStyle = col; ctx.lineWidth = s.width || 1.4;
      if (s.dots) { for (let i = 0; i < s.x.length; i++) { if (!Number.isFinite(s.x[i]) || !Number.isFinite(s.y[i])) continue; ctx.beginPath(); ctx.arc(sx(s.x[i]), sy(s.y[i]), 3, 0, 6.2832); ctx.fill(); } }
      if (!s.dots || s.line) {
        ctx.beginPath(); let pen = false;
        for (let i = 0; i < s.x.length; i++) { if (!Number.isFinite(s.x[i]) || !Number.isFinite(s.y[i])) { pen = false; continue; } const x = sx(s.x[i]), y = sy(s.y[i]); if (pen) ctx.lineTo(x, y); else { ctx.moveTo(x, y); pen = true; } }
        ctx.stroke();
      }
    }
    if (this.o.cursors && this.cur) { ctx.strokeStyle = cssv('--cursor'); ctx.lineWidth = 1.2; for (const v of this.cur) { const x = Math.round(sx(v)) + 0.5; ctx.beginPath(); ctx.moveTo(x, g.t); ctx.lineTo(x, g.h - g.b); ctx.stroke(); } }
    ctx.restore();
  }
  xAt(e) { const r = this.c.getBoundingClientRect(); const px = e.clientX - r.left; const B = this.B, g = this.g; return B.x0 + (px - g.l) / (g.w - g.l - g.r) * (B.x1 - B.x0); }
  move(e) {
    if (!this.B || !this.ro) return; const x = this.xAt(e); const s = this.series.find((q) => q.x && q.x.length); if (!s) return;
    let bi = 0, bd = Infinity; for (let i = 0; i < s.x.length; i++) { const d = Math.abs(s.x[i] - x); if (d < bd) { bd = d; bi = i; } }
    this.ro.textContent = 'x ' + fmtSI(s.x[bi]) + '  y ' + fmtSI(s.y[bi]);
    if (this.drag) this.dragMove(e);
  }
  wheel(e) {
    if (!this.B) return; e.preventDefault(); const x = this.xAt(e), f = e.deltaY < 0 ? 0.8 : 1.25;
    const cur = this.view || [this.B.x0, this.B.x1]; this.view = [x - (x - cur[0]) * f, x + (cur[1] - x) * f]; this.draw();
  }
  down(e) { if (!this.B) return; const pan = !this.o.cursors || e.ctrlKey; this.drag = { pan, x0: this.xAt(e), view: this.view || [this.B.x0, this.B.x1], startX: e.clientX }; if (!pan) { this.cur = [this.drag.x0, this.drag.x0]; } }
  dragMove(e) {
    const d = this.drag; if (!d) return;
    if (d.pan) { const w = this.g.w - this.g.l - this.g.r; const dx = (e.clientX - d.startX) / w * (d.view[1] - d.view[0]); this.view = [d.view[0] - dx, d.view[1] - dx]; }
    else { this.cur = [d.x0, this.xAt(e)]; }
    this.draw();
  }
  up() { const d = this.drag; this.drag = null; if (d && !d.pan && this.cur && this.o.onCursors) { const [a, b] = this.cur[0] <= this.cur[1] ? this.cur : [this.cur[1], this.cur[0]]; this.o.onCursors(a, b); } }
}
const P = {};
function initPlots() {
  P.R = new Plot('plR'); P.Th = new Plot('plTh'); P.Ny = new Plot('plNy');
  P.liveF = new Plot('plLiveF'); P.livePh = new Plot('plLivePh');
  P.qG = new Plot('plQGamma'); P.qD = new Plot('plQRes');
  P.hTr = new Plot('plHTr'); P.hProf = new Plot('plHProf');
  P.an = new Plot('plAn', { cursors: true, onCursors: (a, b) => { $('wA').value = a.toFixed(3); $('wB').value = b.toFixed(3); } });
}
const redrawAll = () => Object.values(P).forEach((p) => p.draw());

/* ------------------------------------------------------------------ tooltips, tabs, theme */
function initTips() {
  document.querySelectorAll('[data-tip]').forEach((n) => {
    const t = n.getAttribute('data-tip');
    const tip = el('span', { class: 'tip', tabindex: '0', role: 'note', 'aria-label': t, 'data-tip': t }, '?');
    n.append(' ', tip);
  });
}
const TABS = ['exp', 'sweep', 'pll', 'q', 'hip', 'ana', 'log'];
function showTab(id) {
  for (const t of TABS) {
    const sel = t === id; $('tab-' + t).setAttribute('aria-selected', String(sel)); $('tab-' + t).tabIndex = sel ? 0 : -1;
    $('p-' + t).hidden = !sel;
  }
  try { sessionStorage.setItem('hq-tab', id); } catch (e) { /* ignore */ }
  requestAnimationFrame(redrawAll);
}
function initTabs() {
  TABS.forEach((t, i) => {
    $('tab-' + t).addEventListener('click', () => showTab(t));
    $('tab-' + t).addEventListener('keydown', (e) => {
      if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') { const n = TABS[(i + (e.key === 'ArrowRight' ? 1 : TABS.length - 1)) % TABS.length]; showTab(n); $('tab-' + n).focus(); }
    });
  });
  let t = 'exp'; try { t = sessionStorage.getItem('hq-tab') || 'exp'; } catch (e) { /* ignore */ }
  showTab(TABS.includes(t) ? t : 'exp');
}
function initTheme() {
  $('themeBtn').addEventListener('click', () => {
    const cur = document.documentElement.getAttribute('data-theme') || (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');
    const next = cur === 'dark' ? 'light' : 'dark'; document.documentElement.setAttribute('data-theme', next);
    try { localStorage.setItem('hq-theme', next); } catch (e) { /* ignore */ }
    redrawAll();
  });
  window.addEventListener('storage', (e) => { if (e.key === 'hq-theme' && e.newValue) { document.documentElement.setAttribute('data-theme', e.newValue); redrawAll(); } });
}

/* ------------------------------------------------------------------ dialogs */
function confirmDialog(title, body, okLabel) {
  return new Promise((res) => {
    const d = $('dlg'); $('dlgTitle').textContent = title; $('dlgBody').textContent = body; $('dlgOk').textContent = okLabel;
    const done = (v) => { d.close(); $('dlgOk').onclick = null; $('dlgCancel').onclick = null; res(v); };
    $('dlgOk').onclick = () => done(true); $('dlgCancel').onclick = () => done(false); d.oncancel = () => done(false); d.showModal();
  });
}

/* ------------------------------------------------------------------ arm / controls */
const armed = () => Date.now() < S.armedUntil;
function setArmed(on) { S.armedUntil = on ? Date.now() + 60000 : 0; refreshControls(); }
function refreshControls() {
  const st = S.status || {}; const connected = !!st.connected; const busy = st.busy;
  const a = armed();
  $('armBtn').classList.toggle('armed', a);
  $('armBtn').textContent = a ? 'ARMED ' + Math.max(0, Math.ceil((S.armedUntil - Date.now()) / 1000)) + ' s' : 'Arm outputs';
  const reason = !connected ? 'not connected' : busy ? 'busy: ' + busy : !a ? 'arm the outputs first' : '';
  document.querySelectorAll('[data-hw]').forEach((b) => {
    const tabOk = tabValid(b.id); const why = reason || (tabOk === true ? '' : tabOk);
    b.disabled = !!why;
    const r = b.closest('.card') && b.closest('.card').querySelector('[data-reason]'); if (r && r.dataset.for !== 'other') r.textContent = why;
  });
  const devWhy = deviceProblem(); $('devHint').textContent = connected ? '' : devWhy;
  ['devId', 'devHost', 'devPort'].forEach((id) => { $(id).disabled = connected || !!busy; });
  $('connectBtn').disabled = connected || !!busy || !!devWhy; $('connectSimBtn').disabled = connected || !!busy || !!devWhy; $('disconnectBtn').disabled = !connected || !!busy;
  $('pllStop').disabled = !st.pll_active; $('pllAdv').disabled = !connected || !st.estimate || !!busy;
  $('trStart').disabled = !connected || !!st.tracking; $('trStop').disabled = !st.tracking;
  $('qOff').disabled = !st.q_engaged; $('fitApply').disabled = !S.sweep || !!busy;
  $('cancelBtn').hidden = !busy; $('progress').hidden = !busy;
}
setInterval(() => { if (S.armedUntil && !armed()) { S.armedUntil = 0; notify('warn', 'outputs disarmed (timeout)'); } if (S.armedUntil || $('armBtn').classList.contains('armed')) refreshControls(); }, 500);

/* validation: returns true or a reason string */
function tabValid(id) {
  const lim = (S.config && S.config.limits) || {};
  if (id === 'swRun') {
    const a = num('swStart'), b = num('swStop'), n = num('swPoints'), d = num('swDrive'), bw = num('swBw');
    const out = $('swDiv').checked ? 2 * d : d; let h = '';
    if (![a, b, n, d, bw].every(Number.isFinite)) h = 'enter all values'; else if (b <= a) h = 'stop must exceed start';
    else if (n < 3 || !Number.isInteger(n)) h = 'points: integer ≥ 3'; else if (d <= 0) h = 'drive must be positive';
    else if (lim.drive_max_v && out > lim.drive_max_v) h = 'output ' + out.toFixed(3) + ' V exceeds the ' + lim.drive_max_v + ' V limit';
    else if (lim.freq_min_hz && (a < lim.freq_min_hz || b > lim.freq_max_hz)) h = 'range outside ' + lim.freq_min_hz + '–' + lim.freq_max_hz + ' Hz';
    $('swHint').textContent = h; return h || true;
  }
  if (id === 'pllStart') { if (!(S.status && S.status.estimate)) return 'no resonance yet: sweep first'; if (!(num('pllBw') > 0)) return 'bandwidth must be positive'; return true; }
  if (id === 'qCal') {
    let h = '';
    if (!(S.status && S.status.estimate)) h = 'no resonance yet: sweep first'; else if (S.status.pll_active) h = 'switch the PLL off first (the sweeps move the oscillator)';
    else if (parseGains().length < 3) h = 'enter at least 3 gains'; else if (parseGains().some((g) => Math.abs(g) > (lim.q_gain_abs_max || Infinity))) h = 'a gain exceeds the limit ±' + lim.q_gain_abs_max;
    $('qHint').textContent = h; return h || true;
  }
  if (id === 'qEngage') { if (!(S.status && S.status.has_qcal)) return 'calibrate first'; return num('qTarget') > 0 ? true : 'target Q must be positive'; }
  if (id === 'hStep') {
    const w = num('hWin'), n = num('hPoints'), tc = num('hTc'), p = num('hPulses'); let h = '';
    if (![w, n, tc, p, num('hStart')].every(Number.isFinite)) h = 'enter all values'; else if (w <= 0 || p < 1) h = 'window and pulses must be positive';
    else if (tc > 10 * (w / n)) h = 'demod TC ≫ grid spacing: the window would be smeared';
    $('hHint').textContent = h; return h || true;
  }
  return true;
}
const parseGains = () => $('qGains').value.split(/[,;\s]+/).filter(Boolean).map(Number).filter(Number.isFinite);

/* ------------------------------------------------------------------ status */
function kv(dl, rows) { dl.replaceChildren(); for (const [k, v] of rows) { dl.append(el('dt', {}, k), el('dd', {}, v)); } }
function applyStatus(st) {
  S.status = st;
  $('connLed').className = 'led ' + (st.connected ? (st.simulated ? 'warn' : 'ok') : '');
  $('connTxt').textContent = st.connected ? (st.simulated ? 'SIMULATOR ' : '') + (st.device ? st.device.devtype + ' ' + st.device.device : 'connected') : 'Not connected';
  const b = $('busyTxt'); b.textContent = st.busy ? st.busy : 'idle'; b.className = 'badge' + (st.busy ? ' warn' : '');
  const live = !!(st.connected && (st.outputs_on || st.pll_active || st.q_engaged || st.tracking || st.busy));
  $('estopBtn').classList.toggle('live', live);
  $('estopBtn').title = live ? 'Switch every output and loop off now (Ctrl+Shift+S)' : 'Nothing is running: outputs are off (still works as a safety switch, Ctrl+Shift+S)';
  const pll = st.pll; const lock = pll && pll.state === 'locked';
  $('pllLed').className = 'led ' + (st.pll_active ? (lock ? 'ok' : 'err') : '');
  $('pllTxt').textContent = !st.pll_active ? 'PLL off' : lock ? 'PLL locked' : 'PLL NOT locked';
  if (st.device && st.connected) { $('devId').value = st.device.device; }
  kv($('devInfo'), st.device && st.connected ? [['Device', st.device.device], ['Type', st.device.devtype], ['Serial', st.device.serial], ['Options', (st.device.options || []).join(', ')], ['Firmware', st.device.firmware], ['Mode', st.device.simulated ? 'SIMULATED' : 'hardware']] : []);
  if (pll) kv($('pllInfo'), [['State', pll.state], ['Frequency', fmt(pll.freq_hz, 3) + ' Hz'], ['Phase error', fmt(pll.phase_error_deg, 2) + '°'], ['At limit', pll.at_limit ? 'YES' : 'no'], ['P / I / D', [pll.p, pll.i, pll.d].map((v) => fmtSI(v)).join(' / ')], ['Bandwidth', fmt(pll.bandwidth_hz, 1) + ' Hz (' + (pll.advisor_from_device ? 'LabOne advisor' : 'local model') + ')'], ['Slope', pll.polarity]]);
  else kv($('pllInfo'), []);
  kv($('trInfo'), st.tracking ? [['File', st.track_file], ['Samples', String(st.track_samples)]] : [['Recording', 'off']]);
  if (st.q_engaged) {
    $('qEngaged').textContent = 'Engaged: target Q ' + fmt(st.q_engaged.q_target, 0) + ', gain ' + fmt(st.q_engaged.gain, 3) + ' (P = ' + fmt(-st.q_engaged.gain, 3) + ')' + (st.q_engaged.agc_setpoint_v ? ', AGC ' + fmtSI(st.q_engaged.agc_setpoint_v, ' V') : '');
    $('qWarn').replaceChildren(...(st.q_engaged.warnings || []).map((w) => el('div', { class: 'warnbox' }, w)));
  } else { $('qEngaged').textContent = 'Q-control off'; $('qWarn').replaceChildren(); }
  if (st.estimate) showEstimate(st.estimate);
  if (st.safe_state_uncertain) notify('error', 'OUTPUT STATE UNCERTAIN: check the instrument front panel');
  document.body.classList.remove('offline'); refreshControls();
}
function showEstimate(e) {
  kv($('resInfo'), [['Method', e.method], ['f₀', fmt(e.f0_hz, 3) + (e.f0_sigma_hz != null ? ' ± ' + fmt(e.f0_sigma_hz, 3) : '') + ' Hz'], ['Bandwidth', fmt(e.bw_hz, 4) + (e.bw_sigma_hz != null ? ' ± ' + fmt(e.bw_sigma_hz, 4) : '') + ' Hz'], ['Q', fmt(e.q, 0)], ['Amplitude', fmtSI(e.amplitude_v, ' V')], ['Phase (setpoint)', fmt(e.phase_deg, 2) + '°'], ['Slope', e.polarity + ' (' + fmt(e.slope_deg_per_hz, 4) + ' °/Hz)'], ['RMS residual', fmtSI(e.rms_residual)]]);
  if (!$('hF0').value) $('hF0').value = e.f0_hz.toFixed(3);
  if (!$('hQ').value) $('hQ').value = e.q.toFixed(0);
  $('hPol').value = e.polarity;
}
function applyProject(p) {
  S.project = p;
  $('projName').textContent = p.name; $('projDir').textContent = p.dir;
  if (document.activeElement !== $('sampleId')) $('sampleId').value = p.sample_id;
  if (document.activeElement !== $('operator')) $('operator').value = p.operator;
  if (document.activeElement !== $('notes')) $('notes').value = p.notes;
  const s = $('saveTxt'); s.textContent = p.dirty ? 'Unsaved changes' : 'All changes saved ' + clockTime(p.modified); s.className = 'badge' + (p.dirty ? ' warn' : ' ok');
  const sel = $('anTrack'); const cur = sel.value; sel.replaceChildren(...(p.tracks || []).map((t) => el('option', { value: t.file }, t.file + (t.complete ? '' : ' (partial)') + ' — ' + t.samples + ' samples')));
  if (cur) sel.value = cur;
  renderRevisions(p.analyses || []); renderHistory(p.history || []);
}
function renderRevisions(list) {
  const t = el('table', {}, el('tr', {}, ...['#', 'time', 'kind', 'result'].map((h) => el('th', {}, h))));
  for (const a of list.slice().reverse()) t.append(el('tr', {}, el('td', {}, a.id), el('td', {}, clockTime(a.created)), el('td', {}, a.kind), el('td', { class: 'mono' }, JSON.stringify(a.result).slice(0, 240))));
  $('revTable').replaceChildren(t);
}
function renderHistory(list) {
  const t = el('table', {}, el('tr', {}, ...['time', 'kind', 'event'].map((h) => el('th', {}, h))));
  for (const h of list.slice().reverse().slice(0, 300)) t.append(el('tr', {}, el('td', { class: 'mono' }, clockTime(h.t)), el('td', {}, h.kind), el('td', {}, h.text)));
  $('history').replaceChildren(t);
}

/* ------------------------------------------------------------------ websocket */
function connectWS() {
  if (!S.token) { notify('error', 'Session token missing: open the URL printed by the service.'); return; }
  const ws = new WebSocket('ws://' + location.host + '/ws?token=' + encodeURIComponent(S.token));
  S.ws = ws;
  ws.onopen = () => { document.body.classList.remove('offline'); notify('ok', 'connected to the service'); };
  ws.onclose = () => { document.body.classList.add('offline'); $('connTxt').textContent = 'Service unreachable: retrying…'; $('connLed').className = 'led err'; setTimeout(connectWS, 2000); };
  ws.onmessage = (m) => { let e; try { e = JSON.parse(m.data); } catch (x) { return; } onEvent(e); };
}
function onEvent(e) {
  switch (e.type) {
    case 'status': applyStatus(e); break;
    case 'log': { const line = e.t + ' [' + e.level + '] ' + e.text; addLog(line, e.level); if (e.level !== 'info') notify(e.level === 'error' ? 'error' : 'warn', e.text); else notify('info', e.text); break; }
    case 'log_tail': for (const l of e.lines) addLog(l, /\[error\]/.test(l) ? 'error' : /\[warn\]/.test(l) ? 'warn' : ''); break;
    case 'progress': $('progress').firstElementChild.style.width = Math.round(100 * e.fraction) + '%'; $('busyTxt').textContent = e.job + ' ' + Math.round(100 * e.fraction) + '% ' + (e.message || ''); break;
    case 'job_done': $('progress').firstElementChild.style.width = '0'; if (e.ok) notify('ok', e.job + ' finished'); break;
    case 'project': applyProject(e.summary); break;
    case 'samples': liveSamples(e.samples); break;
    case 'lagged': addLog('(UI missed ' + e.missed + ' events; recorded data is unaffected)', 'warn'); break;
    default: break;
  }
}
function liveSamples(arr) {
  const L = S.live;
  for (const s of arr) { L.t.push(s.t_s); L.f.push(s.freq_hz); L.ph.push(s.phase_rad * 180 / Math.PI); }
  const cut = L.t.length - 1500; if (cut > 0) for (const k of ['t', 'f', 'ph']) L[k].splice(0, cut);
  const t0 = L.t[0]; const x = L.t.map((t) => t - t0);
  P.liveF.set([{ x, y: L.f }]); P.livePh.set([{ x, y: L.ph, color: '--s2' }]);
}

/* ------------------------------------------------------------------ sweep tab */
function lorentzCurve(e, f0, f1) {
  const x = [], y = []; const g = e.bw_hz / 2;
  for (let i = 0; i <= 400; i++) { const f = f0 + (f1 - f0) * i / 400; x.push(f); y.push(e.offset_v + e.amplitude_v / Math.sqrt(1 + ((f - e.f0_hz) / g) ** 2)); }
  return { x, y };
}
/* Second sweep plot follows the fit selected in the Resonance block: Lorentz => phase, Circle => Nyquist. */
function syncSweepPlots() {
  const circle = (document.querySelector('input[name=fit]:checked') || {}).value === 'Circle';
  $('plTh').parentElement.hidden = circle;
  $('plNy').parentElement.hidden = !circle;
  requestAnimationFrame(() => (circle ? P.Ny : P.Th).draw());
  return circle;
}
function showSweep(r) {
  S.sweep = r;
  syncSweepPlots();
  const sel = (document.querySelector('input[name=fit]:checked') || {}).value;
  const est = (sel === 'Circle' ? r.circle : r.lorentz) || r.lorentz || r.circle;
  const series = [{ x: r.freq_hz, y: r.r, dots: true }];
  const f0 = r.freq_hz[0], f1 = r.freq_hz[r.freq_hz.length - 1];
  if (r.lorentz && sel !== 'Circle') { const c = lorentzCurve(r.lorentz, f0, f1); series.push({ ...c, color: '--s2', width: 1.8 }); }
  if (est) series.push({ vline: est.f0_hz, color: '--cursor' });
  P.R.set(series);
  P.Th.set([{ x: r.freq_hz, y: r.theta_deg, dots: true }].concat(est ? [{ vline: est.f0_hz, color: '--cursor' }] : []));
  const ny = [{ x: r.x, y: r.y, dots: true }];
  if (r.circle && r.circle.circle) { const [cx, cy, R] = r.circle.circle; const x = [], y = []; for (let i = 0; i <= 120; i++) { const a = 6.2832 * i / 120; x.push(cx + R * Math.cos(a)); y.push(cy + R * Math.sin(a)); } ny.push({ x, y, color: '--s2' }); }
  P.Ny.set(ny);
  const fe = $('fitErrors'); fe.replaceChildren(...(r.fit_errors || []).map((m) => el('div', { class: 'warnbox' }, m)));
  if (est) { showEstimate(est); $('ovF0').value = est.f0_hz.toFixed(3); $('ovBw').value = est.bw_hz.toFixed(4); $('ovPh').value = est.phase_deg.toFixed(2); $('ovPol').value = est.polarity; if (est.polarity === 'Inverted') fe.append(el('div', { class: 'warnbox' }, 'Inverted slope: the PLL gains will be positive.')); }
  refreshControls();
}
function initSweep() {
  ['swStart', 'swStop', 'swPoints', 'swDrive', 'swBw', 'swDiv'].forEach((id) => $(id).addEventListener('input', refreshControls));
  $('swRun').addEventListener('click', (ev) => act(ev.target, async () => {
    const body = { start_hz: num('swStart'), stop_hz: num('swStop'), points: num('swPoints'), drive_v: num('swDrive'), compensate_divider: $('swDiv').checked, bandwidth_hz: num('swBw') };
    notify('info', 'sweeping…'); const r = await api('/api/sweep/run', body); showSweep(r); notify('ok', 'sweep done');
  }));
  $('swUseView').addEventListener('click', () => { const B = P.R.B; if (B) { $('swStart').value = Math.round(B.x0); $('swStop').value = Math.round(B.x1); refreshControls(); } });
  document.querySelectorAll('input[name=fit]').forEach((r) => r.addEventListener('change', () => { if (S.sweep) showSweep(S.sweep); else syncSweepPlots(); }));
  syncSweepPlots();
  $('fitApply').addEventListener('click', (ev) => act(ev.target, async () => {
    const m = document.querySelector('input[name=fit]:checked').value; const e = await api('/api/sweep/select', { method: m }); showEstimate(e); notify('ok', m + ' fit selected: f₀ ' + fmt(e.f0_hz, 2) + ' Hz');
  }));
  $('ovApply').addEventListener('click', (ev) => act(ev.target, async () => {
    const e = await api('/api/sweep/override', { f0_hz: num('ovF0'), bw_hz: num('ovBw'), phase_deg: num('ovPh'), polarity: $('ovPol').value }); showEstimate(e); notify('ok', 'override applied');
  }));
}

/* ------------------------------------------------------------------ PLL tab */
function pllParams() {
  const p = { target_bw_hz: num('pllBw'), bw_factor: num('pllFac'), demod_tc_s: num('pllTc') * 1e-3, demod_order: 4, limit_hz: num('pllLim'), use_device_advisor: $('pllDev').checked, manual_gains: null };
  if ($('pllMan').checked) p.manual_gains = [num('gP'), num('gI'), num('gD')];
  return p;
}
function initPll() {
  $('pllMan').addEventListener('change', () => ['gP', 'gI', 'gD'].forEach((id) => { $(id).disabled = !$('pllMan').checked; }));
  $('pllBw').addEventListener('input', refreshControls);
  $('pllAdv').addEventListener('click', (ev) => act(ev.target, async () => {
    const a = await api('/api/pll/advise', pllParams()); const pol = S.status && S.status.estimate ? S.status.estimate.polarity : 'Normal'; const sg = pol === 'Normal' ? -1 : 1;
    $('pllAdvice').textContent = 'Advisor (' + (a.from_device ? 'LabOne' : 'local model') + '): |P| = ' + fmtSI(Math.abs(a.p)) + ', |I| = ' + fmtSI(Math.abs(a.i)) + ', D = ' + fmtSI(a.d) + ', BW ' + fmt(a.bandwidth_hz, 1) + ' Hz. Slope ' + pol + ' ⇒ applied sign ' + (sg < 0 ? 'negative' : 'positive') + ' (P = ' + fmtSI(sg * Math.abs(a.p)) + ', I = ' + fmtSI(sg * Math.abs(a.i)) + ').';
    if (!$('gP').value) { $('gP').value = (sg * Math.abs(a.p)).toPrecision(5); $('gI').value = (sg * Math.abs(a.i)).toPrecision(5); }
  }));
  $('pllStart').addEventListener('click', (ev) => act(ev.target, async () => { notify('info', 'locking PLL…'); S.live = { t: [], f: [], ph: [] }; const st = await api('/api/pll/start', pllParams()); notify('ok', 'PLL locked at ' + fmt(st.freq_hz, 2) + ' Hz'); }));
  $('pllStop').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/pll/stop', {}); notify('info', 'PLL off'); }));
  $('trStart').addEventListener('click', (ev) => act(ev.target, async () => { S.live = { t: [], f: [], ph: [] }; const r = await api('/api/track/start', { rate_hz: num('trRate') }); notify('ok', 'recording ' + r.file); }));
  $('trStop').addEventListener('click', (ev) => act(ev.target, async () => { const r = await api('/api/track/stop', {}); notify('ok', r.samples + ' samples saved in ' + r.file); }));
}

/* ------------------------------------------------------------------ Q-control tab */
function showQcal(c) {
  S.qcal = c;
  const pts = c.points.filter((p) => p.gamma != null);
  const gx = pts.map((p) => p.gain), gy = pts.map((p) => p.gamma);
  const lo = Math.min(...gx, c.gain_lasing != null ? c.gain_lasing : Infinity), hi = Math.max(...gx);
  const fit = { x: [lo, hi], y: [c.gamma_native + c.slope * lo, c.gamma_native + c.slope * hi], color: '--s2' };
  P.qG.set([{ x: gx, y: gy, dots: true }, fit].concat(c.gain_lasing != null ? [{ vline: c.gain_lasing, color: '--cursor' }] : []));
  const cols = ['--s1', '--s2', '--s3', '--s4'];
  P.qD.set(c.points.filter((p) => p.freq_hz && p.freq_hz.length).map((p, i) => ({ x: p.freq_hz, y: p.r, color: cols[i % 4], width: 1.3 })));
  kv($('qInfo'), [['Native Q (calibration)', fmt(c.q_native, 0)], ['Q (sweep fit)', fmt(c.q_native_sweep, 0)], ['Γ native', fmt(c.gamma_native, 2) + ' 1/s'], ['dΓ/dgain', fmt(c.slope, 4) + ' 1/s (R² ' + fmt(c.r2, 4) + ')'], ['Lasing at gain', c.gain_lasing != null ? fmt(c.gain_lasing, 3) : 'n/a']]);
  const t = el('table', {}, el('tr', {}, ...['gain', 'f₀ (Hz)', 'BW (Hz)', 'Q', 'Q (phase slope)', 'Γ (1/s)', 'peak (V)', 'note'].map((h) => el('th', {}, h))));
  for (const p of c.points) t.append(el('tr', {}, el('td', { class: 'num' }, p.gain), el('td', { class: 'num' }, p.f0_hz != null ? fmt(p.f0_hz, 2) : '—'), el('td', { class: 'num' }, p.bw_hz != null ? fmt(p.bw_hz, 3) : '—'), el('td', { class: 'num' }, p.q != null ? fmt(p.q, 0) : '—'), el('td', { class: 'num' }, p.q_phase != null ? fmt(p.q_phase, 0) : '—'), el('td', { class: 'num' }, p.gamma != null ? fmt(p.gamma, 2) : '—'), el('td', { class: 'num' }, p.peak_v != null ? fmtSI(p.peak_v) : '—'), el('td', {}, p.note)));
  $('qTable').replaceChildren(alignHead(t)); refreshControls();
}
function initQ() {
  ['qGains', 'qSpan', 'qPts'].forEach((id) => $(id).addEventListener('input', refreshControls));
  $('qCal').addEventListener('click', (ev) => act(ev.target, async () => {
    const body = { gains: parseGains(), span_bw_factor: num('qSpan'), points: num('qPts'), lasing_margin: num('qMargin') };
    notify('info', 'calibrating (one sweep of the effective resonance per gain)…'); showQcal(await api('/api/q/calibrate', body)); notify('ok', 'calibration done');
  }));
  $('qEngage').addEventListener('click', (ev) => act(ev.target, async () => {
    const e = await api('/api/q/engage', { q_target: num('qTarget'), enable_agc: $('qAgc').checked, agc_gains: [num('agcP'), num('agcI')], lasing_margin: num('qMargin') }); notify(e.warnings.length ? 'warn' : 'ok', 'Q-control engaged: gain ' + fmt(e.gain, 3) + (e.warnings.length ? ' (see warnings)' : ''));
  }));
  $('qOff').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/q/disengage', {}); notify('info', 'Q-control off'); }));
}

/* ------------------------------------------------------------------ HiPIMS tab */
function stepPhaseDeg(step, ref) {
  const out = []; for (let i = 0; i < step.x.length; i++) { let d = Math.atan2(step.y[i], step.x[i]) - ref; d = Math.atan2(Math.sin(d), Math.cos(d)); out.push(d * 180 / Math.PI); } return out;
}
function renderSteps() {
  const cols = ['--s1', '--s2', '--s3', '--s4'];
  if (S.steps.length) {
    const s0 = S.steps[0]; let sx = 0, sy = 0; for (let i = 0; i < s0.x.length; i++) { const a = Math.atan2(s0.y[i], s0.x[i]); sx += Math.sin(a); sy += Math.cos(a); } const ref = Math.atan2(sx, sy);
    P.hTr.set(S.steps.map((s, i) => ({ x: s.t_us, y: stepPhaseDeg(s, ref), color: cols[i % 4], width: 1.2 })).concat([{ vline: 0, color: '--cursor' }, { vline: num('hPulse'), color: '--cursor' }]));
  } else P.hTr.set([]);
  const t = el('table', {}, el('tr', {}, ...['#', 'window start (µs)', 'length (µs)', 'pulses', 'time'].map((h) => el('th', {}, h))));
  for (const s of S.steps) t.append(el('tr', {}, el('td', {}, s.index), el('td', { class: 'num' }, s.window_start_us), el('td', { class: 'num' }, s.window_us), el('td', { class: 'num' }, s.pulses), el('td', {}, clockTime(s.acquired_at))));
  $('hTable').replaceChildren(alignHead(t));
}
function initHip() {
  ['hWin', 'hPoints', 'hTc', 'hPulses', 'hStart'].forEach((id) => $(id).addEventListener('input', refreshControls));
  $('hStep').addEventListener('click', (ev) => act(ev.target, async () => {
    const body = { index: 0, window_start_us: num('hStart'), window_us: num('hWin'), pulses: num('hPulses'), points: num('hPoints'), demod_tc_us: num('hTc'), demod_rate_hz: num('hRate') * 1000 };
    notify('info', 'acquiring step…'); const r = await api('/api/hipims/step', body); S.steps.push(r); renderSteps(); notify('ok', 'step ' + r.index + ' acquired');
  }));
  $('hNew').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/hipims/clear', {}); S.steps = []; S.profile = null; renderSteps(); P.hProf.set([]); $('hWarn').replaceChildren(); notify('info', 'new series (earlier steps stay in the project)'); }));
  $('hProfile').addEventListener('click', (ev) => act(ev.target, async () => {
    const body = { pulse_width_us: num('hPulse'), f0_hz: num('hF0'), q: num('hQ'), polarity: $('hPol').value, end_fraction: num('hEnd'), f_unloaded_hz: num('hFq'), rho_g_cm3: num('hRho') };
    const r = (await api('/api/hipims/profile', body)).profile; S.profile = r;
    P.hProf.set([{ x: r.points.map((p) => p.overlap_us), y: r.points.map((p) => p.delta_f_hz), dots: true, line: true }]);
    const t = el('table', {}, el('tr', {}, ...['step', 'overlap (µs)', 'Δθ (°)', 'Δf (Hz)', 'thickness (nm)', 'R/R₀'].map((h) => el('th', {}, h))));
    for (const p of r.points) t.append(el('tr', {}, el('td', {}, p.index), el('td', { class: 'num' }, fmt(p.overlap_us, 1)), el('td', { class: 'num' }, fmt(p.delta_theta_deg, 3)), el('td', { class: 'num' }, fmt(p.delta_f_hz, 3)), el('td', { class: 'num' }, fmt(p.thickness_nm, 4)), el('td', { class: 'num' }, fmt(p.amplitude_ratio, 4))));
    $('hTable').replaceChildren(alignHead(t)); $('hWarn').replaceChildren(...r.warnings.map((w) => el('div', { class: 'warnbox' }, w)));
    notify('ok', 'profile computed (revision saved)');
  }));
}

/* ------------------------------------------------------------------ analysis tab */
function crystal() { return { f_unloaded_hz: num('cFq'), diameter_mm: num('cDia'), rho_g_cm3: num('cRho'), z_ratio: num('cZ'), model: $('cModel').value }; }
function initAna() {
  $('cMat').addEventListener('change', () => { const m = S.materials.find((x) => x.name === $('cMat').value); if (m) { $('cRho').value = m.rho_g_cm3; $('cZ').value = m.z_ratio; $('hRho').value = m.rho_g_cm3; } });
  const edited = () => { const m = S.materials.find((x) => x.name === $('cMat').value); if (m && (num('cRho') !== m.rho_g_cm3 || num('cZ') !== m.z_ratio)) $('cMat').value = 'custom'; };
  $('cRho').addEventListener('input', edited); $('cZ').addEventListener('input', edited);
  $('anLoad').addEventListener('click', (ev) => act(ev.target, async () => {
    const f = $('anTrack').value; if (!f) throw new Error('no recorded track in this project'); const s = await api('/api/tracks/' + encodeURIComponent(f) + '/series?max=4000'); S.series = { file: f, ...s };
    P.an.view = null; P.an.cur = null; P.an.set([{ x: s.t_s, y: s.f_hz, width: 1.2 }]);
    const n = s.t_s.length; $('anInfo').textContent = n + ' points shown, ' + fmt(s.t_s[n - 1] - s.t_s[0], 1) + ' s';
    $('wA').value = fmt(s.t_s[0], 3); $('wB').value = fmt(s.t_s[n - 1], 3); if (!$('cFq').dataset.set) $('cFq').value = Math.round(s.f_hz[0]);
  }));
  $('anCalc').addEventListener('click', (ev) => act(ev.target, async () => {
    if (!S.series) throw new Error('load a track first');
    const r = (await api('/api/analysis/window', { track: S.series.file, ta_s: num('wA'), tb_s: num('wB'), crystal: crystal() })).result; S.lastWin = r;
    $('outRate').textContent = r.rate_nm_s != null ? r.rate_nm_s.toPrecision(5) : '—';
    kv($('outInfo'), [['Thickness', fmt(r.thickness_nm, 3) + ' nm'], ['Δf', fmt(r.df_hz, 3) + ' Hz'], ['Window', fmt(r.ta_s, 2) + ' → ' + fmt(r.tb_s, 2) + ' s'], ['Model', r.model + ' · loading ' + fmt(r.loading * 100, 3) + ' %'], ['Mass', fmtSI(r.mass_g, ' g')]]);
    $('outWarn').replaceChildren(...(r.warning ? [el('div', { class: 'warnbox' }, r.warning)] : []));
  }));
  $('iffUse').addEventListener('click', () => { if (S.lastWin && S.lastWin.rate_nm_s != null) $('iffRef').value = S.lastWin.rate_nm_s; });
  $('iffCalc').addEventListener('click', (ev) => act(ev.target, async () => {
    if (!S.lastWin || S.lastWin.rate_nm_s == null) throw new Error('calculate a window first (its rate is the neutral rate)');
    const r = (await api('/api/analysis/iff', { reference_nm_s: num('iffRef'), neutral_nm_s: S.lastWin.rate_nm_s })).result;
    $('iffOut').textContent = fmt(r.percent, 1) + ' %'; $('iffNote').textContent = r.note || '';
  }));
}

/* ------------------------------------------------------------------ experiment / log tabs */
async function refreshProjects() { const l = await api('/api/projects'); $('projList').replaceChildren(...l.map((n) => el('option', { value: n }, n))); }
/* The device address is editable; the last one used is remembered in this browser. */
function deviceProblem() {
  const id = $('devId').value.trim(), host = $('devHost').value.trim(), port = num('devPort');
  if (!/^[A-Za-z0-9_-]{1,32}$/.test(id)) return 'device ID: letters/digits only, e.g. dev2091';
  if (!host || /[\s/]/.test(host)) return 'host: a name or IP address, e.g. localhost';
  if (!Number.isInteger(port) || port < 1 || port > 65535) return 'port: 1-65535 (LabOne Data Server: 8004)';
  return '';
}
function deviceBody() { return { id: $('devId').value.trim(), host: $('devHost').value.trim(), port: num('devPort') }; }
function initDeviceFields() {
  let d = (S.config && S.config.device) || { id: '', host: 'localhost', port: 8004 };
  try { const saved = JSON.parse(localStorage.getItem('hq-device') || 'null'); if (saved && saved.id) d = saved; } catch (e) { /* ignore */ }
  $('devId').value = d.id; $('devHost').value = d.host; $('devPort').value = d.port;
  ['devId', 'devHost', 'devPort'].forEach((id) => $(id).addEventListener('input', refreshControls));
}
function initExp() {
  const conn = (sim) => (ev) => act(ev.target, async () => {
    notify('info', sim ? 'starting simulator…' : 'connecting to ' + $('devId').value.trim() + ' (LabOne Data Server)…');
    const dev = deviceBody(); await api('/api/connect', { simulate: sim, device: dev });
    try { localStorage.setItem('hq-device', JSON.stringify(dev)); } catch (e) { /* ignore */ }
    notify('ok', 'connected');
  });
  $('connectBtn').addEventListener('click', conn(false)); $('connectSimBtn').addEventListener('click', conn(true));
  $('disconnectBtn').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/disconnect', {}); S.sweep = null; notify('info', 'disconnected, outputs off'); }));
  $('cfgLoad').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/settings/load', { path: $('cfgPath').value }); notify('ok', 'settings loaded'); }));
  $('cfgSave').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/settings/save', { path: $('cfgPath').value }); notify('ok', 'settings saved'); }));
  $('metaSave').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/project/meta', { sample_id: $('sampleId').value, operator: $('operator').value, notes: $('notes').value }); notify('ok', 'project information applied'); }));
  $('projSave').addEventListener('click', (ev) => act(ev.target, async () => { await saveProject(); }));
  $('projNew').addEventListener('click', (ev) => act(ev.target, async () => { await api('/api/project/new', { name: $('newName').value }); resetProjectViews(); await refreshProjects(); notify('ok', 'project created'); }));
  $('projLoad').addEventListener('click', (ev) => act(ev.target, async () => { const p = await api('/api/project/load', { name: $('projList').value }); loadProjectData(p); notify('ok', 'project loaded'); }));
  $('projRefresh').addEventListener('click', () => act(null, refreshProjects));
  $('reportBtn').addEventListener('click', () => window.open('/api/report?token=' + encodeURIComponent(S.token), '_blank', 'noopener'));
}
async function saveProject() { await api('/api/project/save', {}); notify('ok', 'project saved'); }
function resetProjectViews() { S.sweep = null; S.qcal = null; S.steps = []; S.series = null; [P.R, P.Th, P.Ny, P.qG, P.qD, P.hTr, P.hProf, P.an].forEach((p) => p.set([])); renderSteps(); }
function loadProjectData(p) {
  resetProjectViews();
  if (p.sweeps && p.sweeps.length) showSweep(p.sweeps[p.sweeps.length - 1]);
  if (p.qcal && p.qcal.length) showQcal(p.qcal[p.qcal.length - 1]);
  S.steps = (p.steps || []).slice(p.series_start || 0); renderSteps();
  if (p.hipims) { $('hPulse').value = p.hipims.pulse_width_us; }
}
function initLog() {
  $('logCopy').addEventListener('click', async () => { const text = S.logLines.map((l) => l.line).join('\n'); try { await navigator.clipboard.writeText(text); notify('ok', 'log copied'); } catch (e) { notify('warn', 'clipboard not available: use Export'); } });
  $('logExport').addEventListener('click', (ev) => act(ev.target, async () => { const t = await api('/api/log'); const a = el('a', { href: URL.createObjectURL(new Blob([t], { type: 'text/plain' })), download: 'session.log' }); document.body.append(a); a.click(); a.remove(); }));
  $('lvl').addEventListener('change', () => act(null, async () => { await api('/api/loglevel', { level: $('lvl').value }); }));
}

/* ------------------------------------------------------------------ global buttons / keys */
function initGlobal() {
  $('armBtn').addEventListener('click', () => setArmed(!armed()));
  $('cancelBtn').addEventListener('click', () => act(null, async () => { await api('/api/cancel', {}); notify('warn', 'cancelling…'); }));
  $('estopBtn').addEventListener('click', () => { setArmed(false); act(null, async () => { await api('/api/estop', {}); notify('error', 'EMERGENCY STOP sent'); }); });
  $('helpBtn').addEventListener('click', () => $('helpDlg').showModal());
  $('exitBtn').addEventListener('click', async () => {
    const ok = await confirmDialog('Exit', 'Switch all outputs off, save the project and stop the service?', 'Exit'); if (!ok) return;
    setArmed(false);
    try { await api('/api/shutdown', {}); document.body.innerHTML = '<main style="padding:40px;font:16px system-ui"><h2>Service stopped</h2><p>Outputs were switched off and the project saved. You can close this tab.</p></main>'; } catch (e) { notify('error', 'shutdown failed: ' + e.message); }
  });
  window.addEventListener('keydown', (e) => {
    if (e.altKey && /^[1-7]$/.test(e.key)) { showTab(TABS[Number(e.key) - 1]); e.preventDefault(); }
    else if (e.altKey && (e.key === 'a' || e.key === 'A')) { setArmed(!armed()); e.preventDefault(); }
    else if (e.key === 'Escape' && S.status && S.status.busy && !$('dlg').open) { $('cancelBtn').click(); }
    else if (e.ctrlKey && e.shiftKey && (e.key === 'S' || e.key === 's')) { $('estopBtn').click(); e.preventDefault(); }
    else if (e.ctrlKey && !e.shiftKey && (e.key === 's' || e.key === 'S')) { act(null, saveProject); e.preventDefault(); }
  });
  window.addEventListener('beforeunload', () => { /* outputs stay under service control; nothing to do */ });
}

/* ------------------------------------------------------------------ boot */
async function boot() {
  initTips(); initTabs(); initTheme(); initPlots(); initGlobal(); initExp(); initSweep(); initPll(); initQ(); initHip(); initAna(); initLog();
  if (!S.token) { notify('error', 'Session token missing: open the URL printed by the service.'); return; }
  try {
    S.config = await api('/api/config'); initDeviceFields(); const l = S.config.limits;
    kv($('limitInfo'), [['Frequency', fmtSI(l.freq_min_hz) + ' – ' + fmtSI(l.freq_max_hz) + ' Hz'], ['Drive max', l.drive_max_v + ' V'], ['Q-feedback max', l.q_amp_max_v + ' V'], ['|Q-control gain| max', String(l.q_gain_abs_max)], ['PLL excursion max', l.pll_limit_hz_max + ' Hz'], ['Sweep points max', String(l.sweep_points_max)]]);
    S.materials = await api('/api/materials'); $('cMat').replaceChildren(...S.materials.map((m) => el('option', { value: m.name }, m.name)), el('option', { value: 'custom' }, 'custom')); $('cMat').value = 'Ti';
    const s = await api('/api/status'); applyStatus(s.status); applyProject(s.project);
    loadProjectData(await api('/api/project')); await refreshProjects();
  } catch (e) { notify('error', e.message); }
  connectWS(); refreshControls();
}
window.addEventListener('DOMContentLoaded', boot);
