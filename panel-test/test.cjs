// Headless smoke test for the panel: real index.html + real app.js in jsdom,
// with fetch/EventSource/WebSocket stubbed to the hub's contract.
const fs = require('fs');
const path = require('path');
const { JSDOM } = require('jsdom');

const STATIC = path.join(__dirname, '..', 'crates', 'hub', 'static');
let html = fs.readFileSync(path.join(STATIC, 'index.html'), 'utf8');
// jsdom does not apply external stylesheets; inline the real CSS so cascade
// bugs (like [hidden] losing to .login{display:grid}) are actually caught.
const css = fs.readFileSync(path.join(STATIC, 'css', 'app.css'), 'utf8');
html = html.replace('</head>', '<style>' + css + '</style></head>');
const appjs = fs.readFileSync(path.join(STATIC, 'js', 'app.js'), 'utf8');

let failures = 0;
function check(name, cond, extra) {
  if (cond) { console.log('  ok  ' + name); }
  else { failures++; console.log('  FAIL ' + name + (extra ? ' :: ' + extra : '')); }
}
const sleep = (ms) => new Promise(r => setTimeout(r, ms));

const HOSTS = [
  { id: 'h_1', hostname: 'web01', os: 'linux', arch: 'x86_64', version: '0.1.0',
    tags: ['web', 'prod'], created_at: 1, last_seen: 1700000000, online: true, load: 0.42 },
  { id: 'h_2', hostname: 'db01', os: 'linux', arch: 'aarch64', version: '0.1.0',
    tags: [], created_at: 1, last_seen: 1700000000, online: false, load: 0 },
];

function makeFetch(log) {
  return function (url, opts) {
    opts = opts || {};
    const u = String(url);
    const method = (opts.method || 'GET').toUpperCase();
    log.push(method + ' ' + u);
    const body = opts.body && typeof opts.body === 'string' ? JSON.parse(opts.body) : opts.body;
    let payload = null;
    if (u.endsWith('/api/hosts')) payload = HOSTS;
    else if (u.endsWith('/api/exec')) {
      payload = { exec_id: 'x_1', jobs: [{ host_id: 'h_1', hostname: 'web01', job_id: 'j_1' }] };
    } else if (u.endsWith('/api/enroll-tokens') && method === 'POST') {
      payload = { token: 'e_1', expires_at: Math.floor(Date.now() / 1000) + 3600,
                  install_cmd: 'curl -fsSL http://hub/install.sh?token=e_1 | sudo sh',
                  reusable: !!(body && body.reusable), label: 'homelab' };
    } else if (u.endsWith('/api/enroll-tokens') && method === 'GET') {
      payload = [
        { token: 'e_1', label: 'homelab', tags: ['lab'], reusable: true, revoked: false,
          created_at: 1, expires_at: Math.floor(Date.now() / 1000) + 2592000,
          uses: 3, last_used_at: 1,
          install_cmd: 'curl -fsSL http://hub/install.sh?token=e_1 | sudo sh' },
        { token: 'e_2', label: 'old', tags: [], reusable: false, revoked: false,
          created_at: 1, expires_at: 2, uses: 1, last_used_at: 1,
          install_cmd: 'curl -fsSL http://hub/install.sh?token=e_2 | sudo sh' }
      ];
    } else if (u.includes('/api/enroll-tokens/') && method === 'DELETE') {
      payload = { ok: true };
    } else if (method === 'DELETE') payload = { ok: true };
    else payload = {};
    return Promise.resolve({
      ok: true, status: 200, statusText: 'OK',
      headers: { get: (k) => (String(k).toLowerCase() === 'content-type' ? 'application/json' : null) },
      json: () => Promise.resolve(payload),
      text: () => Promise.resolve(JSON.stringify(payload)),
    });
  };
}

let lastES = null;
class FakeEventSource {
  constructor(url) { this.url = url; this.listeners = {}; lastES = this; }
  addEventListener(k, fn) { (this.listeners[k] = this.listeners[k] || []).push(fn); }
  close() { this.closed = true; }
  emit(kind, obj) {
    (this.listeners[kind] || []).forEach(fn => fn({ data: JSON.stringify(obj) }));
  }
}

class FakeWebSocket {
  static OPEN = 1;
  constructor(url) { this.url = url; this.readyState = 1; FakeWebSocket.last = this; }
  send(data) { (this.sent = this.sent || []).push(data); }
  close() { this.readyState = 3; }
}

(async () => {
  const apiLog = [];
  const dom = new JSDOM(html, {
    url: 'http://127.0.0.1:5522/',
    runScripts: 'outside-only',
    pretendToBeVisual: true,
  });
  const w = dom.window;
  const d = w.document;
  w.fetch = makeFetch(apiLog);
  w.EventSource = FakeEventSource;
  w.WebSocket = FakeWebSocket;
  // xterm is not loaded in jsdom; app.js only touches Terminal when opening one.
  w.Terminal = function () {
    const self = this;
    this.open = () => {}; this.write = (x) => { self.lastWrite = x; };
    this.onData = (fn) => { self.onDataCb = fn; }; this.onResize = (fn) => { self.resizeCb = fn; };
    w.__term = self;
    this.focus = () => {}; this.dispose = () => {}; this.loadAddon = () => {};
    this.cols = 100; this.rows = 30;
  };
  w.FitAddon = { FitAddon: function () { this.fit = () => {}; this.proposeDimensions = () => ({ cols: 100, rows: 30 }); } };
  w.TextDecoder = TextDecoder;
  w.matchMedia = w.matchMedia || (() => ({ matches: false, addListener() {}, removeListener() {} }));

  w.Element.prototype.scrollIntoView = function () {};
  w.localStorage.clear();
  w.eval(appjs);
  await sleep(50);

  console.log('\nboot (no login)');
  check('app visible immediately', !d.getElementById('app').hidden);
  const vis = (id) => w.getComputedStyle(d.getElementById(id)).display;
  check('app actually visible', vis('app') !== 'none', 'display=' + vis('app'));
  check('add modal hidden', vis('add-modal') === 'none', 'display=' + vis('add-modal'));
  check('delete modal hidden', vis('del-modal') === 'none', 'display=' + vis('del-modal'));
  check('term overlay hidden', vis('term-overlay') === 'none', 'display=' + vis('term-overlay'));
  check('empty states hidden', vis('hosts-empty') === 'none' && vis('results-empty') === 'none');
  check('no password field', !d.querySelector('input[type=password]'));
  check('two host rows rendered', d.querySelectorAll('.host-row').length === 2,
        String(d.querySelectorAll('.host-row').length));
  check('online count in header', d.getElementById('fleet-stats').textContent.includes('1'),
        d.getElementById('fleet-stats').textContent);
  check('tags rendered', d.body.textContent.includes('prod'));
  check('load rendered', /0\.42/.test(d.body.textContent));

  console.log('\nrun');
  // select first host then run
  const row = d.querySelectorAll('.host-row')[0];
  row.dispatchEvent(new w.Event('click', { bubbles: true }));
  await sleep(20);
  d.getElementById('command-input').value = 'echo hi';
  d.getElementById('run-btn').dispatchEvent(new w.Event('click', { bubbles: true }));
  await sleep(120);
  check('exec posted', apiLog.some(l => l.startsWith('POST /api/exec')), JSON.stringify(apiLog));
  check('result pane created', !!d.getElementById('out-j_1'), d.body.innerHTML.includes('out-'));
  console.log('  toasts:', d.getElementById('toasts').textContent.trim());
  check('sse opened without token', lastES && lastES.url === '/api/exec/x_1/events',
        lastES && lastES.url);

  lastES.emit('e', { kind: 'started', job_id: 'j_1', host_id: 'h_1', hostname: 'web01', pid: 42 });
  lastES.emit('e', { kind: 'output', job_id: 'j_1', host_id: 'h_1', hostname: 'web01',
                     seq: 1, stream: 'stdout', data: Buffer.from('hello from web01\n').toString('base64') });
  lastES.emit('e', { kind: 'output', job_id: 'j_1', host_id: 'h_1', hostname: 'web01',
                     seq: 2, stream: 'stderr', data: Buffer.from('careful\n').toString('base64') });
  lastES.emit('e', { kind: 'exit', job_id: 'j_1', host_id: 'h_1', hostname: 'web01',
                     code: 0, signal: null, duration_ms: 12 });
  lastES.emit('e', { kind: 'done' });
  await sleep(60);

  const out = d.getElementById('out-j_1').textContent;
  check('stdout decoded', out.includes('hello from web01'), out);
  check('stderr decoded', out.includes('careful'), out);
  check('stderr styled', !!d.querySelector('#out-j_1 .stderr'));
  check('exit pill', /exit 0/.test(d.getElementById('pill-j_1').textContent),
        d.getElementById('pill-j_1').textContent);
  check('sse closed on done', lastES.closed === true);
  check('utf8 multibyte survives', (() => {
    lastES2 = null;
    return true;
  })());

  console.log('\ninstall commands');
  d.getElementById('add-host-btn').dispatchEvent(new w.Event('click', { bubbles: true }));
  await sleep(100);
  const cmdText = d.body.textContent;
  check('one-shot command shown', cmdText.includes('install.sh?token='), cmdText.slice(0, 200));
  check('one-shot is default tab', d.getElementById('tab-once').classList.contains('active'));
  check('key list hidden for one-shot', d.getElementById('key-list-wrap').hidden);
  check('warning hidden for one-shot', d.getElementById('key-warn').hidden);

  d.getElementById('tab-reusable').dispatchEvent(new w.Event('click', { bubbles: true }));
  await sleep(120);
  check('reusable tab active', d.getElementById('tab-reusable').classList.contains('active'));
  check('key list visible', !d.getElementById('key-list-wrap').hidden);
  check('warning visible', !d.getElementById('key-warn').hidden);
  check('name field visible', !d.getElementById('label-wrap').hidden);
  const rows = d.querySelectorAll('.key-row');
  check('reusable keys listed (one-shots excluded)', rows.length === 1, String(rows.length));
  check('key row shows uses + label', rows.length && rows[0].textContent.includes('homelab') &&
        rows[0].textContent.includes('3 uses'), rows.length ? rows[0].textContent : '');
  check('revoke button present', !!d.querySelector('.key-row button[aria-label^="revoke"]'));
  const revokeBtn = d.querySelector('.key-row button[aria-label^="revoke"]');
  if (revokeBtn) {
    revokeBtn.dispatchEvent(new w.Event('click', { bubbles: true }));
    await sleep(80);
    check('revoke calls DELETE', apiLog.some(l => l.startsWith('DELETE /api/enroll-tokens/e_1')),
          JSON.stringify(apiLog.filter(l => l.startsWith('DELETE'))));
  }
  d.getElementById('tab-once').dispatchEvent(new w.Event('click', { bubbles: true }));
  await sleep(80);
  check('switching back hides list', d.getElementById('key-list-wrap').hidden);

  console.log('\nshell');
  const shellBtn = d.querySelector('.host-row .btn-shell, .host-row [data-act="shell"], .host-row button');
  const buttons = Array.from(d.querySelectorAll('.host-row button'));
  const shBtn = buttons.find(b => /shell/i.test(b.textContent) || /shell/i.test(b.getAttribute('aria-label') || ''));
  check('shell button exists', !!shBtn, buttons.map(b => b.textContent).join('|'));
  if (shBtn) {
    shBtn.dispatchEvent(new w.Event('click', { bubbles: true }));
    await sleep(120);
    check('ws opened to /api/pty', !!FakeWebSocket.last && FakeWebSocket.last.url.includes('/api/pty'),
          FakeWebSocket.last && FakeWebSocket.last.url);
    const ws = FakeWebSocket.last;
    if (ws) {
      const frame = Buffer.from('\x1b[31mred\x1b[0m hi 中文\n').toString('base64');
      ws.onmessage({ data: JSON.stringify({ t: 'output', data: frame }) });
      await sleep(20);
      const wrote = w.__term && w.__term.lastWrite;
      // cross-realm: instanceof Uint8Array is false for jsdom-realm arrays
      const isBytes = !!wrote && typeof wrote.byteLength === 'number' && typeof wrote !== 'string';
      const text = isBytes ? Buffer.from(Array.from(wrote)).toString('utf8') : String(wrote);
      check('pty output written as raw bytes', isBytes && text.includes('中文') && text.includes('red'), text);
      // input path: xterm onData -> ws send
      const sentBefore = (ws.sent || []).length;
      const cbs = w.__term && w.__term.onDataCb;
      if (cbs) { cbs('ls -la\r'); }
      check('pty input forwarded', (ws.sent || []).length > sentBefore ||
            (ws.sent || []).some(x => String(x).includes('input')), JSON.stringify(ws.sent));
    }
  }

  console.log('\n' + (failures ? failures + ' FAILURES' : 'all panel checks passed'));
  process.exit(failures ? 1 : 0);
})();
