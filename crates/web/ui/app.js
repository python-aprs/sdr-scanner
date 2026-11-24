// Scanner web UI. Plain JavaScript, no build step: it talks only to /api/v1
// and the /api/v1/live WebSocket. Text from the server is always set with
// textContent, never parsed as HTML.
'use strict';

(() => {
  // ---------- small helpers ----------
  const $ = (sel, el = document) => el.querySelector(sel);

  /** Build an element: h('td', {class: 'x', onclick: fn}, 'text', child). */
  function h(tag, attrs, ...kids) {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs || {})) {
      if (v === null || v === undefined || v === false) continue;
      if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
      else if (k === 'class') el.className = v;
      // CSSOM, not the style attribute: the CSP blocks inline style attributes.
      else if (k === 'style') el.style.cssText = v;
      else if (k === 'dataset') Object.assign(el.dataset, v);
      else if (v === true) el.setAttribute(k, '');
      else el.setAttribute(k, v);
    }
    for (const kid of kids.flat()) {
      if (kid === null || kid === undefined || kid === false) continue;
      el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
    }
    return el;
  }

  // replaceChildren() turns null into the text "null"; skip empty children
  // the way h() does.
  const fill = (el, ...kids) => el.replaceChildren(...kids.flat().filter((k) => k !== null && k !== undefined && k !== false));

  const MHZ = (hz, digits = 4) => (hz / 1e6).toFixed(digits);
  const fmtSecs = (s) => {
    if (s === null || s === undefined) return '';
    if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)} s`;
    const m = Math.floor(s / 60);
    return `${m}:${String(Math.floor(s % 60)).padStart(2, '0')}`;
  };
  const fmtUptime = (s) => {
    const d = Math.floor(s / 86400), hh = Math.floor((s % 86400) / 3600), mm = Math.floor((s % 3600) / 60);
    if (d) return `${d}d ${hh}h`;
    if (hh) return `${hh}h ${mm}m`;
    return `${mm}m ${Math.floor(s % 60)}s`;
  };
  const fmtCount = (n) => new Intl.NumberFormat().format(n);
  const fmtBytes = (n) => {
    if (n === null || n === undefined) return '–';
    const units = ['B', 'kB', 'MB', 'GB', 'TB'];
    let i = 0;
    while (n >= 1000 && i < units.length - 1) { n /= 1000; i++; }
    return `${n >= 100 || i === 0 ? Math.round(n) : n.toFixed(1)} ${units[i]}`;
  };
  const fmtDb = (v) => `${Math.round(v) || 0} dB`;
  const timeFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  const dayFmt = new Intl.DateTimeFormat(undefined, { weekday: 'short', year: 'numeric', month: 'short', day: 'numeric' });
  const dateTimeFmt = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });
  const nsDate = (ns) => new Date(ns / 1e6);
  function dayLabel(d) {
    const today = new Date();
    const y = new Date(today); y.setDate(today.getDate() - 1);
    if (d.toDateString() === today.toDateString()) return 'Today';
    if (d.toDateString() === y.toDateString()) return 'Yesterday';
    return dayFmt.format(d);
  }

  class ApiError extends Error {
    constructor(status, message) { super(message); this.status = status; }
  }

  // ---------- state ----------
  const state = {
    session: null,       // /api/v1/auth/session
    channels: [],        // /api/v1/channels
    status: null,        // latest status from the live socket
    view: null,
  };

  async function api(path, opts = {}) {
    const headers = { Accept: 'application/json' };
    const init = { method: opts.method || 'GET', headers, credentials: 'same-origin' };
    if (opts.body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(opts.body);
    }
    if (init.method !== 'GET' && state.session && state.session.csrf_token) {
      headers['X-CSRF-Token'] = state.session.csrf_token;
    }
    const r = await fetch(`/api/v1${path}`, init);
    if (r.status === 204) return null;
    let data = null;
    try { data = await r.json(); } catch (_) { /* empty body */ }
    if (!r.ok) throw new ApiError(r.status, (data && data.error) || `${r.status} ${r.statusText}`);
    return data;
  }

  const isAdmin = () => state.session && state.session.role === 'admin';
  const pub = () => (state.session && state.session.public) || { live: true, recordings: false, audio: false };
  const canRecordings = () => isAdmin() || pub().recordings;
  // Where visitors stand against the scanner's monthly data budget.
  const budgetSpent = () => !isAdmin() && state.budget === 'spent';
  const canAudio = () => isAdmin() || (pub().audio && !budgetSpent());
  const budgetBanner = h('div', { class: 'budget-banner', role: 'status', hidden: true });
  function setBudget(level) {
    const was = state.budget;
    state.budget = isAdmin() ? 'ok' : (level || 'ok');
    const resets = state.session && state.session.budget_resets_ns;
    const back = resets ? ` on ${dateTimeFmt.format(nsDate(resets))}` : ' next month';
    budgetBanner.hidden = state.budget === 'ok';
    budgetBanner.textContent = state.budget === 'spent'
      ? `This scanner has used its data allowance for visitors this month. Live audio, recordings and the spectrum are back${back}.`
      : 'This scanner is close to its monthly data allowance for visitors, so the spectrum updates once a second.';
    if (was !== undefined && was !== state.budget && state.view) route();
  }

  async function loadSession() {
    try { state.session = await api('/auth/session'); } catch (_) { state.session = null; }
    setBudget(state.session && state.session.budget);
    renderWho();
  }

  async function loadChannels() {
    try { state.channels = await api('/channels'); } catch (_) { state.channels = []; }
  }

  /** Lock out / prioritise a frequency (admin), then refresh the channel list. */
  async function editChannel(freqHz, body) {
    await api(`/channels/${Math.round(freqHz)}`, { method: 'PATCH', body });
    await loadChannels();
  }
  async function resetChannel(freqHz) {
    await api(`/channels/${Math.round(freqHz)}/override`, { method: 'DELETE' });
    await loadChannels();
  }

  /** A button that asks once more before doing something. */
  function confirmButton(label, really, action, cls = '') {
    const b = h('button', { type: 'button', class: cls });
    b.textContent = label;
    b.addEventListener('click', async (e) => {
      e.stopPropagation();
      if (b.dataset.confirm !== '1') {
        b.dataset.confirm = '1'; b.textContent = really;
        setTimeout(() => { if (b.isConnected) { b.dataset.confirm = ''; b.textContent = label; } }, 4000);
        return;
      }
      b.disabled = true;
      try { await action(); } catch (ex) { b.disabled = false; b.textContent = ex.message; }
    });
    return b;
  }

  /** The named channel a frequency belongs to, if any (within half a bandwidth). */
  function channelAt(hz) {
    let best = null, bestD = Infinity;
    for (const c of state.channels) {
      const d = Math.abs(c.freq_hz - hz);
      if (d <= Math.max(c.bandwidth_hz / 2, 3000) && d < bestD) { best = c; bestD = d; }
    }
    return best;
  }
  const kindLabel = {
    channel: 'Channel', repeater_output: 'Repeater output', repeater_input: 'Repeater input', grid: 'Grid', lockout: 'Lockout',
  };
  function channelName(hz) {
    const c = channelAt(hz);
    if (!c) return '';
    if (c.name) return c.kind === 'repeater_input' ? `${c.name} (input)` : c.name;
    return '';
  }

  function renderWho() {
    const who = $('#who');
    if (isAdmin()) {
      who.textContent = state.session.username;
      who.title = 'Signed in';
    } else {
      who.textContent = 'Log in';
      who.title = '';
    }
  }

  // ---------- live socket (one per tab; spectrum only while Live is shown) ----------
  const live = {
    ws: null,
    retry: 1000,
    rate: 2,
    listeners: new Set(),   // fn(kind, payload)
    connect() {
      if (this.ws) return;
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      setLink('busy', 'Connecting');
      const ws = new WebSocket(`${proto}//${location.host}/api/v1/live`);
      ws.binaryType = 'arraybuffer';
      this.ws = ws;
      ws.onopen = () => { this.retry = 1000; setLink('up', 'Live'); this.sendConfig(); };
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          let m; try { m = JSON.parse(ev.data); } catch (_) { return; }
          if (m.type === 'status') { state.status = m; onStatus(m); if (m.budget && m.budget !== state.budget) setBudget(m.budget); }
          this.emit(m.type, m);
        } else {
          this.emit('spectrum', decodeSpectrum(ev.data));
        }
      };
      ws.onclose = () => {
        this.ws = null;
        // The upgrade is refused (401) when live status is private: check
        // over HTTP so we can say so instead of retrying forever.
        api('/status').then(() => {
          setLink('down', 'Reconnecting');
          setTimeout(() => this.connect(), this.retry);
          this.retry = Math.min(this.retry * 2, 30000);
        }).catch((e) => {
          if (e.status === 401 || e.status === 403) { setLink('down', 'Private'); this.emit('private', e); }
          else { setLink('down', 'Offline'); setTimeout(() => this.connect(), this.retry); this.retry = Math.min(this.retry * 2, 30000); }
        });
      };
    },
    reconnect() { if (this.ws) { this.ws.onclose = null; this.ws.close(); this.ws = null; } this.connect(); },
    wantsSpectrum() { return state.view === 'live' && document.visibilityState === 'visible'; },
    sendConfig() {
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: 'config', rate: this.rate, paused: !this.wantsSpectrum() }));
      }
    },
    emit(kind, payload) { for (const fn of this.listeners) fn(kind, payload); },
  };
  document.addEventListener('visibilitychange', () => live.sendConfig());

  function setLink(cls, text) {
    const el = $('#link-state');
    el.className = `link-state ${cls}`;
    el.lastElementChild.textContent = text;
  }

  function decodeSpectrum(buf) {
    const dv = new DataView(buf);
    const start = dv.getFloat64(4, true), stop = dv.getFloat64(12, true);
    const lo = dv.getFloat32(20, true), step = dv.getFloat32(24, true);
    const raw = new Uint8Array(buf, 28);
    const db = new Float32Array(raw.length);
    for (let i = 0; i < raw.length; i++) db[i] = lo + raw[i] * step;
    return { start, stop, db };
  }

  function onStatus(st) {
    const n = st.active ? st.active.length : 0;
    if (live.ws && live.ws.readyState === WebSocket.OPEN) setLink('up', n ? `${n} active` : 'Live');
    $('#brand-sub').textContent = st.profile || '';
  }

  // ---------- live listening ----------
  // Opus over /api/v1/audio, decoded with WebCodecs and scheduled on a Web
  // Audio clock with a short jitter buffer. See crates/web/src/audio.rs.
  const listen = {
    supported: typeof AudioDecoder !== 'undefined' && typeof AudioContext !== 'undefined',
    mode: 'off',            // off | follow | all | freqs
    freqs: new Set(),       // Hz, for mode "freqs"
    ws: null,
    ctx: null,
    master: null,
    volume: 0.8,
    streams: new Map(),     // id -> {freq, decoder, next}
    retry: 1000,
    listeners: new Set(),   // fn() on any change

    set(mode) {
      this.mode = mode;
      if (mode !== 'off') {
        if (!this.ctx) {
          this.ctx = new AudioContext();
          this.master = this.ctx.createGain();
          this.master.gain.value = this.volume;
          this.master.connect(this.ctx.destination);
        }
        this.ctx.resume();
        this.connect();
      }
      this.send();
      this.changed();
    },
    toggleFreq(hz) {
      if (this.freqs.has(hz)) this.freqs.delete(hz); else this.freqs.add(hz);
      this.set(this.freqs.size ? 'freqs' : 'off');
    },
    setVolume(v) { this.volume = v; if (this.master) this.master.gain.value = v; },
    send() {
      if (this.ws && this.ws.readyState === WebSocket.OPEN) {
        this.ws.send(JSON.stringify({ type: 'listen', mode: this.mode, freqs: [...this.freqs] }));
      }
    },
    connect() {
      if (this.ws) return;
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
      const ws = new WebSocket(`${proto}//${location.host}/api/v1/audio`);
      ws.binaryType = 'arraybuffer';
      this.ws = ws;
      ws.onopen = () => { this.retry = 1000; this.send(); };
      ws.onmessage = (ev) => {
        if (typeof ev.data === 'string') {
          let m; try { m = JSON.parse(ev.data); } catch (_) { return; }
          if (m.type === 'start') this.start(m);
          else if (m.type === 'end') this.end(m.id);
          else if (m.type === 'budget') { this.set('off'); setBudget(m.level); }
        } else {
          const dv = new DataView(ev.data);
          const st = this.streams.get(dv.getUint32(4, true));
          if (!st || st.decoder.state !== 'configured') return;
          const seq = dv.getUint32(8, true);
          st.decoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: seq * 40000, data: new Uint8Array(ev.data, 12) }));
        }
      };
      ws.onclose = () => {
        this.ws = null;
        for (const id of [...this.streams.keys()]) this.end(id);
        if (this.mode !== 'off') {
          setTimeout(() => { if (this.mode !== 'off') this.connect(); }, this.retry);
          this.retry = Math.min(this.retry * 2, 30000);
        }
      };
    },
    start(m) {
      const st = { freq: m.freq_hz, next: 0, decoder: null };
      st.decoder = new AudioDecoder({
        output: (data) => this.play(st, data),
        error: () => {},
      });
      st.decoder.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 1 });
      this.streams.set(m.id, st);
      this.changed();
    },
    end(id) {
      const st = this.streams.get(id);
      if (!st) return;
      this.streams.delete(id);
      st.decoder.flush().catch(() => {}).finally(() => { try { st.decoder.close(); } catch (_) { /* closed */ } });
      this.changed();
    },
    play(st, data) {
      const n = data.numberOfFrames;
      const buf = this.ctx.createBuffer(1, n, data.sampleRate);
      data.copyTo(buf.getChannelData(0), { planeIndex: 0, format: 'f32-planar' });
      data.close();
      const now = this.ctx.currentTime;
      // Late (start, or a network hiccup): restart a quarter second ahead.
      if (st.next < now + 0.02) st.next = now + 0.25;
      const src = this.ctx.createBufferSource();
      src.buffer = buf;
      src.connect(this.master);
      src.start(st.next);
      st.next += buf.duration;
    },
    playing() { return [...this.streams.values()].map((s) => s.freq); },
    changed() { for (const fn of this.listeners) fn(); },
  };

  function listenControls() {
    const wrap = h('div', { class: 'controls listen' });
    function render() {
      if (budgetSpent() && pub().audio) { wrap.replaceChildren(h('span', { class: 'muted', title: 'The data allowance for visitors is used up this month' }, 'Listening paused')); return; }
      if (!canAudio()) { wrap.replaceChildren(h('span', { class: 'muted', title: 'Live audio is private on this scanner' }, 'Listening: log in')); return; }
      if (!listen.supported) { wrap.replaceChildren(h('span', { class: 'muted' }, 'This browser can’t play live audio')); return; }
      const titles = { off: 'Not listening', follow: 'One transmission at a time, priority channels first', all: 'Everything on the air, mixed' };
      const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'Listen' },
        [['off', 'Off'], ['follow', 'Follow'], ['all', 'All']].map(([m, t]) =>
          h('button', { type: 'button', 'aria-pressed': String(listen.mode === m), title: titles[m],
            onclick: () => { listen.freqs.clear(); listen.set(m); } }, t)));
      const vol = h('input', { type: 'range', min: '0', max: '1', step: '0.05', value: String(listen.volume), 'aria-label': 'Volume', class: 'vol' });
      vol.oninput = () => listen.setVolume(Number(vol.value));
      const now = listen.playing();
      fill(wrap, h('span', { class: 'k', 'aria-hidden': 'true' }, '\u{1F50A}'), seg, vol,
        listen.mode === 'freqs' ? h('span', { class: 'muted' }, `${listen.freqs.size} selected`) : null,
        now.length ? h('span', { class: 'now-playing mono' }, now.map((f) => MHZ(f, 4)).join(', ')) : null);
    }
    render();
    listen.listeners.add(render);
    wrap.dispose = () => listen.listeners.delete(render);
    return wrap;
  }

  // ---------- player ----------
  // Play the small Opus copy where the browser can (all current ones,
  // Safari since 17), else the WAV.
  const playsOpus = (() => {
    try { return document.createElement('audio').canPlayType('audio/ogg; codecs=opus') !== ''; } catch (_) { return false; }
  })();
  const playUrl = (r) => (r.opus_path && playsOpus ? r.audio_url : r.wav_url);
  const player = {
    list: [], index: -1,
    play(list, index) {
      this.list = list; this.index = index;
      const r = list[index];
      if (!r) return;
      const audio = $('#audio');
      audio.src = playUrl(r);
      audio.play().catch(() => {});
      $('#player').hidden = false;
      document.body.classList.add('has-player');
      $('#player-freq').textContent = `${MHZ(r.freq_hz)} MHz`;
      const when = nsDate(r.start_ns);
      $('#player-meta').textContent = [r.channel || channelName(r.freq_hz), `${dayLabel(when)} ${timeFmt.format(when)}`, fmtSecs(r.audio_secs), r.tone && `Tone ${r.tone}`, r.dtmf && `DTMF ${r.dtmf}`]
        .filter(Boolean).join(' · ');
      document.querySelectorAll('tr.playing, li.playing').forEach((el) => el.classList.remove('playing'));
      document.querySelectorAll(`[data-rec="${r.id}"]`).forEach((el) => el.classList.add('playing'));
    },
    step(d) {
      // The list is newest first, so "next" (newer) is index - 1.
      const i = this.index - d;
      if (i >= 0 && i < this.list.length) this.play(this.list, i);
    },
    close() {
      const audio = $('#audio');
      audio.pause(); audio.removeAttribute('src'); audio.load();
      $('#player').hidden = true;
      document.body.classList.remove('has-player');
    },
  };
  $('#player-prev').onclick = () => player.step(-1);
  $('#player-next').onclick = () => player.step(1);
  $('#player-close').onclick = () => player.close();
  $('#audio').addEventListener('ended', () => { if ($('#player-auto').checked) player.step(1); });

  function playButton(list, index) {
    return h('button', { type: 'button', class: 'icon-btn', title: 'Play', 'aria-label': 'Play', onclick: (e) => { e.stopPropagation(); player.play(list, index); } }, '▶');
  }

  // ---------- views ----------
  let cleanup = null;
  const view = () => $('#view');

  function route() {
    const hash = location.hash.replace(/^#\/?/, '') || 'live';
    const [name, query] = hash.split('?');
    const params = new URLSearchParams(query || '');
    const views = { live: viewLive, recordings: viewRecordings, packets: viewPackets, channels: viewChannels, admin: viewAdmin };
    const fn = views[name] || viewLive;
    state.view = views[name] ? name : 'live';
    document.querySelectorAll('.tabs a').forEach((a) => {
      if (a.dataset.view === state.view) a.setAttribute('aria-current', 'page');
      else a.removeAttribute('aria-current');
    });
    if (cleanup) { cleanup(); cleanup = null; }
    view().replaceChildren();
    cleanup = fn(view(), params) || null;
    live.sendConfig();
  }

  function notice(title, text, action) {
    return h('div', { class: 'notice' }, h('strong', {}, title), h('p', {}, text), action || null);
  }

  // ----- Live -----
  // Kept across page changes so the waterfall's history survives a visit to
  // another tab.
  const waterfallCanvas = h('canvas', { id: 'waterfall', 'aria-label': 'Waterfall' });

  function viewLive(root) {
    const stats = h('div', { class: 'stats' });
    const statEls = {};
    for (const [key, label, wide] of [['source', 'Source', true], ['center', 'Centre'], ['uptime', 'Uptime'], ['load', 'DSP load'], ['dropped', 'Dropped samples'], ['recorded', 'Recorded']]) {
      const v = h('div', { class: 'v' }, '–');
      statEls[key] = v;
      stats.append(h('div', { class: `stat${wide ? ' wide' : ''}` }, h('div', { class: 'k' }, label), v));
    }

    const rateSel = h('select', { 'aria-label': 'Spectrum frames per second' },
      [1, 2, 5].map((r) => h('option', { value: r, selected: r === live.rate }, `${r} fps`)));
    rateSel.onchange = () => { live.rate = Number(rateSel.value); live.sendConfig(); };
    const rangeSel = h('select', { 'aria-label': 'Display range' },
      [40, 60, 80].map((r) => h('option', { value: r, selected: r === 60 }, `${r} dB`)));
    const spec = h('canvas', { id: 'spectrum', 'aria-label': 'Spectrum' });
    const fall = waterfallCanvas;
    const tip = h('div', { class: 'scope-tip', hidden: true });
    const empty = h('div', { class: 'scope-empty' }, budgetSpent() ? 'Spectrum paused until the data allowance resets' : 'Waiting for the first spectrum…');
    const toasts = h('div', { class: 'toasts', 'aria-live': 'polite' });
    const scope = h('div', { class: 'scope' }, spec, fall, tip, empty, toasts);

    const activeList = h('ul', { class: 'active-list' });
    const listenCtl = listenControls();
    const recentBody = h('div');
    const recentPanel = h('section', { class: 'panel' },
      h('div', { class: 'panel-head' }, h('h2', {}, 'Latest recordings'), h('span', { class: 'spacer' }), h('a', { href: '#/recordings' }, 'All recordings')),
      recentBody);

    root.append(
      stats,
      h('div', { class: 'grid live-grid' },
        h('section', { class: 'panel' },
          h('div', { class: 'panel-head' },
            h('h2', {}, 'Spectrum'), h('span', { class: 'muted', id: 'span-label' }), h('span', { class: 'spacer' }),
            h('div', { class: 'controls' }, h('label', {}, 'Range', rangeSel), h('label', {}, 'Rate', rateSel))),
          scope),
        h('div', { class: 'grid' },
          h('section', { class: 'panel' }, h('div', { class: 'panel-head' }, h('h2', {}, 'On the air now'), h('span', { class: 'spacer' }), listenCtl), activeList),
          recentPanel)));

    // --- drawing ---
    const WF_ROWS = 200;
    const wfCtx = fall.getContext('2d', { willReadFrequently: false });
    const specCtx = spec.getContext('2d');
    let frame = null;           // last spectrum
    let floor = null;           // smoothed noise floor estimate
    let hover = null;           // {x, y} in CSS px
    let wfWidth = fall.width;
    const lut = colorLut();

    function css(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }

    function sizeCanvas(c) {
      const r = c.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      const w = Math.max(1, Math.round(r.width * dpr)), hh = Math.max(1, Math.round(r.height * dpr));
      if (c.width !== w || c.height !== hh) { c.width = w; c.height = hh; }
      return { w, h: hh, dpr };
    }

    function range() {
      const span = Number(rangeSel.value);
      // The noise floor sits a little above the bottom edge.
      const lo = (floor === null ? -100 : floor) - span * 0.15;
      return { lo, hi: lo + span };
    }

    function freqToX(hz, w) { return ((hz - frame.start) / (frame.stop - frame.start)) * w; }

    function drawSpectrum() {
      const { w, h: H, dpr } = sizeCanvas(spec);
      const ctx = specCtx;
      ctx.clearRect(0, 0, w, H);
      if (!frame) return;
      const { lo, hi } = range();
      const yOf = (db) => H - ((db - lo) / (hi - lo)) * (H - 18 * dpr) - 18 * dpr;
      // Grid: every 10 dB, and a frequency step that gives ~6-10 labels.
      ctx.strokeStyle = css('--grid'); ctx.lineWidth = 1; ctx.fillStyle = css('--muted');
      ctx.font = `${11 * dpr}px ${css('--mono')}`;
      for (let db = Math.ceil(lo / 10) * 10; db <= hi; db += 10) {
        const y = Math.round(yOf(db)) + 0.5;
        ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
        ctx.fillText(`${db}`, 4 * dpr, y - 3 * dpr);
      }
      const spanHz = frame.stop - frame.start;
      const steps = [10e3, 25e3, 50e3, 100e3, 250e3, 500e3, 1e6, 2e6];
      const step = steps.find((s) => spanHz / s <= (w / dpr) / 90) || 5e6;
      ctx.textAlign = 'center';
      for (let f = Math.ceil(frame.start / step) * step; f <= frame.stop; f += step) {
        const x = Math.round(freqToX(f, w)) + 0.5;
        ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H - 16 * dpr); ctx.stroke();
        const label = MHZ(f, step < 100e3 ? 3 : step < 1e6 ? 2 : 1);
        const half = ctx.measureText(label).width / 2 + 2 * dpr;
        ctx.fillText(label, Math.min(Math.max(x, half), w - half), H - 4 * dpr);
      }
      ctx.textAlign = 'left';
      // Active transmissions.
      const active = ((state.status && state.status.active) || []).slice();
      ctx.font = `${11 * dpr}px ${css('--sans')}`;
      let labelEnd = -Infinity;
      active.sort((p, q) => p.freq_hz - q.freq_hz);
      for (const a of active) {
        const c = channelAt(a.freq_hz);
        const bw = c ? c.bandwidth_hz : 16e3;
        const x0 = freqToX(a.freq_hz - bw / 2, w), x1 = freqToX(a.freq_hz + bw / 2, w);
        ctx.fillStyle = css('--active');
        ctx.fillRect(x0, 0, Math.max(2 * dpr, x1 - x0), H - 16 * dpr);
        ctx.fillStyle = css('--accent');
        ctx.fillRect(x0, 0, Math.max(2 * dpr, x1 - x0), 2 * dpr);
        // Label only where it fits; the list beside the scope names them all.
        const full = `${MHZ(a.freq_hz, 3)}${c && c.name ? ' ' + c.name : ''}`;
        for (const label of [full, MHZ(a.freq_hz, 3)]) {
          const tw = ctx.measureText(label).width;
          const lx = Math.min(Math.max(x0, 2), w - tw - 4);
          if (lx > labelEnd + 8 * dpr) { ctx.fillText(label, lx, 14 * dpr); labelEnd = lx + tw; break; }
        }
      }
      // The trace.
      const n = frame.db.length;
      ctx.beginPath();
      for (let i = 0; i < n; i++) {
        const x = (i + 0.5) / n * w, y = Math.min(H - 16 * dpr, yOf(frame.db[i]));
        if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
      }
      ctx.strokeStyle = css('--spec-line'); ctx.lineWidth = 1.2 * dpr; ctx.stroke();
      ctx.lineTo(w, H - 16 * dpr); ctx.lineTo(0, H - 16 * dpr); ctx.closePath();
      ctx.fillStyle = css('--spec-fill'); ctx.fill();
      // Hover crosshair.
      if (hover) {
        const x = hover.x * dpr;
        ctx.strokeStyle = css('--muted'); ctx.lineWidth = 1; ctx.setLineDash([3 * dpr, 3 * dpr]);
        ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, H - 16 * dpr); ctx.stroke(); ctx.setLineDash([]);
      }
    }

    function pushWaterfall() {
      const n = frame.db.length;
      if (fall.width !== n || fall.height !== WF_ROWS) {
        fall.width = n; fall.height = WF_ROWS; wfWidth = n;
        wfCtx.fillStyle = `rgb(${lut[0]},${lut[1]},${lut[2]})`; wfCtx.fillRect(0, 0, n, WF_ROWS);
      }
      wfCtx.drawImage(fall, 0, 0, wfWidth, WF_ROWS - 1, 0, 1, wfWidth, WF_ROWS - 1);
      const row = wfCtx.createImageData(n, 1);
      const { lo, hi } = range();
      for (let i = 0; i < n; i++) {
        const v = Math.max(0, Math.min(255, Math.round(((frame.db[i] - lo) / (hi - lo)) * 255)));
        row.data[i * 4] = lut[v * 3]; row.data[i * 4 + 1] = lut[v * 3 + 1]; row.data[i * 4 + 2] = lut[v * 3 + 2]; row.data[i * 4 + 3] = 255;
      }
      wfCtx.putImageData(row, 0, 0);
    }

    function onSpectrum(f) {
      frame = f;
      empty.hidden = true;
      // Noise floor: the 20th percentile of the frame, smoothed.
      const sorted = Float32Array.from(f.db).sort();
      const p = sorted[Math.floor(sorted.length * 0.2)];
      floor = floor === null ? p : floor + 0.1 * (p - floor);
      $('#span-label').textContent = `${MHZ(f.start, 3)}–${MHZ(f.stop, 3)} MHz`;
      pushWaterfall();
      drawSpectrum();
      if (hover) showTip();
    }

    function showTip() {
      if (!frame || !hover) { tip.hidden = true; return; }
      const r = spec.getBoundingClientRect();
      const hz = frame.start + (hover.x / r.width) * (frame.stop - frame.start);
      const i = Math.min(frame.db.length - 1, Math.max(0, Math.floor((hover.x / r.width) * frame.db.length)));
      const c = channelAt(hz);
      fill(tip,
        h('span', { class: 'mono' }, `${MHZ(hz, 4)} MHz`), ` · ${fmtDb(frame.db[i])}`,
        c && c.name ? h('div', { class: 'muted' }, `${c.name} (${kindLabel[c.kind] || c.kind})`) : null,
        canRecordings() ? h('div', { class: 'muted' }, 'Click for recordings') : null);
      tip.hidden = false;
      const tw = tip.offsetWidth;
      tip.style.left = `${Math.min(Math.max(0, hover.x + 12), r.width - tw - 4)}px`;
      tip.style.top = `${Math.max(0, hover.y - 10 + (hover.target === fall ? spec.offsetHeight : 0))}px`;
    }

    for (const c of [spec, fall]) {
      c.addEventListener('mousemove', (e) => {
        const r = c.getBoundingClientRect();
        hover = { x: e.clientX - r.left, y: e.clientY - r.top, target: c };
        drawSpectrum(); showTip();
      });
      c.addEventListener('mouseleave', () => { hover = null; tip.hidden = true; drawSpectrum(); });
      c.addEventListener('click', (e) => {
        if (!frame || !canRecordings()) return;
        const r = c.getBoundingClientRect();
        const hz = frame.start + ((e.clientX - r.left) / r.width) * (frame.stop - frame.start);
        const ch = channelAt(hz);
        const f = ch ? ch.freq_hz : Math.round(hz / 2500) * 2500;
        location.hash = `#/recordings?freq=${MHZ(f, 4)}`;
      });
    }

    // --- status ---
    let lastRecorded = null;
    function onStatusView(st) {
      statEls.source.textContent = st.source || '–';
      statEls.source.title = st.source || '';
      statEls.center.textContent = st.center_freq_hz ? `${MHZ(st.center_freq_hz, 4)} MHz` : '–';
      statEls.uptime.textContent = fmtUptime(st.uptime_s || 0);
      statEls.load.textContent = `${Math.round((st.dsp_load || 0) * 100)}% of a core`;
      statEls.dropped.textContent = fmtCount(st.samples_dropped || 0);
      statEls.dropped.classList.toggle('warn', (st.samples_dropped || 0) > 0);
      statEls.recorded.textContent = fmtCount(st.recorded || 0);
      const act = (st.active || []).slice().sort((a, b) => a.freq_hz - b.freq_hz);
      activeList.replaceChildren(...(act.length ? act.map((a) => {
        const c = channelAt(a.freq_hz);
        const snr = Math.max(0, Math.min(40, a.snr_db));
        // A slot's own frequency, or (for a retuned grid signal) where it is.
        const target = c && c.kind !== 'grid' ? c.freq_hz : a.freq_hz;
        return h('li', { class: isAdmin() ? 'with-action' : '' },
          h('span', { class: 'f' },
            canAudio() && listen.supported ? h('button', {
              type: 'button', class: `icon-btn hp${listen.freqs.has(target) ? ' on' : ''}${listen.playing().some((f) => Math.abs(f - a.freq_hz) < 3e3) ? ' live' : ''}`,
              title: listen.freqs.has(target) ? 'Stop listening to this channel' : 'Listen to this channel',
              'aria-label': 'Listen', onclick: () => listen.toggleFreq(target),
            }, '\u{1F3A7}') : null,
            MHZ(a.freq_hz, 4)),
          h('span', { class: 'n' }, c && c.name ? c.name : (c ? kindLabel[c.kind] : '')),
          h('span', { class: 't' }, `${fmtDb(a.snr_db)} · ${fmtSecs(a.secs)}`),
          isAdmin() ? confirmButton('Lock out', 'Sure?', () => editChannel(target, { locked_out: true }), 'mini danger lock-btn') : null,
          h('div', { class: 'snr', title: `${a.snr_db.toFixed(1)} dB SNR` }, h('i', { style: `width:${(snr / 40) * 100}%` })));
      }) : [h('li', { class: 'empty' }, 'Quiet. Nothing on the air right now.')]));
      if (st.recorded !== lastRecorded) { lastRecorded = st.recorded; loadRecent(); }
      if (frame) drawSpectrum();
    }

    async function loadRecent() {
      if (!canRecordings()) {
        recentBody.replaceChildren(h('div', { class: 'empty' }, isAdmin() ? '' : 'Recordings are private on this scanner. Log in to see them.'));
        return;
      }
      try {
        const res = await api('/recordings?limit=8');
        const items = res.items;
        recentBody.replaceChildren(items.length ? h('ul', { class: 'rec-list' }, items.map((r, i) => {
          const when = nsDate(r.start_ns);
          return h('li', { dataset: { rec: r.id } },
            canAudio() ? playButton(items, i) : null,
            h('span', { class: 'f' }, MHZ(r.freq_hz, 4)),
            h('span', { class: 'meta' }, [timeFmt.format(when), r.channel || channelName(r.freq_hz), fmtSecs(r.audio_secs)].filter(Boolean).join(' · ')));
        })) : h('div', { class: 'empty' }, 'No recordings yet.'));
      } catch (e) {
        recentBody.replaceChildren(h('div', { class: 'empty' }, e.message));
      }
    }

    // --- packet toasts, just under the spectrum trace ---
    const TOAST_MS = 6000, MAX_TOASTS = 3;
    function onPacket(p) {
      const when = nsDate(p.time_ns);
      const head = [MHZ(p.freq_hz, 3), p.channel || channelName(p.freq_hz), timeFmt.format(when)].filter(Boolean).join(' · ');
      const t = h('a', { class: 'toast', href: '#/packets', title: 'All packets' },
        h('div', { class: 'toast-head' }, head),
        h('div', { class: 'toast-text mono' }, p.text));
      toasts.prepend(t);
      while (toasts.children.length > MAX_TOASTS) toasts.lastChild.remove();
      setTimeout(() => {
        t.classList.add('leaving');
        setTimeout(() => t.remove(), 300);
      }, TOAST_MS);
    }

    const listener = (kind, p) => {
      if (kind === 'spectrum') onSpectrum(p);
      else if (kind === 'packet') onPacket(p);
      else if (kind === 'status') onStatusView(p);
      else if (kind === 'private') {
        empty.textContent = 'Live status is private on this scanner. Log in to see it.';
        activeList.replaceChildren(h('li', { class: 'empty' }, 'Private'));
      }
    };
    live.listeners.add(listener);
    if (state.status) onStatusView(state.status);
    loadRecent();
    const ro = new ResizeObserver(() => drawSpectrum());
    ro.observe(spec);
    rangeSel.onchange = () => drawSpectrum();
    const relisten = () => { if (state.status) onStatusView(state.status); };
    listen.listeners.add(relisten);
    return () => { live.listeners.delete(listener); listen.listeners.delete(relisten); listenCtl.dispose(); ro.disconnect(); };
  }

  /** 256-entry colour map for the waterfall: deep blue, teal, green, yellow, white. */
  function colorLut() {
    const stops = [[0, [6, 10, 24]], [0.3, [22, 46, 120]], [0.5, [20, 130, 150]], [0.7, [110, 195, 80]], [0.88, [245, 215, 60]], [1, [255, 250, 225]]];
    const out = new Uint8Array(256 * 3);
    for (let i = 0; i < 256; i++) {
      const t = i / 255;
      let k = 0;
      while (k < stops.length - 2 && t > stops[k + 1][0]) k++;
      const [t0, c0] = stops[k], [t1, c1] = stops[k + 1];
      const u = (t - t0) / (t1 - t0);
      for (let j = 0; j < 3; j++) out[i * 3 + j] = Math.round(c0[j] + (c1[j] - c0[j]) * u);
    }
    return out;
  }

  // ----- Recordings timeline -----
  /** The recordings list's filters (from the page's URL) as API query
   * parameters, shared by the table and the timeline. */
  function recordingQuery(params) {
    const q = new URLSearchParams();
    const f = parseFloat(params.get('freq'));
    if (!isNaN(f)) {
      const t = parseFloat(params.get('tol') || '0.005');
      q.set('freq_min', String(Math.round((f - t) * 1e6)));
      q.set('freq_max', String(Math.round((f + t) * 1e6)));
    }
    for (const [k, name] of [['since', 'since'], ['until', 'until']]) {
      if (!params.get(k)) continue;
      const t = new Date(params.get(k)).getTime();
      if (!isNaN(t)) q.set(name, String(t / 1000));
    }
    if (params.get('min')) q.set('min_secs', params.get('min'));
    if (params.get('tone')) q.set('tone', params.get('tone'));
    if (params.get('dtmf')) q.set('dtmf', params.get('dtmf'));
    return q;
  }

  /** A local time as a datetime-local value, to the second. */
  function localInput(ms) {
    const d = new Date(ms);
    const p2 = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())}T${p2(d.getHours())}:${p2(d.getMinutes())}:${p2(d.getSeconds())}`;
  }

  const TIME_STEPS = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800, 604800];
  const FREQ_STEPS = [0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2];
  const hmFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
  const mdFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });

  /**
   * Every call matching the page's filters drawn as a horizontal line: time
   * across, frequency up. Hover for details, click to play, drag across to
   * narrow the page to that time range.
   */
  function recordingsTimeline(params, onPick) {
    const canvas = h('canvas', { class: 'tl-canvas', 'aria-label': 'Timeline of recordings, time across and frequency up' });
    const tip = h('div', { class: 'scope-tip', hidden: true });
    const summary = h('span', { class: 'muted' });
    const zoomOut = h('button', { type: 'button', hidden: !params.get('until'), onclick: () => {
      const p = new URLSearchParams(params); p.delete('since'); p.delete('until');
      location.hash = `#/recordings${p.toString() ? '?' + p : ''}`;
    } }, 'Show all times');
    const wrap = h('div', { class: 'tl' }, canvas, tip);
    const el = h('section', { class: 'panel tl-panel', hidden: true },
      h('div', { class: 'panel-head' }, h('h2', {}, 'Timeline'), summary, h('span', { class: 'spacer' }), zoomOut),
      wrap);
    const PAD = { l: 66, r: 12, t: 10, b: 24 };
    let spans = [];          // [id, freq_hz, start_ms, secs], newest first
    let view = null;         // {t0, t1, f0, f1}
    let hoverId = null, drag = null;

    function css(name) { return getComputedStyle(document.documentElement).getPropertyValue(name).trim(); }
    const playingId = () => (!$('#player').hidden && player.list[player.index] ? player.list[player.index].id : null);

    function computeView() {
      const now = Date.now();
      const since = params.get('since') ? new Date(params.get('since')).getTime() : NaN;
      const until = params.get('until') ? new Date(params.get('until')).getTime() : NaN;
      let t0 = Infinity, t1 = -Infinity, f0 = Infinity, f1 = -Infinity;
      for (const [, f, s, secs] of spans) {
        t0 = Math.min(t0, s); t1 = Math.max(t1, s + secs * 1000);
        f0 = Math.min(f0, f); f1 = Math.max(f1, f);
      }
      // Up to now unless the page ends earlier, so quiet time shows as quiet.
      t1 = !isNaN(until) ? until : Math.max(t1, now);
      if (!isNaN(since)) t0 = since;
      const tp = Math.max((t1 - t0) * 0.02, 5000);
      t0 -= tp;
      if (isNaN(until)) t1 += tp / 2;
      // A frequency filter sets the band; otherwise the calls do.
      const fq = parseFloat(params.get('freq'));
      if (!isNaN(fq)) {
        const tol = parseFloat(params.get('tol') || '0.005') * 1e6;
        f0 = Math.min(f0, fq * 1e6 - tol); f1 = Math.max(f1, fq * 1e6 + tol);
      }
      const span = Math.max(f1 - f0, 25e3);
      const mid = (f0 + f1) / 2;
      f0 = mid - span * 0.56; f1 = mid + span * 0.56;
      view = { t0, t1, f0, f1 };
    }

    function size() {
      const r = canvas.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      const w = Math.max(1, Math.round(r.width * dpr)), hh = Math.max(1, Math.round(r.height * dpr));
      if (canvas.width !== w || canvas.height !== hh) { canvas.width = w; canvas.height = hh; }
      return { W: r.width, H: r.height, dpr };
    }
    const xOf = (t, W) => PAD.l + ((t - view.t0) / (view.t1 - view.t0)) * (W - PAD.l - PAD.r);
    const tOf = (x, W) => view.t0 + ((x - PAD.l) / (W - PAD.l - PAD.r)) * (view.t1 - view.t0);
    const yOf = (f, H) => PAD.t + (1 - (f - view.f0) / (view.f1 - view.f0)) * (H - PAD.t - PAD.b);

    function draw() {
      if (!view) return;
      const { W, H, dpr } = size();
      const ctx = canvas.getContext('2d');
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, W, H);
      const grid = css('--grid'), muted = css('--muted'), accent = css('--accent'), text = css('--text');
      ctx.font = `11px ${css('--mono') || 'monospace'}`;
      ctx.lineWidth = 1;

      // Frequency grid, aiming for a line every ~36 px.
      const plotH = H - PAD.t - PAD.b;
      const fSpanMhz = (view.f1 - view.f0) / 1e6;
      const fStep = FREQ_STEPS.find((s) => (fSpanMhz / s) <= Math.max(2, plotH / 36)) || 5;
      const fDigits = Math.max(1, (String(fStep).split('.')[1] || '').length);
      ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
      for (let k = Math.ceil(view.f0 / 1e6 / fStep); k * fStep <= view.f1 / 1e6; k++) {
        const m = k * fStep;
        const y = Math.round(yOf(m * 1e6, H)) + 0.5;
        ctx.strokeStyle = grid; ctx.beginPath(); ctx.moveTo(PAD.l, y); ctx.lineTo(W - PAD.r, y); ctx.stroke();
        ctx.fillStyle = muted; ctx.fillText(m.toFixed(fDigits), PAD.l - 8, y);
      }

      // Time grid, a label every ~90 px; midnight gets the date.
      const plotW = W - PAD.l - PAD.r;
      const tSpan = (view.t1 - view.t0) / 1000;
      const tStep = TIME_STEPS.find((s) => tSpan / s <= Math.max(2, plotW / 90)) || 2592000;
      const tz = new Date().getTimezoneOffset() * 60000;
      ctx.textAlign = 'center'; ctx.textBaseline = 'top';
      const first = Math.ceil((view.t0 - (tStep >= 86400 ? -tz : 0)) / (tStep * 1000)) * tStep * 1000 + (tStep >= 86400 ? -tz : 0);
      for (let t = first; t <= view.t1; t += tStep * 1000) {
        const x = Math.round(xOf(t, W)) + 0.5;
        if (x < PAD.l || x > W - PAD.r) continue;
        ctx.strokeStyle = grid; ctx.beginPath(); ctx.moveTo(x, PAD.t); ctx.lineTo(x, H - PAD.b); ctx.stroke();
        const d = new Date(t);
        const midnight = d.getHours() === 0 && d.getMinutes() === 0 && d.getSeconds() === 0;
        const label = midnight || tStep >= 86400 ? mdFmt.format(d) : tStep < 60 ? timeFmt.format(d) : hmFmt.format(d);
        ctx.fillStyle = midnight ? text : muted; ctx.fillText(label, x, H - PAD.b + 6);
      }

      // The calls, oldest first so the newest sit on top.
      ctx.save();
      ctx.beginPath(); ctx.rect(PAD.l, 0, plotW, H); ctx.clip();
      ctx.lineCap = 'round'; ctx.lineWidth = 3;
      ctx.strokeStyle = accent; ctx.globalAlpha = 0.8;
      ctx.beginPath();
      for (let i = spans.length - 1; i >= 0; i--) {
        const [, f, s, secs] = spans[i];
        const x0 = xOf(s, W), x1 = Math.max(xOf(s + secs * 1000, W), x0 + 1), y = yOf(f, H);
        ctx.moveTo(x0, y); ctx.lineTo(x1, y);
      }
      ctx.stroke();
      ctx.globalAlpha = 1;
      const marks = [[playingId(), text, 4], [hoverId, text, 5]];
      for (const [id, color, lw] of marks) {
        const sp = id !== null && spans.find((x) => x[0] === id);
        if (!sp) continue;
        const x0 = xOf(sp[2], W), x1 = Math.max(xOf(sp[2] + sp[3] * 1000, W), x0 + 1), y = yOf(sp[1], H);
        ctx.strokeStyle = color; ctx.lineWidth = lw;
        ctx.beginPath(); ctx.moveTo(x0, y); ctx.lineTo(x1, y); ctx.stroke();
      }
      ctx.restore();

      if (drag && Math.abs(drag.x1 - drag.x0) > 2) {
        const a = Math.max(PAD.l, Math.min(drag.x0, drag.x1)), b = Math.min(W - PAD.r, Math.max(drag.x0, drag.x1));
        ctx.fillStyle = css('--active'); ctx.fillRect(a, PAD.t, b - a, plotH);
        ctx.strokeStyle = css('--active-edge'); ctx.lineWidth = 1;
        ctx.strokeRect(a + 0.5, PAD.t + 0.5, b - a - 1, plotH - 1);
      }
    }

    /** The call under the pointer: closest line within a few px. */
    function hit(x, y) {
      const { W, H } = size();
      let best = null, bestD = 7;
      for (const sp of spans) {
        const x0 = xOf(sp[2], W) - 4, x1 = Math.max(xOf(sp[2] + sp[3] * 1000, W), x0 + 5) + 4;
        if (x < x0 || x > x1) continue;
        const d = Math.abs(yOf(sp[1], H) - y);
        if (d < bestD) { best = sp; bestD = d; }
      }
      return best;
    }

    function local(e) {
      const r = canvas.getBoundingClientRect();
      return { x: e.clientX - r.left, y: e.clientY - r.top };
    }

    canvas.addEventListener('pointerdown', (e) => {
      if (!view || e.button !== 0) return;
      const { x } = local(e);
      drag = { x0: x, x1: x, id: e.pointerId };
      canvas.setPointerCapture(e.pointerId);
    });
    canvas.addEventListener('pointermove', (e) => {
      if (!view) return;
      const { x, y } = local(e);
      if (drag) { drag.x1 = x; tip.hidden = true; hoverId = null; draw(); return; }
      const sp = hit(x, y);
      const id = sp ? sp[0] : null;
      canvas.classList.toggle('pointing', !!sp);
      if (id !== hoverId) { hoverId = id; draw(); }
      if (!sp) { tip.hidden = true; return; }
      const when = new Date(sp[2]);
      fill(tip,
        h('div', { class: 'mono' }, `${MHZ(sp[1], 4)} MHz`),
        channelName(sp[1]) ? h('div', {}, channelName(sp[1])) : null,
        h('div', { class: 'muted' }, `${dayLabel(when)} ${timeFmt.format(when)} · ${fmtSecs(sp[3])}`),
        canAudio() ? h('div', { class: 'muted' }, 'Click to play') : null);
      tip.hidden = false;
      const W = wrap.clientWidth;
      const left = Math.min(Math.max(4, x + 12), W - tip.offsetWidth - 4);
      tip.style.left = `${left}px`;
      tip.style.top = `${Math.max(4, y - tip.offsetHeight - 10)}px`;
    });
    const endDrag = (e) => {
      if (!drag) return;
      const d = drag; drag = null;
      const { W } = size();
      if (Math.abs(d.x1 - d.x0) > 6) {
        const a = tOf(Math.min(d.x0, d.x1), W), b = tOf(Math.max(d.x0, d.x1), W);
        const p = new URLSearchParams(params);
        p.set('since', localInput(Math.floor(a / 1000) * 1000));
        p.set('until', localInput(Math.ceil(b / 1000) * 1000));
        location.hash = `#/recordings?${p}`;
        return;
      }
      draw();
      if (e.type === 'pointerup') {
        const { x, y } = local(e);
        const sp = hit(x, y);
        if (sp) onPick(sp[0]);
      }
    };
    canvas.addEventListener('pointerup', endDrag);
    canvas.addEventListener('pointercancel', endDrag);
    canvas.addEventListener('pointerleave', () => { if (!drag) { tip.hidden = true; if (hoverId !== null) { hoverId = null; draw(); } } });

    const ro = new ResizeObserver(() => draw());
    ro.observe(canvas);
    const audio = $('#audio');
    audio.addEventListener('play', draw);
    const scheme = window.matchMedia('(prefers-color-scheme: light)');
    scheme.addEventListener('change', draw);

    async function load() {
      const q = recordingQuery(params);
      q.set('limit', '2000');
      try {
        const res = await api(`/recordings/timeline?${q}`);
        spans = res.items;
        el.hidden = spans.length === 0;
        if (!spans.length) return;
        const freqs = new Set(spans.map((s) => s[1]));
        const n = fmtCount(spans.length);
        summary.textContent = res.truncated
          ? `latest ${n} calls on ${fmtCount(freqs.size)} frequencies · drag to zoom in`
          : `${n} call${spans.length === 1 ? '' : 's'} on ${fmtCount(freqs.size)} frequenc${freqs.size === 1 ? 'y' : 'ies'} · drag to zoom in`;
        computeView();
        draw();
      } catch (_) {
        el.hidden = true;
      }
    }

    return {
      el, load,
      dispose() { ro.disconnect(); audio.removeEventListener('play', draw); scheme.removeEventListener('change', draw); },
    };
  }

  // ----- Recordings -----
  function viewRecordings(root, params) {
    root.append(h('div', { class: 'page-title' }, h('h1', {}, 'Recordings'), h('span', { class: 'muted', id: 'rec-count' })));
    if (!canRecordings()) {
      root.append(notice('Recordings are private', 'The operator hasn’t made recordings public on this scanner.', h('a', { href: '#/admin' }, 'Log in')));
      return null;
    }
    const named = state.channels.filter((c) => c.name).sort((a, b) => a.freq_hz - b.freq_hz);
    const freq = h('input', { type: 'number', step: '0.0025', min: '0', placeholder: '146.5200', value: params.get('freq') || '' });
    const tol = h('select', {}, [['0.005', '±5 kHz'], ['0.0125', '±12.5 kHz'], ['0.1', '±100 kHz'], ['1', '±1 MHz']]
      .map(([v, t]) => h('option', { value: v, selected: (params.get('tol') || '0.005') === v }, t)));
    const chan = h('select', {}, h('option', { value: '' }, 'Any channel'),
      named.map((c) => h('option', { value: MHZ(c.freq_hz, 4) }, `${MHZ(c.freq_hz, 4)} ${c.name}${c.kind === 'repeater_input' ? ' (input)' : ''}`)));
    chan.value = params.get('freq') || '';
    chan.onchange = () => { freq.value = chan.value; apply(); };
    const since = h('input', { type: 'datetime-local', value: params.get('since') || '' });
    const minSecs = h('input', { type: 'number', min: '0', step: '0.5', placeholder: '0', value: params.get('min') || '', style: 'width:80px' });
    const tone = h('input', { type: 'text', placeholder: '100.0 or D023N', value: params.get('tone') || '', style: 'width:120px', 'aria-label': 'Tone' });
    const dtmf = h('input', { type: 'text', placeholder: 'digits or any', value: params.get('dtmf') || '', style: 'width:110px', 'aria-label': 'DTMF digits' });
    const form = h('form', { class: 'filters', onsubmit: (e) => { e.preventDefault(); apply(); } },
      h('label', {}, 'Channel', chan),
      h('label', {}, 'Frequency (MHz)', freq),
      h('label', {}, 'Within', tol),
      h('label', {}, 'Since', since),
      h('label', {}, 'Min length (s)', minSecs),
      h('label', {}, 'Tone', tone),
      h('label', {}, 'DTMF', dtmf),
      h('button', { type: 'submit', class: 'primary' }, 'Search'),
      h('button', { type: 'button', onclick: () => { location.hash = '#/recordings'; } }, 'Clear'));
    const tbody = h('tbody');
    const table = h('table', {},
      h('thead', {}, h('tr', {}, h('th', {}, ''), h('th', {}, 'Time'), h('th', {}, 'Frequency'), h('th', {}, 'Channel'), h('th', {}, 'Tone'), h('th', {}, 'DTMF'), h('th', { class: 'num' }, 'Length'), h('th', { class: 'num' }, 'Peak SNR'), h('th', {}, ''))),
      tbody);
    const more = h('button', { type: 'button' }, 'Load older');
    const status = h('div', { class: 'empty', hidden: true });
    const timeline = recordingsTimeline(params, pick);
    root.append(
      h('section', { class: 'panel' }, h('div', { class: 'panel-body' }, form)),
      h('div', { style: 'height:16px' }),
      timeline.el,
      h('section', { class: 'panel' }, h('div', { class: 'table-wrap' }, table), status, h('div', { class: 'more' }, more)));

    function apply() {
      const p = new URLSearchParams();
      if (freq.value) { p.set('freq', freq.value); p.set('tol', tol.value); }
      if (since.value) p.set('since', since.value);
      if (minSecs.value) p.set('min', minSecs.value);
      if (tone.value.trim()) p.set('tone', tone.value.trim().toUpperCase());
      if (dtmf.value.trim()) p.set('dtmf', dtmf.value.trim().toUpperCase());
      if (since.value && params.get('until')) p.set('until', params.get('until'));
      location.hash = `#/recordings${p.toString() ? '?' + p : ''}`;
    }

    const items = [];
    let next = null, lastDay = null;
    async function load() {
      const q = recordingQuery(params);
      q.set('limit', '100');
      if (next !== null) q.set('before_id', String(next));
      more.disabled = true;
      try {
        const res = await api(`/recordings?${q}`);
        const start = items.length;
        items.push(...res.items);
        for (let i = start; i < items.length; i++) {
          const r = items[i];
          const when = nsDate(r.start_ns);
          const day = dayLabel(when);
          if (day !== lastDay) { lastDay = day; tbody.append(h('tr', { class: 'day' }, h('td', { colspan: 9 }, day))); }
          tbody.append(h('tr', { dataset: { rec: r.id }, class: canAudio() ? 'clickable' : '', onclick: canAudio() ? () => player.play(items, i) : null },
            h('td', {}, canAudio() ? playButton(items, i) : null),
            h('td', { class: 'mono' }, timeFmt.format(when)),
            h('td', { class: 'mono' }, MHZ(r.freq_hz, 4)),
            h('td', { class: 'grow' }, r.channel || channelName(r.freq_hz) || h('span', { class: 'muted' }, kindLabel[r.origin] || r.origin)),
            h('td', { class: 'mono', title: r.tone ? (r.tone.startsWith('D') ? 'DCS code' : 'CTCSS tone (Hz)') : '' }, r.tone || ''),
            h('td', { class: 'mono dtmf', title: r.dtmf ? 'DTMF digits heard' : '' }, r.dtmf || ''),
            h('td', { class: 'num mono' }, fmtSecs(r.audio_secs)),
            h('td', { class: 'num mono' }, r.peak_snr_db === null ? '' : fmtDb(r.peak_snr_db)),
            h('td', {}, canAudio() ? h('a', { href: r.wav_url, download: '', onclick: (e) => e.stopPropagation(), title: 'Download WAV' }, 'WAV') : null)));
        }
        next = res.items.length === 100 ? res.next_before_id : null;
        more.hidden = next === null;
        $('#rec-count').textContent = `${fmtCount(items.length)}${next !== null ? '+' : ''} shown`;
        status.hidden = items.length > 0;
        status.textContent = 'No recordings match.';
        if (player.list.length && !$('#player').hidden) {
          const cur = player.list[player.index];
          if (cur) document.querySelectorAll(`[data-rec="${cur.id}"]`).forEach((el) => el.classList.add('playing'));
        }
      } catch (e) {
        status.hidden = false; status.textContent = e.message;
      } finally { more.disabled = false; }
    }
    more.onclick = load;
    load();
    timeline.load();

    // A call picked on the timeline: play it from the table when it's
    // there (so next and previous follow the table), else on its own.
    async function pick(id) {
      const i = items.findIndex((r) => r.id === id);
      const row = tbody.querySelector(`[data-rec="${id}"]`);
      if (row) row.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
      if (!canAudio()) return;
      if (i >= 0) { player.play(items, i); return; }
      try { player.play([await api(`/recordings/${id}`)], 0); } catch (_) { /* gone since */ }
    }
    return () => timeline.dispose();
  }

  // ----- Packets -----
  function viewPackets(root) {
    root.append(h('div', { class: 'page-title' }, h('h1', {}, 'Packets'), h('span', { class: 'muted', id: 'pk-count' })));
    if (!canRecordings()) {
      root.append(notice('Packets are private', 'The operator hasn’t made recordings public on this scanner.', h('a', { href: '#/admin' }, 'Log in')));
      return null;
    }
    const tbody = h('tbody');
    const status = h('div', { class: 'empty', hidden: true });
    const more = h('button', { type: 'button' }, 'Load older');
    root.append(h('section', { class: 'panel' },
      h('div', { class: 'table-wrap' }, h('table', {},
        h('thead', {}, h('tr', {}, h('th', {}, 'Time'), h('th', {}, 'Frequency'), h('th', {}, 'Packet'))),
        tbody)),
      status, h('div', { class: 'more' }, more)));
    let newest = 0, oldest = null;
    const row = (p) => {
      const when = nsDate(p.time_ns);
      return h('tr', {},
        h('td', { class: 'mono' }, `${dayLabel(when)} ${timeFmt.format(when)}`),
        h('td', { class: 'mono' }, MHZ(p.freq_hz, 4)),
        h('td', { class: 'grow mono packet' }, p.text));
    };
    async function load(older) {
      const q = new URLSearchParams({ limit: '100' });
      if (older && oldest !== null) q.set('before_id', String(oldest));
      try {
        const res = await api(`/packets?${q}`);
        if (older || newest === 0) {
          tbody.append(...res.items.map(row));
          if (res.items.length) oldest = res.items[res.items.length - 1].id;
          more.hidden = res.next_before_id === null;
        } else {
          const fresh = res.items.filter((p) => p.id > newest);
          tbody.prepend(...fresh.map(row));
        }
        if (res.items.length) newest = Math.max(newest, res.items[0].id);
        const n = tbody.children.length;
        $('#pk-count').textContent = n ? `${fmtCount(n)} shown` : '';
        status.hidden = n > 0;
        status.textContent = 'No packets decoded yet. Channels with mode afsk1200 (144.390 APRS, 144.920) decode them.';
      } catch (e) {
        status.hidden = false; status.textContent = e.message;
      }
    }
    more.onclick = () => load(true);
    load(false);
    const timer = setInterval(() => load(false), 5000);
    return () => clearInterval(timer);
  }

  // ----- Channels -----
  function viewChannels(root) {
    root.append(h('div', { class: 'page-title' }, h('h1', {}, 'Channels'), h('span', { class: 'muted', id: 'ch-count' })));
    const filters = [['named', 'Named'], ['repeaters', 'Repeaters'], ['all', 'All'], ['grid', 'Grid'], ['priority', 'Priority'], ['locked', 'Locked out']];
    let filter = 'named';
    const search = h('input', { type: 'search', placeholder: 'Search name or MHz', 'aria-label': 'Search channels' });
    const chips = h('div', { class: 'chips' }, filters.map(([k, t]) =>
      h('button', { type: 'button', class: 'chip', 'aria-pressed': String(k === filter), onclick: () => { filter = k; chips.querySelectorAll('.chip').forEach((c, i) => c.setAttribute('aria-pressed', String(filters[i][0] === k))); render(); } }, t)));
    const tbody = h('tbody');
    root.append(
      h('section', { class: 'panel' },
        h('div', { class: 'panel-head' }, chips, h('span', { class: 'spacer' }), search),
        h('div', { class: 'table-wrap' }, h('table', {},
          h('thead', {}, h('tr', {}, ['Frequency', 'Name', 'Type', 'Mode', 'Bandwidth', '', 'Pair', isAdmin() ? 'Change' : ''].map((t) => h('th', {}, t)))),
          tbody))));
    search.oninput = render;

    function matches(c) {
      switch (filter) {
        case 'named': if (!c.name) return false; break;
        case 'repeaters': if (!c.kind.startsWith('repeater')) return false; break;
        case 'grid': if (c.kind !== 'grid') return false; break;
        case 'priority': if (!c.priority) return false; break;
        case 'locked': if (!c.locked_out) return false; break;
        default: break;
      }
      const q = search.value.trim().toLowerCase();
      return !q || (c.name || '').toLowerCase().includes(q) || MHZ(c.freq_hz, 4).includes(q);
    }

    function render() {
      const active = new Set(((state.status && state.status.active) || []).map((a) => {
        const c = channelAt(a.freq_hz); return c ? c.freq_hz : null;
      }));
      const rows = state.channels.filter(matches).sort((a, b) => a.freq_hz - b.freq_hz);
      $('#ch-count').textContent = `${fmtCount(rows.length)} of ${fmtCount(state.channels.length)} watched`;
      tbody.replaceChildren(...(rows.length ? rows.map((c) => h('tr', {
        id: `ch-${Math.round(c.freq_hz)}`,
        class: `${canRecordings() ? 'clickable' : ''}${active.has(c.freq_hz) ? ' is-active' : ''}`,
        title: canRecordings() ? 'Show recordings' : null,
        onclick: canRecordings() ? () => { location.hash = `#/recordings?freq=${MHZ(c.freq_hz, 4)}`; } : null,
      },
      h('td', { class: 'mono' }, MHZ(c.freq_hz, 4)),
      h('td', { class: 'grow' }, c.name || h('span', { class: 'muted' }, '–')),
      h('td', {}, kindLabel[c.kind] || c.kind),
      h('td', {}, c.mode.toUpperCase()),
      h('td', { class: 'mono' }, `${(c.bandwidth_hz / 1e3).toFixed(c.bandwidth_hz % 1e3 ? 1 : 0)} kHz`),
      h('td', {}, active.has(c.freq_hz) ? h('span', { class: 'badge live' }, 'On air') : null,
        c.priority ? h('span', { class: 'badge prio' }, 'Priority') : null,
        c.locked_out ? h('span', { class: 'badge lock' }, 'Locked out') : null,
        c.spur ? h('span', { class: 'badge', title: 'Locked out automatically: a carrier that stayed on with a steady level, like a birdie or a beacon. Reset or Remove to unlock.' }, 'Spur') : null,
        c.overridden && !c.spur ? h('span', { class: 'badge', title: 'Changed in the web UI; Reset goes back to the config file' }, 'Edited') : null),
      h('td', { class: 'mono' }, c.pair_hz ? h('a', { href: `#/channels`, onclick: (e) => { e.preventDefault(); e.stopPropagation(); jump(c.pair_hz); } }, `${c.kind === 'repeater_output' ? 'in' : 'out'} ${MHZ(c.pair_hz, 4)}`) : null),
      h('td', { class: 'actions' }, isAdmin() ? actions(c) : null))) :
        [h('tr', {}, h('td', { colspan: 8, class: 'empty' }, state.channels.length ? 'No channels match.' : 'No channel list (is live status private?).'))]));
    }

    function actions(c) {
      const run = (fn) => async (e) => {
        e.stopPropagation();
        const b = e.currentTarget; b.disabled = true;
        try { await fn(); render(); } catch (ex) { b.disabled = false; b.textContent = ex.message; }
      };
      return [
        c.kind === 'lockout' ? null : h('button', { type: 'button', class: 'mini', onclick: run(() => editChannel(c.freq_hz, { locked_out: !c.locked_out })) }, c.locked_out ? 'Unlock' : 'Lock out'),
        c.kind === 'lockout' ? null : h('button', { type: 'button', class: 'mini', onclick: run(() => editChannel(c.freq_hz, { priority: !c.priority })) }, c.priority ? 'No priority' : 'Priority'),
        c.overridden ? h('button', { type: 'button', class: 'mini', title: 'Back to the config file', onclick: run(() => resetChannel(c.freq_hz)) }, c.kind === 'lockout' ? 'Remove' : 'Reset') : null,
      ];
    }

    function jump(hz) {
      const row = document.getElementById(`ch-${Math.round(hz)}`);
      if (!row) { filter = 'all'; chips.querySelectorAll('.chip').forEach((c, i) => c.setAttribute('aria-pressed', String(filters[i][0] === 'all'))); render(); }
      const r = document.getElementById(`ch-${Math.round(hz)}`);
      if (r) { r.scrollIntoView({ block: 'center', behavior: 'smooth' }); r.classList.add('playing'); setTimeout(() => r.classList.remove('playing'), 1600); }
    }

    const listener = (kind) => { if (kind === 'status') render(); };
    live.listeners.add(listener);
    render();
    return () => live.listeners.delete(listener);
  }

  // ----- Admin -----
  function viewAdmin(root) {
    root.append(h('div', { class: 'page-title' }, h('h1', {}, isAdmin() ? 'Admin' : 'Log in')));
    if (!isAdmin()) {
      if (state.session && !state.session.accounts_configured) {
        root.append(notice('No accounts on this scanner', 'Logins aren’t set up here.'));
        return null;
      }
      const user = h('input', { type: 'text', autocomplete: 'username', required: true, autofocus: true });
      const pw = h('input', { type: 'password', autocomplete: 'current-password', required: true });
      const err = h('div', { class: 'error', role: 'alert' });
      const btn = h('button', { type: 'submit', class: 'primary' }, 'Log in');
      const form = h('form', { class: 'form', onsubmit: async (e) => {
        e.preventDefault(); err.textContent = ''; btn.disabled = true;
        try {
          state.session = await api('/auth/login', { method: 'POST', body: { username: user.value.trim(), password: pw.value } });
          await afterLoginChange();
          location.hash = '#/live';
        } catch (ex) {
          err.textContent = ex.status === 401 ? 'Wrong username or password.' : ex.message;
          pw.value = ''; pw.focus();
        } finally { btn.disabled = false; }
      } },
      h('label', {}, 'Username', user), h('label', {}, 'Password', pw), err, h('div', { class: 'row' }, btn),
      h('p', { class: 'muted' }, 'Accounts are created on the scanner with ', h('code', {}, 'scanner admin create <name>'), '.'));
      root.append(h('section', { class: 'panel', style: 'max-width:440px' }, h('div', { class: 'panel-body' }, form)));
      return null;
    }

    const s = state.session;
    const logout = h('button', { type: 'button', onclick: async () => {
      try { await api('/auth/logout', { method: 'POST' }); } catch (_) { /* already gone */ }
      state.session = null; await afterLoginChange(); location.hash = '#/live';
    } }, 'Log out');
    const p = pub();
    const yes = (b) => (b ? h('span', { class: 'ok' }, 'Yes') : h('span', { class: 'muted' }, 'No'));
    const account = h('section', { class: 'panel' },
      h('div', { class: 'panel-head' }, h('h2', {}, 'Account'), h('span', { class: 'spacer' }), logout),
      h('div', { class: 'panel-body' },
        h('dl', { class: 'kv' },
          h('dt', {}, 'Signed in as'), h('dd', {}, s.username),
          h('dt', {}, 'Session ends'), h('dd', {}, s.expires_ns ? dateTimeFmt.format(nsDate(s.expires_ns)) : '–'))));

    const visitors = h('section', { class: 'panel' },
      h('div', { class: 'panel-head' }, h('h2', {}, 'What visitors can see')),
      h('div', { class: 'panel-body' },
        h('dl', { class: 'kv' },
          h('dt', {}, 'Live spectrum and status'), h('dd', {}, yes(p.live)),
          h('dt', {}, 'Recordings list'), h('dd', {}, yes(p.recordings)),
          h('dt', {}, 'Recording audio'), h('dd', {}, yes(p.audio))),
        h('p', { class: 'muted' }, 'Set when starting the scanner with ', h('code', {}, '--public-recordings'), ', ', h('code', {}, '--public-audio'), ' and ', h('code', {}, '--private-live'), '.')));

    // Storage: what the recordings use and what retention has deleted.
    const storageBody = h('div', { class: 'panel-body' }, h('p', { class: 'muted' }, 'Loading…'));
    const storage = h('section', { class: 'panel' },
      h('div', { class: 'panel-head' }, h('h2', {}, 'Storage')), storageBody);
    api('/storage').then((st) => {
      const r = st.retention;
      const rows = [];
      if (r && r.budget_bytes) {
        rows.push(h('dt', {}, 'Recordings'), h('dd', {},
          `${fmtBytes(r.used_bytes)} of ${fmtBytes(r.budget_bytes)}`,
          h('meter', { class: 'disk', min: 0, max: r.budget_bytes, high: 0.95 * r.budget_bytes, low: 0.85 * r.budget_bytes, optimum: 0, value: r.used_bytes })));
      } else {
        rows.push(h('dt', {}, 'Recordings'), h('dd', {}, `${fmtBytes(st.used_bytes)}${r ? ', no budget' : ''}`));
      }
      if (r && r.free_bytes !== null) {
        rows.push(h('dt', {}, 'Free on disk'), h('dd', {}, fmtBytes(r.free_bytes),
          r.min_free_bytes ? h('span', { class: 'muted' }, ` (keeps ${fmtBytes(r.min_free_bytes)})`) : null));
      }
      if (st.oldest_ns) {
        const t = nsDate(st.oldest_ns);
        rows.push(h('dt', {}, 'Oldest recording'), h('dd', {}, `${dayLabel(t)} ${timeFmt.format(t)}`));
      }
      if (r) rows.push(h('dt', {}, 'Deleted to make room'), h('dd', {}, r.expired ? `${fmtCount(r.expired)} (${fmtBytes(r.expired_bytes)}) since the scanner started` : 'None since the scanner started'));
      storageBody.replaceChildren(h('dl', { class: 'kv' }, rows),
        h('p', { class: 'muted' }, r
          ? ['Past 95 % of the budget the oldest recordings are deleted until under 85 %. Set with ', h('code', {}, '--disk-budget-gb'), ' and ', h('code', {}, '--min-free-gb'), '.']
          : 'Retention is off: nothing is deleted.'));
    }).catch((ex) => storageBody.replaceChildren(h('p', { class: 'error' }, ex.message)));

    // Data sent: what the uplink has carried this month, by role.
    const dataBody = h('div', { class: 'panel-body' }, h('p', { class: 'muted' }, 'Loading…'));
    const data = h('section', { class: 'panel' },
      h('div', { class: 'panel-head' }, h('h2', {}, 'Data sent')), dataBody);
    api('/egress').then((eg) => {
      const c = eg.current;
      if (!c) { dataBody.replaceChildren(h('p', { class: 'muted' }, 'Not counted on this scanner.')); return; }
      const levels = { ok: null, low: h('span', { class: 'badge' }, 'Slowed'), spent: h('span', { class: 'badge warn' }, 'Used up') };
      const rows = [
        h('dt', {}, 'Visitors'), h('dd', {},
          c.budget_bytes ? `${fmtBytes(c.anonymous_bytes)} of ${fmtBytes(c.budget_bytes)} ` : `${fmtBytes(c.anonymous_bytes)}, no limit`,
          levels[c.level],
          c.budget_bytes ? h('meter', { class: 'disk', min: 0, max: c.budget_bytes, low: 0.8 * c.budget_bytes, high: 0.999 * c.budget_bytes, optimum: 0, value: Math.min(c.anonymous_bytes, c.budget_bytes) }) : null),
        h('dt', {}, 'Admins'), h('dd', {}, fmtBytes(c.admin_bytes)),
      ];
      if (c.budget_bytes) rows.push(h('dt', {}, 'Allowance resets'), h('dd', {}, dateTimeFmt.format(nsDate(c.resets_ns))));
      const months = new Map();
      for (const e of eg.history) {
        if (e.month === c.month) continue;
        const m = months.get(e.month) || { anonymous: 0, admin: 0 };
        m[e.role] = e.bytes; months.set(e.month, m);
      }
      const past = months.size ? h('table', { class: 'table compact' },
        h('thead', {}, h('tr', {}, h('th', {}, 'Month'), h('th', { class: 'num' }, 'Visitors'), h('th', { class: 'num' }, 'Admins'))),
        h('tbody', {}, [...months].map(([m, v]) => h('tr', {}, h('td', { class: 'mono' }, m), h('td', { class: 'num mono' }, fmtBytes(v.anonymous)), h('td', { class: 'num mono' }, fmtBytes(v.admin)))))) : null;
      dataBody.replaceChildren(...[h('dl', { class: 'kv' }, rows), past].filter(Boolean),
        h('p', { class: 'muted' }, 'This month is ', h('span', { class: 'mono' }, c.month), ' (UTC). Past 80 % of the visitor allowance their spectrum slows; once it is used up their audio and spectrum stop. Set with ', h('code', {}, '--anon-egress-gb'), '.'));
    }).catch((ex) => dataBody.replaceChildren(h('p', { class: 'error' }, ex.message)));

    // Password.
    const cur = h('input', { type: 'password', autocomplete: 'current-password', required: true });
    const nw = h('input', { type: 'password', autocomplete: 'new-password', required: true, minlength: 10 });
    const nw2 = h('input', { type: 'password', autocomplete: 'new-password', required: true, minlength: 10 });
    const pwMsg = h('div', { role: 'status' });
    const pwForm = h('form', { class: 'form', onsubmit: async (e) => {
      e.preventDefault(); pwMsg.className = ''; pwMsg.textContent = '';
      if (nw.value !== nw2.value) { pwMsg.className = 'error'; pwMsg.textContent = 'The new passwords don’t match.'; return; }
      try {
        const r = await api('/auth/password', { method: 'POST', body: { current: cur.value, new: nw.value } });
        state.session.csrf_token = r.csrf_token;
        cur.value = nw.value = nw2.value = '';
        pwMsg.className = 'ok'; pwMsg.textContent = 'Password changed. Other browsers have been logged out.';
      } catch (ex) {
        pwMsg.className = 'error'; pwMsg.textContent = ex.status === 401 ? 'The current password is wrong.' : ex.message;
      }
    } },
    h('label', {}, 'Current password', cur), h('label', {}, 'New password (10+ characters)', nw), h('label', {}, 'New password again', nw2),
    pwMsg, h('div', { class: 'row' }, h('button', { type: 'submit' }, 'Change password')));
    const password = h('section', { class: 'panel' }, h('div', { class: 'panel-head' }, h('h2', {}, 'Password')), h('div', { class: 'panel-body' }, pwForm));

    // Tokens.
    const tokBody = h('tbody');
    const tokName = h('input', { type: 'text', placeholder: 'e.g. backup script', required: true, maxlength: 64 });
    const tokDays = h('input', { type: 'number', min: '1', max: '3650', placeholder: 'never', style: 'width:96px' });
    const tokMsg = h('div');
    const tokForm = h('form', { class: 'filters', onsubmit: async (e) => {
      e.preventDefault(); tokMsg.replaceChildren();
      try {
        const body = { name: tokName.value.trim() };
        if (tokDays.value) body.expires_days = Number(tokDays.value);
        const r = await api('/auth/tokens', { method: 'POST', body });
        const code = h('code', {}, r.token);
        const copy = h('button', { type: 'button', onclick: async () => {
          try { await navigator.clipboard.writeText(r.token); copy.textContent = 'Copied'; } catch (_) { copy.textContent = 'Select and copy'; }
        } }, 'Copy');
        tokMsg.replaceChildren(h('div', { class: 'secret' }, code, copy),
          h('p', { class: 'muted' }, `Token “${r.name}” created. It won’t be shown again; use it as `, h('code', {}, 'Authorization: Bearer scn_…'), '.'));
        tokName.value = ''; tokDays.value = '';
        loadTokens();
      } catch (ex) { tokMsg.replaceChildren(h('div', { class: 'error' }, ex.message)); }
    } },
    h('label', {}, 'Name', tokName), h('label', {}, 'Expires after (days)', tokDays), h('button', { type: 'submit', class: 'primary' }, 'Create token'));
    const tokens = h('section', { class: 'panel', style: 'grid-column: 1 / -1' },
      h('div', { class: 'panel-head' }, h('h2', {}, 'API tokens')),
      h('div', { class: 'panel-body' }, tokForm, tokMsg),
      h('div', { class: 'table-wrap' }, h('table', {},
        h('thead', {}, h('tr', {}, ['Name', 'Created', 'Last used', 'Expires', '', ''].map((t) => h('th', {}, t)))), tokBody)));

    async function loadTokens() {
      try {
        const list = await api('/auth/tokens');
        const now = Date.now() * 1e6;
        tokBody.replaceChildren(...(list.length ? list.map((t) => {
          const expired = t.expires_ns && t.expires_ns <= now;
          const st = t.revoked ? h('span', { class: 'badge lock' }, 'Revoked') : expired ? h('span', { class: 'badge' }, 'Expired') : h('span', { class: 'badge live' }, 'Active');
          return h('tr', {},
            h('td', { class: 'grow' }, t.name),
            h('td', {}, dateTimeFmt.format(nsDate(t.created_ns))),
            h('td', {}, t.last_used_ns ? dateTimeFmt.format(nsDate(t.last_used_ns)) : h('span', { class: 'muted' }, 'Never')),
            h('td', {}, t.expires_ns ? dateTimeFmt.format(nsDate(t.expires_ns)) : h('span', { class: 'muted' }, 'Never')),
            h('td', {}, st),
            h('td', {}, t.revoked ? null : h('button', { type: 'button', class: 'danger', onclick: async (e) => {
              const b = e.currentTarget;
              if (b.dataset.confirm !== '1') { b.dataset.confirm = '1'; b.textContent = 'Really revoke?'; return; }
              try { await api(`/auth/tokens/${t.id}`, { method: 'DELETE' }); loadTokens(); } catch (ex) { b.textContent = ex.message; }
            } }, 'Revoke')));
        }) : [h('tr', {}, h('td', { colspan: 6, class: 'empty' }, 'No tokens yet.'))]));
      } catch (ex) { tokBody.replaceChildren(h('tr', {}, h('td', { colspan: 6, class: 'error' }, ex.message))); }
    }
    loadTokens();

    root.append(h('div', { class: 'grid two-col' }, account, visitors, storage, data, password, tokens));
    return null;
  }

  async function afterLoginChange() {
    if (!state.session) await loadSession(); else renderWho();
    await loadChannels();
    // Reopen the socket so it runs as the new role.
    live.reconnect();
  }

  // ---------- start ----------
  window.addEventListener('hashchange', route);
  view().before(budgetBanner);
  (async () => {
    await loadSession();
    await loadChannels();
    live.connect();
    route();
  })();
})();
