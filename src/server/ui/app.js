/* Speedcraft control panel. Vanilla JS, no build step. Talks only to this
   app's /api; other computers are reached through /api/agents/{id}/... */
(() => {
  const $ = (s) => document.querySelector(s);
  const state = {
    agent: 'local', agents: [], agentHealth: {}, status: null, lastSeq: 0, events: [],
    edgeHistory: {}, token: localStorage.getItem('speedcraft_token') || '', isHost: true,
    configDirty: false, scan: null, scanning: false,
  };

  // ---------- transport ----------
  const base = () => state.agent === 'local' ? '/api' : `/api/agents/${encodeURIComponent(state.agent)}`;
  async function api(path, opts = {}, agentBase = base()) {
    const headers = Object.assign({ 'content-type': 'application/json' }, opts.headers || {});
    if (state.token) headers['authorization'] = 'Bearer ' + state.token;
    const r = await fetch(agentBase + path, Object.assign({}, opts, { headers }));
    if (r.status === 401 && !agentBase.includes('/agents/')) { $('#modal-token').hidden = false; throw new Error('token required'); }
    const text = await r.text();
    let body = null;
    try { body = text ? JSON.parse(text) : null; } catch { body = { error: text }; }
    if (!r.ok) throw new Error((body && body.error) || `${r.status} ${r.statusText}`);
    return body;
  }
  const post = (p, b) => api(p, { method: 'POST', body: JSON.stringify(b || {}) });

  // ---------- formatting ----------
  const N = (v) => Number(v);
  const fmt = (v, d = 2) => (v === null || v === undefined || v === '' || Number.isNaN(N(v))) ? '—' : N(v).toLocaleString(undefined, { minimumFractionDigits: d, maximumFractionDigits: d });
  const usd = (v, d = 2) => { const n = N(v); return (n < 0 ? '−$' : '$') + fmt(Math.abs(n), d); };
  const fmtPx = (v) => { const n = N(v); return n >= 1000 ? fmt(n, 2) : n >= 10 ? fmt(n, 3) : n >= 0.01 ? fmt(n, 5) : n.toPrecision(3); };
  const cls = (v) => N(v) > 0 ? 'pos' : N(v) < 0 ? 'neg' : 'zero';
  const ago = (ms) => ms < 1000 ? `${ms} ms` : ms < 60000 ? `${(ms / 1000).toFixed(1)} s` : `${Math.round(ms / 60000)} min`;
  const dur = (s) => s < 60 ? `${s}s` : s < 3600 ? `${Math.floor(s / 60)} min` : s < 86400 ? `${(s / 3600).toFixed(1)} h` : `${(s / 86400).toFixed(1)} days`;
  const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
  const exName = (e) => ({ coinbase: 'Coinbase', kraken: 'Kraken', binance: 'Binance', binance_us: 'Binance.US' }[e] || e);
  // 1 bp on $10,000 is $1.
  const per10k = (bps) => usd(N(bps), 0);
  function toast(msg, err = false) {
    const t = $('#toast'); t.textContent = msg; t.hidden = false; t.className = 'toast' + (err ? ' err' : '');
    clearTimeout(toast._h); toast._h = setTimeout(() => (t.hidden = true), err ? 7000 : 3000);
  }

  // ---------- derived facts ----------
  function facts(st) {
    const s = st.snapshot || {};
    const running = st.engine.state === 'running';
    const uptimeS = running ? Math.max(1, Math.floor((st.now_ms - s.started_ms) / 1000)) : 0;
    const pnl = N(s.realised_pnl || 0);
    const perHour = uptimeS > 0 ? pnl / (uptimeS / 3600) : 0;
    const best = Object.entries(s.best_net_edge || {}).map(([p, v]) => [p, N(v)]).sort((a, b) => b[1] - a[1])[0] || null;
    return { running, uptimeS, pnl, perDay: perHour * 24, perMonth: perHour * 24 * 30, trades: s.trades || 0, successes: s.successes || 0, opps: s.opportunities || 0, best, minEdge: N(s.min_edge_bps || st.config.min_edge_bps || 0), mode: st.engine.mode };
  }

  // ---------- rendering ----------
  function renderTabs() {
    const nav = $('#agent-tabs');
    const tabs = [{ id: 'local', name: state.isHost ? 'This computer' : 'This agent' }].concat(state.agents.map((a) => ({ id: a.id, name: a.name })));
    nav.innerHTML = tabs.map((t) => {
      const h = t.id === 'local' ? (state.status ? (state.status.engine.state === 'running' ? 'run' : 'ok') : 'bad') : (state.agentHealth[t.id] || '');
      return `<button data-id="${esc(t.id)}" class="${t.id === state.agent ? 'active' : ''}"><span class="dot ${h}"></span>${esc(t.name)}</button>`;
    }).join('');
    nav.querySelectorAll('button').forEach((b) => b.onclick = () => switchAgent(b.dataset.id));
  }

  function renderBanners(st) {
    const b = []; const s = st.snapshot || {};
    if (st.engine.state === 'running' && st.engine.mode === 'live') b.push(`<div class="banner red">● LIVE: trading real money on ${esc(st.agent)}. Losses are capped only by your daily limit (${usd(st.config.max_daily_loss_quote)}).</div>`);
    if (s.halted) b.push(`<div class="banner amber">⛔ Trading stopped by a safety rule: ${esc(s.halted)}. The bot is still watching. <button class="btn" onclick="window.sc.resume()">I have checked, resume</button></div>`);
    if (st.stop_file_present) b.push(`<div class="banner amber">⏸ Pause file present on this computer. No orders will be sent until you untick "Pause file".</div>`);
    if (st.engine.last_error) b.push(`<div class="banner red">The engine stopped with an error: ${esc(st.engine.last_error)}</div>`);
    if (st.config.config_error) b.push(`<div class="banner red">Settings file problem: ${esc(st.config.config_error)}</div>`);
    if (s.disabled_exchanges && s.disabled_exchanges.length) b.push(`<div class="banner amber">${s.disabled_exchanges.map(exName).join(', ')} stopped answering and was switched off. Press Stop then Start to retry.</div>`);
    $('#banners').innerHTML = b.join('');
  }

  function renderHero(st) {
    const f = facts(st); const s = st.snapshot || {};
    const pill = $('#hero-state'); const stt = st.engine.state;
    pill.className = 'hero-state ' + (stt === 'running' ? (s.halted ? 'halted' : f.mode) : '');
    pill.textContent = stt === 'stopped' ? 'Off' : stt === 'starting' ? 'Starting' : stt === 'stopping' ? 'Stopping' : s.halted ? 'Stopped by safety rule' : f.mode === 'live' ? 'LIVE · real money' : 'Watching · pretend money';
    const coins = st.config.pairs.length, exs = st.config.exchanges.map(exName).join(' and ');
    let title, sub;
    if (stt !== 'running') {
      title = 'Not running';
      sub = `Press "Start watching" to follow ${coins} coins on ${exs} with pretend money. Nothing is bought or sold.`;
    } else if (s.halted) {
      title = 'Paused by a safety rule';
      sub = `Reason: ${s.halted}. Prices are still being watched. Read the reason, then press Resume.`;
    } else if (f.mode === 'live') {
      title = `Trading real money on ${exs}`;
      sub = `${f.trades} trades so far, ${f.successes} fully completed. It stops itself at a ${usd(st.config.max_daily_loss_quote)} daily loss.`;
    } else {
      title = `Watching ${coins} coins on ${exs}`;
      sub = f.opps === 0
        ? `Pretend money, real prices. In ${dur(f.uptimeS)} there has not been a single moment where a price gap beat the fees, so no trade was made. That is the bot being careful, not broken.`
        : `Pretend money, real prices. ${f.opps} moments beat the fees in ${dur(f.uptimeS)}; ${f.trades} pretend trades were made.`;
    }
    $('#hero-title').textContent = title; $('#hero-sub').textContent = sub;
    const chips = [
      `<span class="chip">${esc(st.agent)} on <b>${esc(st.hostname)}</b></span>`,
      `<span class="chip">prices/sec <b>${fmt(s.quotes_per_sec, 1)}</b></span>`,
      `<span class="chip">needs a gap of at least <b>${f.minEdge} bps</b> after fees</span>`,
      `<span class="chip">max per trade <b>${usd(st.config.max_notional_quote, 0)}</b></span>`,
      `<span class="chip">daily loss limit <b>${usd(st.config.max_daily_loss_quote, 0)}</b></span>`,
      st.recorder.active ? `<span class="chip">● recording <b>${st.recorder.count.toLocaleString()}</b> prices</span>` : '',
    ];
    $('#hero-chips').innerHTML = chips.join('');
    const stopped = stt === 'stopped';
    $('#btn-paper').disabled = !stopped; $('#btn-live').disabled = !stopped; $('#btn-stop').disabled = stopped;
    $('#btn-halt').disabled = stopped || !!s.halted; $('#btn-resume').disabled = stopped || !s.halted;
    const chk = $('#chk-stopfile'); if (document.activeElement !== chk) chk.checked = !!st.stop_file_present;
    $('#raw-meta').textContent = JSON.stringify({ agent: st.agent, host: st.hostname, version: st.version, config: st.config.path, engine: st.engine, uptime_s: st.uptime_s, fees_taker_bps: s.fees_taker_bps, best_net_edge_bps: s.best_net_edge, consecutive_failures: s.consecutive_failures, quotes_seen: s.quotes_seen }, null, 2);
  }

  function renderVerdict(st) {
    const f = facts(st); const v = $('#verdict');
    $('#verdict-window').textContent = f.running ? `based on this run: ${dur(f.uptimeS)} of ${f.mode === 'live' ? 'real' : 'pretend'} trading` : 'start watching to measure';
    const tile = (label, value, sub, c) => `<div class="tile"><div class="label">${label}</div><div class="value ${c}">${value}</div><div class="sub">${sub}</div></div>`;
    if (!f.running) {
      $('#money').innerHTML = tile('Made so far', '—', 'not running', 'zero') + tile('Per day at this rate', '—', '', 'zero') + tile('Per month at this rate', '—', '', 'zero');
      $('#verdict-text').textContent = 'No data yet. Press "Start watching" and leave it running; this panel fills in from what actually happens.';
      v.className = 'card verdict'; return;
    }
    const tooShort = f.uptimeS < 3600;
    $('#money').innerHTML =
      tile('Made so far', usd(f.pnl, 2), `${f.trades} trades, ${f.successes} fully completed`, cls(f.pnl)) +
      tile('Per day at this rate', tooShort && f.trades === 0 ? '$0' : usd(f.perDay, 2), tooShort ? 'needs at least an hour to mean much' : 'if the last hours repeat', cls(f.perDay)) +
      tile('Per month at this rate', tooShort && f.trades === 0 ? '$0' : usd(f.perMonth, 0), f.mode === 'paper' ? 'pretend fills are easier than real ones' : 'real', cls(f.perMonth));
    let text;
    if (f.trades === 0) {
      const short = f.best ? ` The closest moment was ${f.best[0]}, which was ${Math.abs(f.best[1]).toFixed(0)} bps short of paying: trading $10,000 then would have lost about ${per10k(Math.abs(f.best[1]))} after fees.` : '';
      text = `No. In ${dur(f.uptimeS)} no price gap has beaten the fees, so projected earnings are $0.${short} To change this you need lower fees (a higher volume tier, or a cheaper exchange), not a faster bot.`;
      v.className = 'card verdict bad';
    } else if (f.pnl > 0) {
      text = `So far yes: ${usd(f.pnl)} in ${dur(f.uptimeS)}. At that pace about ${usd(f.perDay)} a day. ${f.mode === 'paper' ? 'This is pretend money; expect real results to be lower because real orders do not always fill.' : ''} Judge it over at least a week, not a day.`;
      v.className = 'card verdict good';
    } else {
      text = `Not at the moment: ${usd(f.pnl)} over ${dur(f.uptimeS)}. ${f.trades - f.successes} of ${f.trades} trades only half-completed and had to be undone, which is where the loss came from. The daily limit will stop it at ${usd(st.config.max_daily_loss_quote)}.`;
      v.className = 'card verdict bad';
    }
    $('#verdict-text').textContent = text;
  }

  function renderSteps(st) {
    const f = facts(st); const rec = st.recorder; const hasRec = (st.recordings || []).length > 0;
    const scanDone = !!state.scan;
    const steps = [
      { t: 'Start watching', d: 'Real prices, pretend money. Leave it on. Safe.', done: f.running, btn: f.running ? null : { id: 'st-watch', label: 'Start watching' } },
      { t: 'Record overnight', d: rec.active ? `Recording: ${rec.count.toLocaleString()} prices saved so far. Leave it until morning.` : 'Saves every price the bot sees to a file. Start it before bed.', done: hasRec && !rec.active, btn: rec.active ? { id: 'st-rec-stop', label: 'Stop recording' } : { id: 'st-rec', label: hasRec ? 'Record again' : 'Record overnight', disabled: !f.running } },
      { t: 'Rank the coins', d: 'Reads the recording and shows which coins came closest to paying at your fees.', done: scanDone, btn: { id: 'st-scan', label: state.scanning ? 'Ranking…' : 'Rank the coins', disabled: !hasRec || state.scanning } },
      { t: 'Read the verdict', d: scanDone ? 'See "Which coins came closest" below and the panel above.' : 'Appears after ranking. "No" for every coin is the normal result at retail fees.', done: scanDone },
      { t: 'Go live (only if yes)', d: 'Needs a week of positive pretend results first, money on both exchanges, API keys, and three confirmations.', done: f.mode === 'live', btn: { id: 'st-live', label: 'How to go live', link: 'https://github.com/cjsmitty/speedcraft/blob/main/docs/START_HERE.md' } },
    ];
    let current = steps.findIndex((s) => !s.done); if (current < 0) current = steps.length - 1;
    $('#step-list').innerHTML = steps.map((s, i) => `<li class="step ${s.done ? 'done' : ''} ${i === current ? 'current' : ''}"><span class="n">${s.done ? '✓' : i + 1}</span><span class="t">${s.t}</span><span class="d">${esc(s.d)}</span>${s.btn ? (s.btn.link ? `<a class="btn" href="${s.btn.link}" target="_blank" rel="noopener">${s.btn.label}</a>` : `<button class="btn ${i === current ? 'primary' : ''}" id="${s.btn.id}" ${s.btn.disabled ? 'disabled' : ''}>${s.btn.label}</button>`) : ''}</li>`).join('');
    const on = (id, fn) => { const el = document.getElementById(id); if (el) el.onclick = fn; };
    on('st-watch', () => $('#btn-paper').onclick());
    on('st-rec', () => act(() => post('/record', { on: true }), 'Recording started. Come back in the morning and press "Rank the coins".'));
    on('st-rec-stop', () => act(() => post('/record', { on: false }), 'Recording saved'));
    on('st-scan', runScan);
  }

  async function runScan() {
    state.scanning = true; renderSteps(state.status);
    try {
      const r = await post('/scan', {});
      state.scan = r; renderResearch(); toast('Ranking done');
      $('#research').scrollIntoView({ behavior: 'smooth', block: 'start' });
    } catch (e) { toast(e.message, true); }
    state.scanning = false; renderSteps(state.status);
  }

  function renderResearch() {
    const r = state.scan; const card = $('#research');
    if (!r) { card.hidden = true; return; }
    card.hidden = false;
    const hours = r.span_ms / 3600000;
    $('#research-sub').textContent = `${r.quotes.toLocaleString()} prices over ${hours < 1 ? Math.round(hours * 60) + ' minutes' : hours.toFixed(1) + ' hours'} · ${r.path.split('/').pop()}`;
    const rows = r.stats.map((p) => {
      const arb = p.arb_net_p95.yours ?? -999, mk = p.maker_net_p95.yours ?? -999, mkFrac = (p.maker_positive_frac.yours || 0) * 100;
      const bestMode = arb >= mk ? 'arb' : 'maker';
      const best = Math.max(arb, mk);
      const verdict = best >= r.min_edge_bps ? ['ok', 'Yes, on paper'] : best >= 0 ? ['warn', 'Barely'] : best > -20 ? ['warn', 'Close'] : ['no', 'No'];
      return { p, arb, mk, mkFrac, best, bestMode, verdict };
    }).sort((a, b) => b.best - a.best);
    const yes = rows.filter((x) => x.verdict[0] === 'ok').length, close = rows.filter((x) => x.verdict[1] !== 'No').length - yes;
    $('#research-lead').textContent = yes === 0
      ? `None of the ${rows.length} coins would have paid at your fees. ${close ? close + ' came close.' : 'The best was ' + rows[0].p.pair + ', about ' + per10k(Math.abs(rows[0].best)) + ' short on every $10,000.'} This is the expected result at retail fees; the gaps between exchanges are far smaller than the fees.`
      : `${yes} of ${rows.length} coins showed a gap that beat your fees in the best 5% of moments. "On paper" is the key phrase: leave the bot in paper mode on those coins for a week and watch the panel at the top before believing it.`;
    $('#research-table').innerHTML = `<tr><th>Coin</th><th class="num">Biggest gap seen</th><th class="num">Usual best gap</th><th class="num">After your fees, best case (per $10,000)</th><th>Best way to trade it</th><th>Verdict</th></tr>` +
      rows.map(({ p, best, bestMode, verdict, mkFrac }) => `<tr><td><b>${esc(p.pair.split('/')[0])}</b> <small>${esc(p.pair)}</small></td><td class="num">${fmt(p.gross_max_bps, 1)} bps</td><td class="num">${fmt(p.gross_p95_bps, 1)} bps</td><td class="num ${cls(best)}">${per10k(best)} <small>(${fmt(best, 1)} bps)</small></td><td>${best < 0 ? '<small>neither way pays</small>' : bestMode === 'arb' ? 'buy here, sell there' : `standing offer <small>${mkFrac.toFixed(0)}% of the time</small>`}</td><td><span class="tag ${verdict[0]}">${verdict[1]}</span></td></tr>`).join('');
  }

  function renderEdges(st) {
    const s = st.snapshot || {}; const edges = s.edges || []; const minEdge = N(s.min_edge_bps || 0); const t = $('#edges');
    if (!edges.length) { t.innerHTML = `<tr><td class="empty">${st.engine.state === 'running' ? 'Waiting for prices from at least two exchanges…' : 'Start watching to see live gaps.'}</td></tr>`; return; }
    // One row per coin: its best direction. (All directions are in the raw status.)
    const bestPer = {}; edges.forEach((e) => { if (!bestPer[e.pair] || N(e.net_bps) > N(bestPer[e.pair].net_bps)) bestPer[e.pair] = e; });
    const shown = Object.values(bestPer).sort((a, b) => N(b.net_bps) - N(a.net_bps));
    const maxAbs = Math.max(10, ...shown.map((e) => Math.abs(N(e.net_bps))));
    t.innerHTML = `<tr><th>Coin</th><th>Buy on</th><th>Sell on</th><th class="num">Gap before fees</th><th class="num">After fees</th><th>Per $10,000</th><th></th></tr>` +
      shown.map((e) => {
        const n = N(e.net_bps), w = Math.min(100, Math.abs(n) / maxAbs * 100);
        const tag = e.tradable ? '<span class="tag ok">WOULD TRADE</span>' : !e.fresh ? '<span class="tag dim">old price</span>' : n >= minEdge ? '<span class="tag warn">just under</span>' : `<span class="tag no">${fmt(minEdge - n, 0)} bps short</span>`;
        return `<tr><td><b>${esc(e.pair.split('/')[0])}</b></td><td>${exName(e.buy_exchange)}</td><td>${exName(e.sell_exchange)}</td><td class="num">${fmt(e.gross_bps, 1)} bps</td><td class="num ${cls(n)}">${fmt(n, 1)} bps</td><td class="${cls(n)}"><span class="bar ${e.tradable ? 'ok' : ''}" style="width:${Math.round(w * 1.1)}px"></span> ${per10k(n)}</td><td>${tag}</td></tr>`;
      }).join('');
  }

  function renderMaker(st) {
    const rows = (st.snapshot && st.snapshot.maker) || []; const card = $('#maker-card');
    if (!rows.length) { card.hidden = true; return; }
    card.hidden = false;
    const q = (x) => x ? `${fmtPx(x.price)} × ${fmt(x.qty, 4)} <small class="${cls(x.expected_edge_bps)}">(${per10k(x.expected_edge_bps)} per $10k if taken)</small>` : '<small>none</small>';
    $('#maker').innerHTML = `<tr><th>Coin</th><th>Offers on</th><th>Hedged on</th><th>Standing buy offer</th><th>Standing sell offer</th><th class="num">Holding</th><th class="num">Taken</th><th class="num">Made</th></tr>` +
      rows.map((r) => `<tr><td><b>${esc(r.pair.split('/')[0])}</b></td><td>${exName(r.quote_exchange)}</td><td>${exName(r.hedge_exchange)}</td><td>${q(r.bid)}</td><td>${q(r.ask)}</td><td class="num">${fmt(r.inventory_base, 4)}</td><td class="num">${r.fills}</td><td class="num ${cls(r.realised_pnl)}">${usd(r.realised_pnl, 4)}</td></tr>`).join('');
  }

  function renderBooks(st) {
    const books = (st.snapshot && st.snapshot.books) || []; const t = $('#books');
    if (!books.length) { t.innerHTML = `<tr><td class="empty">No prices yet.</td></tr>`; return; }
    t.innerHTML = `<tr><th>Coin</th><th>Exchange</th><th class="num">Buyers pay</th><th class="num">Sellers get</th><th class="num">Updated</th></tr>` +
      books.map((b) => `<tr><td><b>${esc(b.pair.split('/')[0])}</b></td><td>${exName(b.exchange)}</td><td class="num">${fmtPx(b.ask)}</td><td class="num">${fmtPx(b.bid)}</td><td class="num ${b.age_ms > 5000 ? 'neg' : ''}">${ago(b.age_ms)} ago</td></tr>`).join('');
  }

  function renderBalances(st) {
    const bal = (st.snapshot && st.snapshot.balances) || {}; const fees = (st.snapshot && st.snapshot.fees_taker_bps) || {}; const exs = Object.keys(bal).sort(); const t = $('#balances');
    if (!exs.length) { t.innerHTML = `<tr><td class="empty">Shown once the engine is running.</td></tr>`; return; }
    t.innerHTML = `<tr><th>Exchange</th><th>Asset</th><th class="num">Available</th><th class="num">Fee per trade</th></tr>` +
      exs.flatMap((ex) => Object.entries(bal[ex]).filter(([, v]) => N(v) !== 0).sort().map(([a, v], i) => `<tr><td>${i === 0 ? exName(ex) : ''}</td><td>${esc(a)}</td><td class="num">${fmt(v, 4)}</td><td class="num">${i === 0 && fees[ex] !== undefined ? (N(fees[ex]) / 100).toFixed(2) + '%' : ''}</td></tr>`)).join('');
  }

  function describe(e) {
    switch (e.event) {
      case 'trade': return `${e.pair.split('/')[0]}: bought on ${exName(e.buy_exchange)}, sold on ${exName(e.sell_exchange)}, ${e.matched_qty} coins. Result ${usd(e.realised_pnl_quote, 4)} (expected ${usd(e.expected_pnl_quote, 4)}). ${e.success ? 'Both halves completed.' : 'FAILED: ' + esc(e.note)}`;
      case 'opportunity': { const o = e.op; return `${o.pair.split('/')[0]}: gap beat fees. Buy ${exName(o.buy_exchange)} @ ${fmtPx(o.buy_price)}, sell ${exName(o.sell_exchange)} @ ${fmtPx(o.sell_price)}, ${o.qty} coins, about ${usd(o.expected_profit_quote, 4)} after fees.`; }
      case 'rejected': return `${e.op.pair.split('/')[0]}: opportunity skipped. ${esc(e.reason)}`;
      case 'leg': return `${e.unwind ? 'Undo: ' : ''}${e.side} ${e.requested_qty} ${e.pair.split('/')[0]} on ${exName(e.exchange)} at up to ${fmtPx(e.limit_price)} → ${e.result.status}, filled ${e.result.filled_qty} at ${fmtPx(e.result.avg_price)}, fee ${usd(e.result.fee_quote, 4)}`;
      case 'halt': return `Trading stopped: ${esc(e.reason)}`;
      case 'resume': return 'Trading resumed by you';
      case 'start': return `Engine started in ${e.mode} mode (v${e.version})`;
      case 'stop': return 'Engine stopped';
      default: return '';
    }
  }
  const label = { trade: 'TRADE', opportunity: 'CHANCE', rejected: 'SKIPPED', leg: 'ORDER', halt: 'STOPPED', resume: 'RESUMED', start: 'STARTED', stop: 'STOPPED', quote: 'OFFER' };
  function renderEvents() {
    const el = $('#events');
    if (!state.events.length) { el.innerHTML = `<div class="empty">Nothing yet. In pretend mode at normal fees this list usually stays empty: it means no trade was worth making, which is the bot doing its job.</div>`; return; }
    el.innerHTML = state.events.slice(0, 300).map((e) => `<div class="ev ${e.event}"><span class="t">${new Date(e.ts_ms).toLocaleTimeString()}</span><span class="k">${label[e.event] || e.event.toUpperCase()}</span><span>${describe(e)}</span></div>`).join('');
  }

  function recordEdgeHistory(st) {
    const s = st.snapshot || {}; const now = Date.now(); const best = {};
    (s.edges || []).forEach((e) => { const n = N(e.net_bps); if (best[e.pair] === undefined || n > best[e.pair]) best[e.pair] = n; });
    Object.entries(best).forEach(([p, v]) => { const h = state.edgeHistory[p] = state.edgeHistory[p] || []; h.push({ t: now, v }); if (h.length > 900) h.shift(); });
  }
  const COLORS = ['#7c6cff', '#22d3ee', '#f472b6', '#34d399', '#fbbf24', '#fb7185', '#a78bfa', '#60a5fa'];
  function drawChart(st) {
    const c = $('#edge-chart'); const dpr = window.devicePixelRatio || 1; const W = c.clientWidth, H = 150; c.width = W * dpr; c.height = H * dpr;
    const ctx = c.getContext('2d'); ctx.scale(dpr, dpr); ctx.clearRect(0, 0, W, H);
    const styles = getComputedStyle(document.documentElement); const muted = styles.getPropertyValue('--muted').trim(), line = styles.getPropertyValue('--line2').trim(), green = styles.getPropertyValue('--green').trim();
    const pairs = Object.keys(state.edgeHistory).slice(0, 8);
    const minEdge = N((st.snapshot && st.snapshot.min_edge_bps) || 0);
    ctx.font = '12px -apple-system, Segoe UI, sans-serif';
    if (!pairs.length) { ctx.fillStyle = muted; ctx.fillText('The chart fills in once two exchanges are sending prices. The green line is where a trade becomes worth it.', 12, 28); return; }
    const all = pairs.flatMap((p) => state.edgeHistory[p].map((x) => x.v)).concat([minEdge, 0]);
    let lo = Math.min(...all), hi = Math.max(...all); if (hi - lo < 10) { hi += 5; lo -= 5; } const padv = (hi - lo) * .08; lo -= padv; hi += padv;
    const pad = 44, y = (v) => H - 22 - (v - lo) / (hi - lo) * (H - 34), t0 = Math.min(...pairs.map((p) => state.edgeHistory[p][0].t)), t1 = Date.now();
    const x = (t) => pad + (t - t0) / Math.max(1, t1 - t0) * (W - pad - 10);
    ctx.strokeStyle = line; ctx.lineWidth = 1;
    [lo + padv, 0, hi - padv].forEach((v) => { ctx.beginPath(); ctx.moveTo(pad, y(v)); ctx.lineTo(W - 10, y(v)); ctx.stroke(); ctx.fillStyle = muted; ctx.fillText(`${v.toFixed(0)}`, 6, y(v) + 4); });
    ctx.strokeStyle = green; ctx.setLineDash([5, 4]); ctx.beginPath(); ctx.moveTo(pad, y(minEdge)); ctx.lineTo(W - 10, y(minEdge)); ctx.stroke(); ctx.setLineDash([]);
    ctx.fillStyle = green; ctx.fillText(`worth trading above ${minEdge} bps`, W - 190, y(minEdge) - 5);
    pairs.forEach((p, i) => {
      const h = state.edgeHistory[p]; ctx.strokeStyle = COLORS[i % COLORS.length]; ctx.lineWidth = 1.8; ctx.beginPath();
      h.forEach((pt, j) => { const X = x(pt.t), Y = y(pt.v); j ? ctx.lineTo(X, Y) : ctx.moveTo(X, Y); }); ctx.stroke();
      ctx.fillStyle = COLORS[i % COLORS.length]; ctx.fillText(`${p.split('/')[0]} ${h[h.length - 1].v.toFixed(0)}`, pad + 6 + i * 92, H - 6);
    });
  }

  function renderAgents() {
    const card = $('#agents-card'); if (!state.isHost) { card.hidden = true; return; } card.hidden = false;
    const t = $('#agents-table');
    if (!state.agents.length) { t.innerHTML = `<tr><td class="empty">None yet. The one-line installer in docs/REMOTE.md prints the address and token to paste here.</td></tr>`; return; }
    t.innerHTML = `<tr><th>Name</th><th>Address</th><th>Status</th><th></th></tr>` + state.agents.map((a) => {
      const h = state.agentHealth[a.id]; const tag = h === 'run' ? '<span class="tag ok">RUNNING</span>' : h === 'ok' ? '<span class="tag warn">IDLE</span>' : h === 'bad' ? '<span class="tag no">UNREACHABLE</span>' : '…';
      return `<tr><td>${esc(a.name)}</td><td>${esc(a.url)}${a.has_token ? '' : ' <span class="tag warn">no token</span>'}</td><td>${tag}</td><td><button class="btn" data-rm="${esc(a.id)}">Remove</button></td></tr>`;
    }).join('');
    t.querySelectorAll('[data-rm]').forEach((b) => b.onclick = async () => {
      if (!confirm(`Remove "${b.dataset.rm}" from this list? It keeps running on its own computer.`)) return;
      try { await api(`/agents/${encodeURIComponent(b.dataset.rm)}`, { method: 'DELETE' }, '/api'); if (state.agent === b.dataset.rm) switchAgent('local'); await loadAgents(); toast('Removed'); } catch (e) { toast(e.message, true); }
    });
  }

  function render() {
    const st = state.status; if (!st) return;
    renderTabs(); renderBanners(st); renderHero(st); renderVerdict(st); renderSteps(st); renderEdges(st); renderMaker(st); renderBooks(st); renderBalances(st); drawChart(st);
  }

  // ---------- polling ----------
  async function pollStatus() {
    try {
      const st = await api('/status'); state.status = st; recordEdgeHistory(st); render();
      $('#conn').className = 'conn ok'; $('#conn-text').textContent = state.agent === 'local' ? 'connected' : `connected to ${st.agent}`;
    } catch (e) { $('#conn').className = 'conn bad'; $('#conn-text').textContent = e.message; if (state.agent !== 'local') state.status = null; }
  }
  async function pollEvents() {
    try {
      const r = await api(`/events?since=${state.lastSeq}&limit=500`);
      if (r.events.length) { state.events = r.events.slice().reverse().concat(state.events).slice(0, 2000); state.lastSeq = r.latest_seq; renderEvents(); }
      else if (r.latest_seq < state.lastSeq) { state.lastSeq = 0; state.events = []; renderEvents(); }
    } catch { /* status poll reports connectivity */ }
  }
  async function pollAgentHealth() {
    if (!state.isHost) return;
    await Promise.all(state.agents.map(async (a) => { try { const st = await api('/status', {}, `/api/agents/${encodeURIComponent(a.id)}`); state.agentHealth[a.id] = st.engine.state === 'running' ? 'run' : 'ok'; } catch { state.agentHealth[a.id] = 'bad'; } }));
    renderTabs(); renderAgents();
  }
  async function loadAgents() {
    try { const r = await api('/agents', {}, '/api'); state.agents = r.agents; state.isHost = true; } catch (e) { if (/headless/.test(e.message)) state.isHost = false; }
    renderTabs(); renderAgents(); pollAgentHealth();
  }
  async function loadConfig() {
    try { const r = await api('/config'); $('#config-text').value = r.toml; $('#config-path').textContent = r.path; state.configDirty = false; $('#config-msg').textContent = ''; } catch (e) { $('#config-msg').textContent = e.message; }
  }
  function switchAgent(id) {
    state.agent = id; state.lastSeq = 0; state.events = []; state.edgeHistory = {}; state.status = null; state.scan = null;
    renderEvents(); renderResearch(); renderTabs(); pollStatus(); pollEvents(); loadConfig();
  }

  // ---------- actions ----------
  const act = async (fn, okMsg) => { try { await fn(); if (okMsg) toast(okMsg); await pollStatus(); } catch (e) { toast(e.message, true); } };
  $('#btn-paper').onclick = () => act(() => post('/engine/start', { mode: 'paper' }), 'Watching. Pretend money, real prices.');
  $('#btn-stop').onclick = () => act(() => post('/engine/stop'), 'Stopping…');
  $('#btn-halt').onclick = () => act(() => post('/engine/halt', { reason: 'emergency stop pressed' }), 'Orders stopped. Still watching.');
  $('#btn-resume').onclick = () => act(() => post('/engine/resume'), 'Resumed');
  window.sc = { resume: () => $('#btn-resume').onclick() };
  $('#chk-stopfile').onchange = (ev) => act(() => post('/stopfile', { present: ev.target.checked }), ev.target.checked ? 'Paused' : 'Unpaused');
  $('#btn-live').onclick = () => {
    const st = state.status; if (!st) return;
    $('#live-agent-name').textContent = st.agent;
    const gates = [[st.config.live_in_config, 'live = true in the settings file'], [st.config.live_env_set, 'SPEEDCRAFT_I_ACCEPT_LOSSES=yes set where the bot runs (and exchange API keys)'], [true, 'The phrase typed below']];
    $('#live-gates').innerHTML = gates.map(([ok, t]) => `<li class="${ok ? 'ok' : 'no'}">${ok ? '✓' : '✗'} ${t}</li>`).join('');
    $('#live-phrase').value = ''; $('#live-go').disabled = true; $('#modal-live').hidden = false; $('#live-phrase').focus();
  };
  $('#live-phrase').oninput = (e) => { $('#live-go').disabled = e.target.value.trim() !== 'I ACCEPT LOSSES'; };
  $('#live-cancel').onclick = () => { $('#modal-live').hidden = true; };
  $('#live-go').onclick = () => { $('#modal-live').hidden = true; act(() => post('/engine/start', { mode: 'live', confirm: $('#live-phrase').value }), 'LIVE trading starting'); };
  $('#btn-config-reload').onclick = loadConfig;
  $('#config-text').oninput = () => { state.configDirty = true; };
  $('#btn-config-save').onclick = () => act(async () => { const r = await api('/config', { method: 'PUT', body: JSON.stringify({ toml: $('#config-text').value }) }); $('#config-msg').textContent = r.restart_required ? 'Saved. Stop and start the engine to apply.' : 'Saved.'; state.configDirty = false; }, 'Settings saved');
  $('#agent-form').onsubmit = (ev) => { ev.preventDefault(); act(async () => { await api('/agents', { method: 'POST', body: JSON.stringify({ name: $('#agent-name').value, url: $('#agent-url').value, token: $('#agent-token').value }) }, '/api'); $('#agent-form').reset(); await loadAgents(); }, 'Computer added'); };
  $('#token-save').onclick = () => { state.token = $('#token-input').value.trim(); localStorage.setItem('speedcraft_token', state.token); $('#modal-token').hidden = true; pollStatus(); loadAgents(); loadConfig(); };
  $('#btn-help').onclick = () => { $('#modal-help').hidden = false; };
  $('#help-close').onclick = () => { $('#modal-help').hidden = true; };
  document.querySelectorAll('.modal').forEach((m) => m.addEventListener('click', (e) => { if (e.target === m) m.hidden = true; }));
  const applyTheme = (t) => { if (t) document.documentElement.setAttribute('data-theme', t); else document.documentElement.removeAttribute('data-theme'); };
  try { applyTheme(localStorage.getItem('speedcraft_theme') || (window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : '')); } catch { /* ignore */ }
  $('#btn-theme').onclick = () => { const cur = document.documentElement.getAttribute('data-theme'); const next = cur === 'light' ? '' : 'light'; applyTheme(next); try { localStorage.setItem('speedcraft_theme', next); } catch { /* ignore */ } if (state.status) drawChart(state.status); };
  window.addEventListener('beforeunload', (e) => { if (state.configDirty) { e.preventDefault(); e.returnValue = ''; } });
  window.addEventListener('resize', () => state.status && drawChart(state.status));

  // ---------- boot ----------
  loadAgents(); loadConfig(); pollStatus(); pollEvents(); renderEvents();
  setInterval(pollStatus, 1000); setInterval(pollEvents, 2000); setInterval(pollAgentHealth, 5000);
})();
