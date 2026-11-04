/* hypha fleet panel — vanilla JS, no build step. */
(function () {
  'use strict';

  var TOKEN_KEY = 'hypha.token';
  var token = localStorage.getItem(TOKEN_KEY) || '';

  var $ = function (id) { return document.getElementById(id); };

  // ── base64 → bytes → utf-8 string ────────────────────────────────────
  function decodeB64(b64) {
    try {
      var bin = atob(b64);
      var bytes = new Uint8Array(bin.length);
      for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
      return new TextDecoder('utf-8', { fatal: false }).decode(bytes);
    } catch (e) {
      return '<<binary output: ' + (e && e.message ? e.message : 'decode error') + '>>';
    }
  }

  function b64Bytes(b64) {
    var bin = atob(b64);
    var bytes = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return bytes;
  }

  // ── api ──────────────────────────────────────────────────────────────
  function api(path, opts) {
    opts = opts || {};
    var headers = { 'Accept': 'application/json' };
    if (token) headers['Authorization'] = 'Bearer ' + token;
    if (opts.body !== undefined && opts.raw === undefined) {
      headers['Content-Type'] = 'application/json';
    }
    return fetch(path, {
      method: opts.method || 'GET',
      headers: headers,
      body: opts.body === undefined
        ? undefined
        : (opts.raw ? opts.body : JSON.stringify(opts.body))
    }).then(function (res) {
      var ct = res.headers.get('content-type') || '';
      if (res.status === 401) {
        logout(true);
        throw new Error('unauthorized');
      }
      if (!res.ok) {
        return res.text().then(function (t) {
          var msg = t;
          try { msg = JSON.parse(t).error || t; } catch (e) { /* keep raw */ }
          throw new Error(msg || res.status + ' ' + res.statusText);
        });
      }
      return ct.indexOf('json') >= 0 ? res.json() : res.text();
    });
  }

  // ── toasts ───────────────────────────────────────────────────────────
  function toast(msg, kind) {
    var el = document.createElement('div');
    el.className = 'toast' + (kind ? ' ' + kind : '');
    el.setAttribute('role', 'status');
    el.textContent = msg;
    $('toasts').appendChild(el);
    setTimeout(function () { el.remove(); }, kind === 'ok' ? 2200 : 6000);
  }

  // ── state ────────────────────────────────────────────────────────────
  var state = {
    hosts: [],
    selected: {}      // host_id -> true
  };

  function selectedHosts() {
    return state.hosts.filter(function (h) { return state.selected[h.id]; });
  }

  // ── login ────────────────────────────────────────────────────────────
  function doLogin(password) {
    return fetch('/api/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: password })
    }).then(function (res) {
      if (!res.ok) {
        return res.json().catch(function () {
          return { error: res.status + ' ' + res.statusText };
        }).then(function (j) { throw new Error(j.error || 'login failed'); });
      }
      return res.json();
    });
  }

  function logout(silent) {
    localStorage.removeItem(TOKEN_KEY);
    token = '';
    state.hosts = [];
    state.selected = {};
    $('app').hidden = true;
    $('login').hidden = false;
    $('login-password').value = '';
    if (!silent) toast('logged out', 'info');
    setTimeout(function () { $('login-password').focus(); }, 0);
  }

  function boot() {
    if (!token) {
      $('login').hidden = false;
      return;
    }
    // a stale token is caught by the first authenticated call
    api('/api/hosts').then(function () { enterApp(); })
      .catch(function () { $('login').hidden = false; });
  }

  function enterApp() {
    $('login').hidden = true;
    $('app').hidden = false;
    loadHosts().then(updateRunHint);
  }

  // ── hosts ────────────────────────────────────────────────────────────
  function loadHosts() {
    return api('/api/hosts').then(function (hosts) {
      state.hosts = hosts || [];
      renderHosts();
      renderStats();
    }).catch(function (e) { toast('hosts: ' + e.message); });
  }

  function renderStats() {
    var on = state.hosts.filter(function (h) { return h.online; }).length;
    $('fleet-stats').textContent =
      state.hosts.length + ' hosts · ' + on + ' online';
    $('host-count').textContent = String(state.hosts.length);
  }

  function renderHosts() {
    var list = $('host-list');
    list.textContent = '';
    var empty = $('hosts-empty');
    if (!state.hosts.length) { empty.hidden = false; return; }
    empty.hidden = true;

    state.hosts.forEach(function (h) {
      var row = document.createElement('div');
      row.className = 'host-row ' + (h.online ? 'online' : 'offline') +
        (state.selected[h.id] ? ' selected' : '');
      row.setAttribute('role', 'listitem');
      row.dataset.hostId = h.id;

      var cb = document.createElement('input');
      cb.type = 'checkbox';
      cb.checked = !!state.selected[h.id];
      cb.setAttribute('aria-label', 'select ' + h.hostname);
      cb.addEventListener('click', function (ev) { ev.stopPropagation(); });
      cb.addEventListener('change', function () { toggleSelect(h.id); });

      var main = document.createElement('div');
      main.className = 'host-main';

      var name = document.createElement('div');
      name.className = 'host-name';
      name.textContent = h.hostname;

      var meta = document.createElement('div');
      meta.className = 'host-meta';
      meta.textContent = h.os + '/' + h.arch + ' · v' + h.version +
        (h.online ? ' · load ' + (h.load != null ? Number(h.load).toFixed(2) : '?') : '');

      main.appendChild(name);
      main.appendChild(meta);

      if (h.tags && h.tags.length) {
        var tags = document.createElement('div');
        tags.className = 'host-tags';
        h.tags.forEach(function (t) {
          var chip = document.createElement('span');
          chip.className = 'tag';
          chip.textContent = t;
          tags.appendChild(chip);
        });
        main.appendChild(tags);
      }

      var actions = document.createElement('div');
      actions.className = 'host-actions';

      var shell = document.createElement('button');
      shell.className = 'btn btn-ghost btn-sm';
      shell.textContent = 'Shell';
      shell.disabled = !h.online;
      shell.setAttribute('aria-label', 'open shell on ' + h.hostname);
      shell.addEventListener('click', function (ev) {
        ev.stopPropagation();
        openTerminal(h);
      });

      var del = document.createElement('button');
      del.className = 'btn btn-ghost btn-sm';
      del.textContent = 'Remove';
      del.setAttribute('aria-label', 'remove ' + h.hostname);
      del.addEventListener('click', function (ev) {
        ev.stopPropagation();
        confirmDelete(h);
      });

      actions.appendChild(shell);
      actions.appendChild(del);

      row.appendChild(cb);
      row.appendChild(main);
      row.appendChild(actions);

      row.addEventListener('click', function () { toggleSelect(h.id); });

      list.appendChild(row);
    });
  }

  function toggleSelect(id) {
    if (state.selected[id]) delete state.selected[id];
    else state.selected[id] = true;
    renderHosts();
    updateRunHint();
  }

  function selectAll(on) {
    state.selected = {};
    if (on) state.hosts.forEach(function (h) { state.selected[h.id] = true; });
    renderHosts();
    updateRunHint();
  }

  function updateRunHint() {
    var n = selectedHosts().length;
    var total = state.hosts.length;
    var hint = $('run-hint');
    if (n === 0) {
      hint.textContent = total
        ? 'No hosts selected — Run will target all ' + total + '.'
        : 'No hosts in the fleet yet.';
      hint.classList.toggle('bad', total === 0);
    } else {
      hint.textContent = 'Will run on ' + n + ' host' + (n === 1 ? '' : 's') +
        ' — leave empty to run on all ' + total + '.';
      hint.classList.remove('bad');
    }
    $('run-btn').textContent = n ? 'Run on ' + n : 'Run on ' + (total || 0);
  }

  // ── exec ─────────────────────────────────────────────────────────────
  var liveStreams = {};

  function runCommand() {
    var cmd = $('command-input').value.trim();
    if (!cmd) { $('command-input').focus(); return; }
    var sel = selectedHosts();
    var targets = sel.length
      ? { hosts: sel.map(function (h) { return h.id; }), tags: [], all: false }
      : { hosts: [], tags: [], all: true };

    var btn = $('run-btn');
    btn.disabled = true;
    $('results-empty').hidden = true;

    api('/api/exec', {
      method: 'POST',
      body: { targets: targets, command: cmd, timeout_sec: 300 }
    }).then(function (res) {
      res.jobs.forEach(function (j) { addResultPane(res.exec_id, j, cmd); });
      listenExec(res.exec_id, res.jobs);
    }).catch(function (e) {
      toast('run failed: ' + e.message);
      if (!$('results').children.length) $('results-empty').hidden = false;
    }).then(function () { btn.disabled = false; });
  }

  function addResultPane(execId, job, cmd) {
    var pane = document.createElement('div');
    pane.className = 'result-pane';
    pane.id = 'pane-' + job.job_id;

    var head = document.createElement('div');
    head.className = 'result-head';

    var hostBtn = document.createElement('button');
    hostBtn.className = 'result-host-btn';
    hostBtn.type = 'button';
    hostBtn.textContent = job.hostname;
    hostBtn.title = 'open a shell on ' + job.hostname;
    hostBtn.addEventListener('click', function () {
      var h = findHost(job.host_id);
      if (h) openTerminal(h);
    });

    var pill = document.createElement('span');
    pill.className = 'pill running';
    pill.id = 'pill-' + job.job_id;
    pill.textContent = 'running';

    var spacer = document.createElement('span');
    spacer.className = 'result-spacer';

    var dur = document.createElement('span');
    dur.className = 'pill';
    dur.id = 'dur-' + job.job_id;
    dur.textContent = '–';

    head.appendChild(hostBtn);
    head.appendChild(pill);
    head.appendChild(spacer);
    head.appendChild(dur);

    var out = document.createElement('div');
    out.className = 'result-out';
    out.id = 'out-' + job.job_id;
    var wait = document.createElement('span');
    wait.className = 'waiting';
    wait.textContent = 'waiting for output…';
    out.appendChild(wait);

    pane.appendChild(head);
    pane.appendChild(out);
    $('results').appendChild(pane);
    pane.scrollIntoView({ block: 'nearest' });
  }

  function findHost(id) {
    for (var i = 0; i < state.hosts.length; i++) {
      if (state.hosts[i].id === id) return state.hosts[i];
    }
    return null;
  }

  function fmtDuration(ms) {
    if (ms < 1000) return ms + 'ms';
    return (ms / 1000).toFixed(2) + 's';
  }

  function appendOutput(jobId, stream, b64) {
    var out = $('out-' + jobId);
    if (!out) return;
    var wait = out.querySelector('.waiting');
    if (wait) wait.remove();
    var span = document.createElement('span');
    span.className = stream === 'stderr' ? 'stderr' : '';
    span.textContent = decodeB64(b64);
    out.appendChild(span);
    // autoscroll unless the user scrolled up
    if (out.scrollHeight - out.clientHeight - out.scrollTop < 40) {
      out.scrollTop = out.scrollHeight;
    }
  }

  function setPill(jobId, kind, text) {
    var pill = $('pill-' + jobId);
    if (!pill) return;
    pill.className = 'pill ' + kind;
    pill.textContent = text;
  }

  function listenExec(execId, jobs) {
    var es = new EventSource('/api/exec/' + encodeURIComponent(execId) + '/events?token=' +
      encodeURIComponent(token));

    var startedAt = {};
    jobs.forEach(function (j) { startedAt[j.job_id] = Date.now(); });

    es.addEventListener('e', function (ev) {
      var ev2;
      try { ev2 = JSON.parse(ev.data); } catch (e) { return; }

      if (ev2.kind === 'output') {
        appendOutput(ev2.job_id, ev2.stream, ev2.data);
      } else if (ev2.kind === 'started') {
        setPill(ev2.job_id, 'running', 'running');
      } else if (ev2.kind === 'exit') {
        var ok = ev2.code === 0;
        setPill(ev2.job_id, ok ? 'ok' : 'fail',
          'exit ' + (ev2.code === null ? (ev2.signal !== null ? 'sig ' + ev2.signal : '?') : ev2.code));
        var d = $('dur-' + ev2.job_id);
        if (d) d.textContent = fmtDuration(ev2.duration_ms || (Date.now() - startedAt[ev2.job_id]));
        var pane = $('pane-' + ev2.job_id);
        if (pane) pane.classList.add('finished');
      } else if (ev2.kind === 'failed') {
        setPill(ev2.job_id, 'fail', 'failed');
        var out = $('out-' + ev2.job_id);
        if (out) {
          var w = out.querySelector('.waiting');
          if (w) w.remove();
          var sp = document.createElement('span');
          sp.className = 'stderr';
          sp.textContent = ev2.message + '\n';
          out.appendChild(sp);
        }
      } else if (ev2.kind === 'done') {
        es.close();
        delete liveStreams[execId];
      }
    });

    es.onerror = function () {
      // EventSource retries on its own; if the server is gone it will error.
      if (es.readyState === EventSource.CLOSED) {
        jobs.forEach(function (j) {
          setPill(j.job_id, 'dead', 'stream closed');
        });
      }
    };

    liveStreams[execId] = es;
  }

  // ── add host modal ───────────────────────────────────────────────────
  function openAddModal() {
    $('add-modal').hidden = false;
    $('install-cmd').textContent = 'generating…';
    $('install-expiry').textContent = '';
    generateToken();
    setTimeout(function () { $('copy-install-btn').focus(); }, 0);
  }

  function closeModal(id) { $(id).hidden = true; }

  function generateToken() {
    api('/api/enroll-tokens', {
      method: 'POST',
      body: { ttl_sec: 86400 }
    }).then(function (r) {
      $('install-cmd').textContent = r.install_cmd;
      var d = new Date(r.expires_at * 1000);
      $('install-expiry').textContent = 'expires ' + d.toLocaleString();
    }).catch(function (e) {
      $('install-cmd').textContent = 'error: ' + e.message;
    });
  }

  function copyInstall() {
    var text = $('install-cmd').textContent;
    var done = function () { toast('copied to clipboard', 'ok'); };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallbackCopy(text, done); });
    } else {
      fallbackCopy(text, done);
    }
  }

  function fallbackCopy(text, done) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand('copy'); done(); }
    catch (e) { toast('copy failed — select the text manually'); }
    ta.remove();
  }

  // ── delete host ──────────────────────────────────────────────────────
  var pendingDelete = null;

  function confirmDelete(host) {
    pendingDelete = host;
    $('del-hostname').textContent = host.hostname;
    $('del-modal').hidden = false;
    setTimeout(function () { $('del-confirm-btn').focus(); }, 0);
  }

  function doDelete() {
    if (!pendingDelete) return;
    var host = pendingDelete;
    closeModal('del-modal');
    api('/api/hosts/' + encodeURIComponent(host.id), { method: 'DELETE' })
      .then(function () {
        toast('removed ' + host.hostname, 'ok');
        return loadHosts().then(function () {
          delete state.selected[host.id];
          renderHosts();
          updateRunHint();
        });
      })
      .catch(function (e) { toast('remove failed: ' + e.message); })
      .then(function () { pendingDelete = null; });
  }

  // ── terminal ─────────────────────────────────────────────────────────
  var term = null;

  function openTerminal(host) {
    if (!host.online) { toast(host.hostname + ' is offline'); return; }

    var container = $('term-container');
    container.textContent = '';
    $('term-overlay').hidden = false;
    $('term-title').textContent = host.hostname + ' — shell';
    var status = $('term-status');
    status.textContent = 'connecting…';
    status.className = 'term-status';

    var cols = 80, rows = 24;

    if (!term) {
      term = new Terminal({
        fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
        fontSize: 13,
        cursorBlink: true,
        theme: {
          background: '#05070a',
          foreground: '#d6dde5',
          cursor: '#3ce6b0',
          selectionBackground: '#2c3540'
        },
        scrollback: 10000,
        allowProposedApi: true
      });
      if (window.FitAddon && window.FitAddon.FitAddon) {
        try { term.loadAddon(new window.FitAddon.FitAddon()); } catch (e) { /* optional */ }
      }
      term.open(container);
    } else {
      term.open(container);
    }
    fitTerm();

    var proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
    var url = proto + location.host + '/api/pty?token=' + encodeURIComponent(token) +
      '&host_id=' + encodeURIComponent(host.id) +
      '&cols=' + cols + '&rows=' + rows;

    var ws = new WebSocket(url);
    term._ws = ws;
    status.textContent = 'connecting…';

    var dataHandler = term.onData(function (d) {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ t: 'input', data: d }));
    });
    var resizeHandler = term.onResize(function (size) {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ t: 'resize', cols: size.cols, rows: size.rows }));
      }
    });

    ws.onopen = function () {
      status.textContent = 'connected';
      fitTerm();
      term.focus();
    };
    ws.onmessage = function (ev) {
      var m;
      try { m = JSON.parse(ev.data); } catch (e) { return; }
      // pty output is base64 raw bytes; hand xterm the bytes so it can
      // decode UTF-8 across chunk boundaries.
      if (m.t === 'output') term.write(b64Bytes(m.data));
      else if (m.t === 'exit') {
        status.textContent = 'exited' + (m.code !== null ? ' (' + m.code + ')' : '');
        term.write('\r\n\x1b[38;5;244m[session closed]\x1b[0m\r\n');
        setTimeout(closeTerminal, 900);
      } else if (m.t === 'error') {
        status.textContent = m.message;
        status.className = 'term-status bad';
        term.write('\r\n\x1b[31merror: ' + m.message + '\x1b[0m\r\n');
      }
    };
    ws.onerror = function () {
      status.textContent = 'connection failed';
      status.className = 'term-status bad';
    };
    ws.onclose = function () {
      if ($('term-overlay').hidden === false) {
        status.textContent = 'disconnected';
        status.className = 'term-status bad';
      }
      if (dataHandler) dataHandler.dispose();
      if (resizeHandler) resizeHandler.dispose();
      term._ws = null;
    };

    term.focus();
  }

  function fitTerm() {
    if (term && typeof term.fit === 'function') {
      try { term.fit(); } catch (e) { /* viewport not ready */ }
    }
  }

  function closeTerminal() {
    if (term && term._ws && term._ws.readyState === WebSocket.OPEN) {
      term._ws.send(JSON.stringify({ t: 'close' }));
      term._ws.close();
    }
    $('term-overlay').hidden = true;
    $('term-status').textContent = '';
  }

  // ── wiring ───────────────────────────────────────────────────────────
  $('login-form').addEventListener('submit', function (ev) {
    ev.preventDefault();
    var err = $('login-error');
    err.hidden = true;
    var btn = $('login-submit');
    btn.disabled = true;
    doLogin($('login-password').value).then(function (r) {
      token = r.token;
      localStorage.setItem(TOKEN_KEY, token);
      enterApp();
    }).catch(function (e) {
      err.textContent = e.message;
      err.hidden = false;
      $('login-password').select();
    }).then(function () { btn.disabled = false; });
  });

  $('logout-btn').addEventListener('click', function () { logout(false); });
  $('refresh-btn').addEventListener('click', function () {
    loadHosts().then(updateRunHint);
  });
  $('select-all').addEventListener('change', function (ev) {
    selectAll(ev.target.checked);
  });
  $('run-btn').addEventListener('click', runCommand);
  $('command-input').addEventListener('keydown', function (ev) {
    if (ev.key === 'Enter') { ev.preventDefault(); runCommand(); }
  });
  $('clear-btn').addEventListener('click', function () {
    $('results').textContent = '';
    $('results-empty').hidden = false;
  });

  $('add-host-btn').addEventListener('click', openAddModal);
  $('regenerate-btn').addEventListener('click', generateToken);
  $('copy-install-btn').addEventListener('click', copyInstall);
  $('del-confirm-btn').addEventListener('click', doDelete);
  $('term-close-btn').addEventListener('click', closeTerminal);

  Array.prototype.forEach.call(document.querySelectorAll('[data-close]'), function (btn) {
    btn.addEventListener('click', function () { closeModal(btn.dataset.close); });
  });
  Array.prototype.forEach.call(document.querySelectorAll('.modal-backdrop'), function (bd) {
    bd.addEventListener('click', function (ev) {
      if (ev.target === bd) bd.hidden = true;
    });
  });

  window.addEventListener('resize', function () {
    if (!$('term-overlay').hidden && term) fitTerm();
  });
  document.addEventListener('keydown', function (ev) {
    if (ev.key !== 'Escape') return;
    if (!$('add-modal').hidden) closeModal('add-modal');
    else if (!$('del-modal').hidden) closeModal('del-modal');
  });

  // auto-refresh the fleet view; user selection is preserved in state
  setInterval(function () {
    if ($('app').hidden) return;
    loadHosts().then(updateRunHint);
  }, 15000);

  boot();
})();
