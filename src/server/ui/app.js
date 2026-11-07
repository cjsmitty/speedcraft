/* Speedcraft UI. Vanilla JS, no build step. Talks only to this app's /api;
   remote agents are reached through /api/agents/{id}/... proxying. */
(() => {
  const $ = (s) => document.querySelector(s);
  const state = {
    agent: 'local',          // 'local' or a remote agent id
    agents: [],              // remote agents from the registry
    agentHealth: {},         // id -> 'ok' | 'bad' | 'run'
    status: null,
    lastSeq: 0,
    events: [],
    edgeHistory: {},         // pair -> [{t, v}]
    token: localStorage.getItem('speedcraft_token') || '',
    isHost: true,            // false when this instance is itself a headless agent
    configDirty: false,
  };

  // ---------- transport ----------
  const base = () => state.agent === 'local' ? '/api' : `/api/agents/${encodeURIComponent(state.agent)}`;
  async function api(path, opts = {}, agentBase = base()) {
    const headers = Object.assign({ 'content-type': 'application/json' }, opts.headers || {});
    if (state.token) headers['authorization'] = 'Bearer ' + state.token;
    const r = await fetch(agentBase + path, Object.assign({}, opts, { headers }));
    if (r.status === 401 && agentBase.startsWith('/api') && !agentBase.includes('/agents/')) {
      showTokenModal();
      throw new Error('token required');
    }
    const text = await r.text();
    let body = null;
    try { body = text ? JSON.parse(text) : null; } catch { body = { error: text }; }
    if (!r.ok) throw new Error((body && body.error) || `${r.status} ${r.statusText}`);
    return body;
  }
  const post = (p, b) => api(p, { method: 'POST', body: JSON.stringify(b || {}) });

  // ---------- formatting ----------
  const fmt = (v, d = 2) => (v === null || v === undefined || v === '') ? '—' : Number(v).toLocaleString(undefined, { minimumFractionDigits: d, maximumFractionDigits: d });
  const fmtPx = (v) => { const n = Number(v); return n >= 1000 ? fmt(n, 2) : n >= 10 ? fmt(n, 3) : fmt(n, 5); };
  const signed = (v, d = 2) => { const n = Number(v); return (n > 0 ? '+' : '') + fmt(n, d); };
  const cls = (v) => Number(v) > 0 ? 'pos' : Number(v) < 0 ? 'neg' : '';
  const ago = (ms) => ms < 1000 ? `${ms} ms` : ms < 60000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 60000)} min`;
  const dur = (s) => s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)}m ${s % 60}s` : `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  function toast(msg, err = false) {
    const t = $('#toast'); t.textContent = msg; t.hidden = false; t.className = 'toast' + (err ? ' err' : '');
    clearTimeout(toast._h); toast._h = setTimeout(() => (t.hidden = true), err ? 6000 : 3000);
  }

  // ---------- rendering ----------
  function renderTabs() {
    const nav = $('#agent-tabs');
    const tabs = [{ id: 'local', name: state.isHost ? 'This Mac' : 'This agent' }].concat(state.agents.map((a) => ({ id: a.id, name: a.name })));
    nav.innerHTML = tabs.map((t) => {
      const h = t.id === 'local' ? (state.status ? (state.status.engine.state === 'running' ? 'run' : 'ok') : 'bad') : (state.agentHealth[t.id] || '');
      return `<button data-id="${esc(t.id)}" class="${t.id === state.agent ? 'active' : ''}"><span class="dot ${h}"></span>${esc(t.name)}</button>`;
    }).join('');
    nav.querySelectorAll('button').forEach((b) => b.onclick = () => switchAgent(b.dataset.id));
  }

  function renderBanners(st) {
    const b = [];
    const snap = st.snapshot || {};
    if (st.engine.state === 'running' && st.engine.mode === 'live') b.push(`<div class="banner red">● LIVE TRADING with real money on ${esc(st.agent)}. Losses are bounded only by your configured limits.</div>`);
    if (snap.halted) b.push(`<div class="banner amber">⛔ Trading halted: ${esc(snap.halted)} <button class="btn" onclick="window.sc.resume()">Review done, resume</button></div>`);
    if (st.stop_file_present) b.push(`<div class="banner amber">⏸ STOP file present on this agent. No orders will be sent until it is removed.</div>`);
    if (st.engine.last_error) b.push(`<div class="banner red">Engine exited with an error: ${esc(st.engine.last_error)}</div>`);
    if (st.config.config_error) b.push(`<div class="banner red">Config error: ${esc(st.config.config_error)}</div>`);
    if (snap.disabled_exchanges && snap.disabled_exchanges.length) b.push(`<div class="banner amber">Circuit breaker tripped for: ${snap.disabled_exchanges.map(esc).join(', ')}. Restart the engine to re-enable.</div>`);
    $('#banners').innerHTML = b.join('');
  }

  function renderControls(st) {
    const pill = $('#state-pill');
    const s = st.engine.state, m = st.engine.mode, halted = st.snapshot && st.snapshot.halted;
    pill.className = 'state-pill ' + (s === 'running' ? (halted ? 'halted' : m) : '');
    pill.textContent = s === 'stopped' ? 'STOPPED' : s === 'starting' ? 'STARTING…' : s === 'stopping' ? 'STOPPING…' : (halted ? `${m.toUpperCase()} · HALTED` : `${m.toUpperCase()} · RUNNING`);
    const stopped = s === 'stopped';
    $('#btn-paper').disabled = !stopped; $('#btn-live').disabled = !stopped; $('#btn-stop').disabled = stopped;
    $('#btn-halt').disabled = stopped || !!halted; $('#btn-resume').disabled = stopped || !halted;
    const chk = $('#chk-stopfile'); if (document.activeElement !== chk) chk.checked = !!st.stop_file_present;
    $('#agent-meta').textContent = `${st.agent} · ${st.hostname} · v${st.version} · up ${dur(st.uptime_s)} · config ${st.config.path} · exchanges ${st.config.exchanges.join(', ')} · pairs ${st.config.pairs.join(', ')} · min edge ${st.config.min_edge_bps} bps · max notional ${st.config.max_notional_quote} · max daily loss ${st.config.max_daily_loss_quote}`;
  }

  function kpi(label, value, sub = '', c = '') { return `<div class="kpi"><div class="label">${label}</div><div class="value ${c}">${value}</div><div class="sub">${sub}</div></div>`; }
  function renderKpis(st) {
    const s = st.snapshot || {};
    const run = st.engine.state === 'running';
    $('#kpis').innerHTML = [
      kpi('PnL today', run ? signed(s.pnl_today, 4) : '—', 'realised, quote currency', cls(s.pnl_today)),
      kpi('PnL this run', run ? signed(s.realised_pnl, 4) : '—', `${s.successes || 0} / ${s.trades || 0} trades both-legs`, cls(s.realised_pnl)),
      kpi('Opportunities', run ? (s.opportunities || 0) : '—', 'after fees, this run'),
      kpi('Quotes / s', run ? fmt(s.quotes_per_sec, 1) : '—', `${(s.quotes_seen || 0).toLocaleString()} seen`),
      kpi('Failure streak', run ? (s.consecutive_failures || 0) : '—', 'consecutive unwound trades', s.consecutive_failures ? 'neg' : ''),
      kpi('Engine uptime', run ? dur(Math.max(0, Math.floor((st.now_ms - s.started_ms) / 1000))) : '—', st.engine.mode || ''),
    ].join('');
  }

  function renderEdges(st) {
    const s = st.snapshot || {};
    const edges = s.edges || [];
    const minEdge = Number(s.min_edge_bps || 0);
    const t = $('#edges');
    if (!edges.length) { t.innerHTML = `<tr><td class="empty">No cross-exchange quotes yet${st.engine.state === 'running' ? ' (waiting for both feeds)' : ' (engine stopped)'}.</td></tr>`; return; }
    const maxAbs = Math.max(10, ...edges.map((e) => Math.abs(Number(e.net_bps))));
    t.innerHTML = `<tr><th>Pair</th><th>Buy on</th><th>Sell on</th><th class="num">Gross bps</th><th class="num">Net bps</th><th>vs. minimum ${minEdge} bps</th></tr>` +
      edges.map((e) => {
        const n = Number(e.net_bps), w = Math.min(100, Math.abs(n) / maxAbs * 100);
        return `<tr><td>${esc(e.pair)}</td><td>${esc(e.buy_exchange)}</td><td>${esc(e.sell_exchange)}</td><td class="num">${signed(e.gross_bps)}</td><td class="num ${cls(n)}">${signed(n)}</td><td class="barcell"><span class="bar ${e.tradable ? 'ok' : ''}" style="width:${Math.round(w * 1.2)}px"></span> <span class="tag ${e.tradable ? 'ok' : 'no'}">${e.tradable ? 'TRADABLE' : n >= minEdge ? 'STALE QUOTE' : `${fmt(minEdge - n, 1)} bps short`}</span>${e.fresh ? '' : ' <span class="tag warn">stale</span>'}</td></tr>`;
      }).join('');
  }

  function renderBooks(st) {
    const books = (st.snapshot && st.snapshot.books) || [];
    const t = $('#books');
    if (!books.length) { t.innerHTML = `<tr><td class="empty">No quotes.</td></tr>`; return; }
    t.innerHTML = `<tr><th>Pair</th><th>Exchange</th><th class="num">Bid</th><th class="num">Ask</th><th class="num">Spread bps</th><th class="num">Age</th></tr>` +
      books.map((b) => {
        const bid = Number(b.bid), ask = Number(b.ask), sp = (ask - bid) / ask * 1e4;
        const stale = b.age_ms > 5000;
        return `<tr><td>${esc(b.pair)}</td><td>${esc(b.exchange)}</td><td class="num">${fmtPx(bid)}<br><small>${fmt(b.bid_qty, 4)}</small></td><td class="num">${fmtPx(ask)}<br><small>${fmt(b.ask_qty, 4)}</small></td><td class="num">${fmt(sp, 2)}</td><td class="num ${stale ? 'neg' : ''}">${ago(b.age_ms)}</td></tr>`;
      }).join('');
  }

  function renderBalances(st) {
    const bal = (st.snapshot && st.snapshot.balances) || {};
    const fees = (st.snapshot && st.snapshot.fees_taker_bps) || {};
    const exs = Object.keys(bal).sort();
    const t = $('#balances');
    if (!exs.length) { t.innerHTML = `<tr><td class="empty">No balances (engine stopped or preflight pending).</td></tr>`; return; }
    t.innerHTML = `<tr><th>Exchange</th><th>Asset</th><th class="num">Available</th><th class="num">Taker fee</th></tr>` +
      exs.flatMap((ex) => Object.entries(bal[ex]).filter(([, v]) => Number(v) !== 0).sort().map(([a, v], i) =>
        `<tr><td>${i === 0 ? esc(ex) : ''}</td><td>${esc(a)}</td><td class="num">${fmt(v, 6)}</td><td class="num">${i === 0 && fees[ex] !== undefined ? fmt(fees[ex], 1) + ' bps' : ''}</td></tr>`)).join('');
  }

  function describe(e) {
    switch (e.event) {
      case 'trade': return `${e.pair} buy ${e.buy_exchange} / sell ${e.sell_exchange} qty ${e.matched_qty} · pnl ${signed(e.realised_pnl_quote, 4)} (expected ${signed(e.expected_pnl_quote, 4)}) · ${e.success ? 'OK' : 'FAILED'} · ${esc(e.note)}`;
      case 'opportunity': { const o = e.op; return `${o.pair} buy ${o.buy_exchange} @ ${fmtPx(o.buy_price)} sell ${o.sell_exchange} @ ${fmtPx(o.sell_price)} qty ${o.qty} net ${signed(o.net_edge_bps)} bps ≈ ${signed(o.expected_profit_quote, 4)}`; }
      case 'rejected': return `${e.op.pair}: ${esc(e.reason)}`;
      case 'leg': return `${e.unwind ? 'UNWIND ' : ''}${e.side} ${e.requested_qty} ${e.pair} on ${e.exchange} @ limit ${fmtPx(e.limit_price)} → ${e.result.status} filled ${e.result.filled_qty} @ ${fmtPx(e.result.avg_price)} fee ${fmt(e.result.fee_quote, 4)}`;
      case 'halt': return esc(e.reason);
      case 'start': return `${e.mode} mode, v${e.version}`;
      default: return '';
    }
  }
  function renderEvents() {
    const el = $('#events');
    if (!state.events.length) { el.innerHTML = `<div class="empty">No events yet. In paper mode on real markets at retail fees, expect this to stay quiet: that means the bot is correctly refusing unprofitable trades.</div>`; return; }
    el.innerHTML = state.events.slice(0, 300).map((e) => `<div class="ev ${e.event}"><span class="t">${new Date(e.ts_ms).toLocaleTimeString()}</span><span class="k">${e.event.toUpperCase()}</span><span>${describe(e)}</span></div>`).join('');
  }

  function recordEdgeHistory(st) {
    const s = st.snapshot || {};
    const now = Date.now();
    const best = {};
    (s.edges || []).forEach((e) => { const n = Number(e.net_bps); if (best[e.pair] === undefined || n > best[e.pair]) best[e.pair] = n; });
    Object.entries(best).forEach(([p, v]) => {
      const h = state.edgeHistory[p] = state.edgeHistory[p] || [];
      h.push({ t: now, v }); if (h.length > 600) h.shift();
    });
  }
  const COLORS = ['#2563eb', '#db2777', '#059669', '#d97706', '#7c3aed'];
  function drawChart(st) {
    const c = $('#edge-chart'); const dpr = window.devicePixelRatio || 1;
    const W = c.clientWidth, H = 140; c.width = W * dpr; c.height = H * dpr;
    const ctx = c.getContext('2d'); ctx.scale(dpr, dpr); ctx.clearRect(0, 0, W, H);
    const pairs = Object.keys(state.edgeHistory);
    const minEdge = Number((st.snapshot && st.snapshot.min_edge_bps) || 0);
    const styles = getComputedStyle(document.documentElement);
    const muted = styles.getPropertyValue('--muted').trim(), line = styles.getPropertyValue('--line').trim();
    if (!pairs.length) { ctx.fillStyle = muted; ctx.font = '12px sans-serif'; ctx.fillText('Best net edge over time appears here once two exchanges are quoting.', 10, 24); return; }
    let all = pairs.flatMap((p) => state.edgeHistory[p].map((x) => x.v)).concat([minEdge, 0]);
    let lo = Math.min(...all), hi = Math.max(...all); if (hi - lo < 10) { hi += 5; lo -= 5; }
    const pad = 36, y = (v) => H - 18 - (v - lo) / (hi - lo) * (H - 30), t0 = Math.min(...pairs.map((p) => state.edgeHistory[p][0].t)), t1 = Date.now();
    const x = (t) => pad + (t - t0) / Math.max(1, t1 - t0) * (W - pad - 8);
    ctx.strokeStyle = line; ctx.lineWidth = 1;
    [lo, 0, minEdge, hi].forEach((v) => { ctx.beginPath(); ctx.moveTo(pad, y(v)); ctx.lineTo(W - 8, y(v)); ctx.stroke(); ctx.fillStyle = muted; ctx.font = '11px sans-serif'; ctx.fillText(`${v.toFixed(0)}`, 2, y(v) + 4); });
    ctx.fillStyle = muted; ctx.fillText(`min ${minEdge} bps`, W - 80, y(minEdge) + 12);
    pairs.forEach((p, i) => {
      const h = state.edgeHistory[p]; ctx.strokeStyle = COLORS[i % COLORS.length]; ctx.lineWidth = 1.6; ctx.beginPath();
      h.forEach((pt, j) => { const X = x(pt.t), Y = y(pt.v); j ? ctx.lineTo(X, Y) : ctx.moveTo(X, Y); }); ctx.stroke();
      ctx.fillStyle = COLORS[i % COLORS.length]; ctx.fillText(`${p} ${h[h.length - 1].v.toFixed(1)} bps`, pad + 6 + i * 150, H - 4);
    });
  }

  function renderAgents() {
    const card = $('#agents-card');
    if (!state.isHost) { card.hidden = true; return; }
    card.hidden = false;
    const t = $('#agents-table');
    if (!state.agents.length) { t.innerHTML = `<tr><td class="empty">No remote agents yet. Install one on a Linux VM with <code>install/install-agent.sh</code>, then add it here.</td></tr>`; return; }
    t.innerHTML = `<tr><th>Name</th><th>URL</th><th>Status</th><th></th></tr>` + state.agents.map((a) => {
      const h = state.agentHealth[a.id];
      const tag = h === 'run' ? '<span class="tag ok">RUNNING</span>' : h === 'ok' ? '<span class="tag warn">IDLE</span>' : h === 'bad' ? '<span class="tag no">UNREACHABLE</span>' : '…';
      return `<tr><td>${esc(a.name)}</td><td>${esc(a.url)}${a.has_token ? '' : ' <span class="tag warn">no token</span>'}</td><td>${tag}</td><td><button class="btn" data-rm="${esc(a.id)}">Remove</button></td></tr>`;
    }).join('');
    t.querySelectorAll('[data-rm]').forEach((b) => b.onclick = async () => {
      if (!confirm(`Remove agent "${b.dataset.rm}" from this app? (The agent keeps running on its box.)`)) return;
      try { await api(`/agents/${encodeURIComponent(b.dataset.rm)}`, { method: 'DELETE' }, '/api'); if (state.agent === b.dataset.rm) switchAgent('local'); await loadAgents(); toast('Agent removed'); } catch (e) { toast(e.message, true); }
    });
  }

  function render() {
    const st = state.status; if (!st) return;
    renderTabs(); renderBanners(st); renderControls(st); renderKpis(st); renderEdges(st); renderBooks(st); renderBalances(st); drawChart(st);
  }

  // ---------- polling ----------
  async function pollStatus() {
    try {
      const st = await api('/status');
      state.status = st; recordEdgeHistory(st); render();
      $('#conn').className = 'conn ok'; $('#conn-text').textContent = state.agent === 'local' ? 'connected' : `connected to ${st.agent}`;
    } catch (e) {
      $('#conn').className = 'conn bad'; $('#conn-text').textContent = e.message;
      if (state.agent !== 'local') { state.status = null; }
    }
  }
  async function pollEvents() {
    try {
      const r = await api(`/events?since=${state.lastSeq}&limit=500`);
      if (r.events.length) { state.events = r.events.slice().reverse().concat(state.events).slice(0, 2000); state.lastSeq = r.latest_seq; renderEvents(); }
      else if (r.latest_seq < state.lastSeq) { state.lastSeq = 0; state.events = []; renderEvents(); } // agent restarted
    } catch { /* status poll reports connectivity */ }
  }
  async function pollAgentHealth() {
    if (!state.isHost) return;
    await Promise.all(state.agents.map(async (a) => {
      try { const st = await api('/status', {}, `/api/agents/${encodeURIComponent(a.id)}`); state.agentHealth[a.id] = st.engine.state === 'running' ? 'run' : 'ok'; }
      catch { state.agentHealth[a.id] = 'bad'; }
    }));
    renderTabs(); renderAgents();
  }
  async function loadAgents() {
    try { const r = await api('/agents', {}, '/api'); state.agents = r.agents; state.isHost = true; }
    catch (e) { if (/headless/.test(e.message)) state.isHost = false; }
    renderTabs(); renderAgents(); pollAgentHealth();
  }
  async function loadConfig() {
    try {
      const r = await api('/config'); $('#config-text').value = r.toml; $('#config-path').textContent = r.path; state.configDirty = false; $('#config-msg').textContent = '';
    } catch (e) { $('#config-msg').textContent = e.message; }
  }
  function switchAgent(id) {
    state.agent = id; state.lastSeq = 0; state.events = []; state.edgeHistory = {}; state.status = null;
    renderEvents(); renderTabs(); pollStatus(); pollEvents(); loadConfig();
  }

  // ---------- actions ----------
  const act = async (fn, okMsg) => { try { await fn(); if (okMsg) toast(okMsg); await pollStatus(); } catch (e) { toast(e.message, true); } };
  $('#btn-paper').onclick = () => act(() => post('/engine/start', { mode: 'paper' }), 'Paper engine starting');
  $('#btn-stop').onclick = () => act(() => post('/engine/stop'), 'Stopping engine');
  $('#btn-halt').onclick = () => act(() => post('/engine/halt', { reason: 'halted from UI' }), 'Trading halted');
  $('#btn-resume').onclick = () => act(() => post('/engine/resume'), 'Trading resumed');
  window.sc = { resume: () => $('#btn-resume').onclick() };
  $('#chk-stopfile').onchange = (ev) => act(() => post('/stopfile', { present: ev.target.checked }), ev.target.checked ? 'STOP file created' : 'STOP file removed');
  $('#btn-live').onclick = () => {
    const st = state.status; if (!st) return;
    $('#live-agent-name').textContent = st.agent;
    const gates = [[st.config.live_in_config, 'live = true in config.toml'], [st.config.live_env_set, 'SPEEDCRAFT_I_ACCEPT_LOSSES=yes in the agent process environment'], [true, 'Confirmation phrase typed below']];
    $('#live-gates').innerHTML = gates.map(([ok, t]) => `<li class="${ok ? 'ok' : 'no'}">${ok ? '✓' : '✗'} ${t}</li>`).join('');
    $('#live-phrase').value = ''; $('#live-go').disabled = true; $('#modal-live').hidden = false; $('#live-phrase').focus();
  };
  $('#live-phrase').oninput = (e) => { $('#live-go').disabled = e.target.value.trim() !== 'I ACCEPT LOSSES'; };
  $('#live-cancel').onclick = () => { $('#modal-live').hidden = true; };
  $('#live-go').onclick = () => { $('#modal-live').hidden = true; act(() => post('/engine/start', { mode: 'live', confirm: $('#live-phrase').value }), 'LIVE engine starting'); };
  $('#btn-config-reload').onclick = loadConfig;
  $('#config-text').oninput = () => { state.configDirty = true; };
  $('#btn-config-save').onclick = () => act(async () => {
    const r = await api('/config', { method: 'PUT', body: JSON.stringify({ toml: $('#config-text').value }) });
    $('#config-msg').textContent = r.restart_required ? 'Saved. Restart the engine to apply.' : 'Saved.'; state.configDirty = false;
  }, 'Config saved');
  $('#agent-form').onsubmit = (ev) => { ev.preventDefault(); act(async () => {
    await api('/agents', { method: 'POST', body: JSON.stringify({ name: $('#agent-name').value, url: $('#agent-url').value, token: $('#agent-token').value }) }, '/api');
    $('#agent-form').reset(); await loadAgents();
  }, 'Agent added'); };
  function showTokenModal() { $('#modal-token').hidden = false; }
  $('#token-save').onclick = () => { state.token = $('#token-input').value.trim(); localStorage.setItem('speedcraft_token', state.token); $('#modal-token').hidden = true; pollStatus(); loadAgents(); loadConfig(); };
  window.addEventListener('beforeunload', (e) => { if (state.configDirty) { e.preventDefault(); e.returnValue = ''; } });
  window.addEventListener('resize', () => state.status && drawChart(state.status));

  // ---------- boot ----------
  loadAgents(); loadConfig(); pollStatus(); pollEvents(); renderEvents();
  setInterval(pollStatus, 1000); setInterval(pollEvents, 2000); setInterval(pollAgentHealth, 5000);
})();
