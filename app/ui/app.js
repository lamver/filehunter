const DEV = /fhdev/.test(location.href);
const invoke = !DEV ? window.__TAURI__.core.invoke : async (cmd, args) => {
  // тестовая версия: вызовы дольше 300 мс и ошибки — в журнал dev.log
  const t = performance.now();
  try { return await window.__TAURI__.core.invoke(cmd, args); } catch (e) { if (cmd !== 'dev_log') devLog(`вызов ${cmd} — ошибка: ${e}`); throw e; } finally {
    const ms = performance.now() - t;
    if (ms > 300 && cmd !== 'dev_log') devLog(`вызов ${cmd}: ${Math.round(ms)} мс`);
  }
};
// тестовая версия (dev.bat): что интерфейс поднялся и его ошибки — в журнал dev.log
function devLog(m) { if (DEV) window.__TAURI__.core.invoke('dev_log', { msg: String(m) }).catch(() => {}); }
window.addEventListener('error', (e) => devLog(`ошибка: ${e.message} @ ${(e.filename || '').split('/').pop()}:${e.lineno}`));
window.addEventListener('unhandledrejection', (e) => devLog(`ошибка (promise): ${e.reason}`));
// тестовая версия: файл интерфейса изменился в папке проекта — окно перезагружается само
if (DEV) {
  const seen = {};
  setInterval(async () => {
    for (const f of ['index.html', 'app.js', 'style.css']) {
      try {
        const text = await (await fetch(f, { cache: 'no-store' })).text();
        if (seen[f] !== undefined && seen[f] !== text) { devLog(`изменился ${f} — перезагружаю`); location.reload(); return; }
        seen[f] = text;
      } catch { /* файл сохраняется прямо сейчас — проверим в следующий раз */ }
    }
  }, 1000);
}
const { listen } = window.__TAURI__.event;
const $ = (s) => document.querySelector(s);
const $$ = (s) => [...document.querySelectorAll(s)];

// ---------- утилиты ----------
// числа, размеры и даты — по языку интерфейса
const locale = () => (LANG === 'ru' ? 'ru-RU' : 'en-US');
const fmtSize = (n) => {
  const u = ['unit.B', 'unit.KB', 'unit.MB', 'unit.GB', 'unit.TB'].map((k) => tr(k));
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i === 0 ? `${n} ${u[0]}` : `${n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
};
const fmtNum = (n) => n.toLocaleString(locale());
const fmtDate = (s) => s ? new Date(s * 1000).toLocaleString(locale(), {
  day: '2-digit', month: '2-digit', year: 'numeric', hour: '2-digit', minute: '2-digit',
}) : '—';
const esc = (s) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const joinPath = (dir, name) => (/[\\/]$/.test(dir) ? dir + name : dir + (dir.includes('/') && !dir.includes('\\') ? '/' : '\\') + name);
const baseName = (p) => p.split(/[\\/]/).pop();
const cmp = (a, b) => (a < b ? -1 : a > b ? 1 : 0);

// Подтверждение действия. Возвращает Promise<boolean>: true — только если нажали кнопку действия.
// Фокус по умолчанию — на «Отмена», чтобы случайный Enter ничего не сделал.
function ask(text, { ok = tr('common.yes'), danger = false } = {}) {
  return new Promise((resolve) => {
    const m = $('#ask-modal');
    $('#ask-text').textContent = text;
    const yes = $('#ask-yes'), no = $('#ask-no');
    yes.textContent = ok;
    yes.className = danger ? 'danger' : 'primary';
    const done = (v) => {
      m.classList.add('hidden');
      yes.onclick = no.onclick = m.onkeydown = null;
      resolve(v);
    };
    yes.onclick = () => done(true);
    no.onclick = () => done(false);
    m.onkeydown = (e) => { if (e.key === 'Escape') { e.stopPropagation(); done(false); } };
    m.classList.remove('hidden');
    no.focus();
  });
}

let toastTimer;
function toast(msg, ms = 3500) {
  const t = $('#toast');
  t.textContent = msg;
  t.classList.remove('hidden');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add('hidden'), ms);
}
async function call(cmd, args) {
  try { return await invoke(cmd, args); } catch (e) { toast(String(e), 6000); throw e; }
}

// ---------- вкладки ----------
// виды: обзор источников (во вкладках сверху) и «О программе»
function showView(name) {
  $$('.tab').forEach((b) => b.classList.toggle('active', b.dataset.tab === name));
  $$('.view').forEach((v) => v.classList.toggle('active', v.id === `tab-${name}`));
  if (name === 'about') loadAbout();
  if (typeof renderTabs === 'function') renderTabs();
}
$$('.tab').forEach((b) => b.addEventListener('click', () => showView(b.dataset.tab)));

// ---------- меню программы: настройки и «О программе» ----------
function menuOpen(on) {
  $('#app-menu').classList.toggle('hidden', !on);
  $('#menu-btn').setAttribute('aria-expanded', on ? 'true' : 'false');
}
$('#menu-btn').addEventListener('click', (e) => { e.stopPropagation(); menuOpen($('#app-menu').classList.contains('hidden')); });
document.addEventListener('mousedown', (e) => { if (!e.target.closest('#app-menu, #menu-btn')) menuOpen(false); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') menuOpen(false); });
$('#app-menu').addEventListener('click', (e) => {
  const b = e.target.closest('[data-menu]');
  if (!b) return;
  menuOpen(false);
  if (b.dataset.menu === 'about') showView('about');
  else if (b.dataset.menu === 'settings') settingsOpen();
});
function settingsOpen() {
  $('#set-lang').innerHTML = LANGS.map((l) => `<option value="${l.id}"${l.id === LANG ? ' selected' : ''}>${l.name}</option>`).join('');
  $('#set-theme').value = THEME;
  $('#settings-modal').classList.remove('hidden');
  $('#set-lang').focus();
  mcpRender();
}
// доступ для ИИ-агентов (MCP): переключатель и готовые настройки подключения
async function mcpRender() {
  let info = { enabled: false, exe: '' };
  try { info = await invoke('mcp_info'); } catch { /* старое ядро */ }
  $('#set-mcp').checked = info.enabled;
  $('#set-mcp-setup').classList.toggle('hidden', !info.enabled);
  const exe = info.exe || 'FileHunter';
  $('#mcp-cmd').textContent = `claude mcp add filehunter -- "${exe}" --mcp`;
  $('#mcp-json').textContent = JSON.stringify({ mcpServers: { filehunter: { command: exe, args: ['--mcp'] } } }, null, 2);
}
$('#set-mcp').addEventListener('change', async (e) => {
  try { await invoke('mcp_set', { enabled: e.target.checked }); } catch (er) { toast(String(er), 6000); }
  mcpRender();
});
$('#settings-modal').addEventListener('click', (e) => {
  const b = e.target.closest('[data-copy]');
  if (b) copyText($('#' + b.dataset.copy).textContent, tr('mcp.what'));
});
// агент просит скопировать — спрашиваем человека
listen('mcp-confirm', async ({ payload }) => {
  const ok = await ask(payload.text, { ok: tr('mcp.allow') });
  invoke('mcp_answer', { id: payload.id, ok }).catch(() => {});
});
const settingsClose = () => $('#settings-modal').classList.add('hidden');
$('#set-lang').addEventListener('change', (e) => setLang(e.target.value));
$('#set-theme').addEventListener('change', (e) => setTheme(e.target.value));
$('#set-close').addEventListener('click', settingsClose);
$('#settings-modal').addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); settingsClose(); } });
// язык сменили — перерисовать всё, что собирается из кода
window.addEventListener('langchange', () => {
  invoke('set_lang', { lang: LANG }).catch(() => {});
  if (typeof renderTabs === 'function') renderTabs();
  if (typeof renderSourceList === 'function' && Bz.sources) renderSourceList();
  if (Bz.data) { renderCrumbs(); renderBrowse(); renderDetails(); }
  if (typeof renderTasks === 'function') renderTasks();
  refreshStatus();
  if ($('#tab-about').classList.contains('active')) loadAbout();
  updRender();
});
applyI18n();
invoke('set_lang', { lang: LANG }).catch(() => {});

// ---------- статус ----------
async function refreshStatus() {
  const s = await invoke('status');
  $('#st-index').textContent = trn('status.index', s.entries);
  $('#st-watch').textContent = s.watching
    ? `${tr('status.watching')}${s.changes ? ` · ${tr('status.changes', { n: fmtNum(s.changes) })}` : ''}`
    : '';
  return s;
}

// ---------- источники: список слева, обзор папок справа ----------
const ICON = {
  fixed: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.5" y="4" width="13" height="8" rx="1.5" fill="none" stroke="currentColor"/><circle cx="12" cy="8" r=".9" fill="currentColor"/></svg>',
  removable: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="4.5" y="1.5" width="7" height="13" rx="1.5" fill="none" stroke="currentColor"/><path d="M6.5 4h3" stroke="currentColor"/></svg>',
  network: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="1.5" y="2.5" width="13" height="7" rx="1.5" fill="none" stroke="currentColor"/><path d="M8 9.5v3M3 13.5h10" stroke="currentColor"/></svg>',
  s3: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 12.5h7a3 3 0 0 0 .3-6A4 4 0 0 0 4 6.6a3 3 0 0 0 .5 5.9z" fill="none" stroke="currentColor"/></svg>',
  folder: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M1.5 4.5v8h13v-7H7.5l-1.5-1.5h-4.5z" fill="currentColor" opacity=".85"/></svg>',
  file: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.5 1.5h6l3 3v10h-9z M9.5 1.5v3h3" fill="none" stroke="currentColor"/></svg>',
};
const Bz = { sources: [], cur: null, dir: null, data: null, live: false, seq: 0, busy: false, scanning: null, scanFiles: 0, sort: { by: 'name', desc: false }, sel: -1, shown: 2000, rows: [] };
const RECENT = 15 * 60; // «подключён недавно» — 15 минут

// ---------- источники этого и других компьютеров ----------
// Ключ источника: свой — путь корня, чужой — «id устройства|путь корня».
const keyOf = (s) => (s.peer ? `${s.peer}|${s.root}` : s.root);
// Запрос к источнику: свой — напрямую, на другом компьютере — по сети через него.
function api(method, args, s = curSource()) {
  return s && s.peer ? invoke('peer_call', { peer: s.peer, method, args }) : invoke(method, args);
}
const PEER_CACHE = 'filecat.peerSources';
function peerCache() { try { return JSON.parse(localStorage.getItem(PEER_CACHE)) || {}; } catch { return {}; } }
const Pz = { info: { ready: false, peers: [] } };

async function fetchSources() {
  const local = (await invoke('sources')).map((s) => ({ ...s, key: s.root }));
  try { Pz.info = await invoke('p2p_info'); } catch { Pz.info = { ready: false, peers: [] }; }
  try { if (TK.open) renderTasks(); } catch { /* панель задач ещё не готова */ } // имена компьютеров в таблице задач
  const cache = peerCache();
  const remote = await Promise.all(Pz.info.peers.map(async (p) => {
    let list = null;
    if (p.online) {
      try { list = await invoke('peer_call', { peer: p.id, method: 'sources', args: null }); cache[p.id] = list; } catch { list = null; }
    }
    // устройство не в сети — показываем, что было видно в прошлый раз, но без доступа
    const online = !!list;
    return (list || cache[p.id] || []).map((s) => ({ ...s, peer: p.id, peerName: p.name, peerOnline: online, key: `${p.id}|${s.root}`, online: online && s.online }));
  }));
  try { localStorage.setItem(PEER_CACHE, JSON.stringify(cache)); } catch { /* недоступно */ }
  return [...local, ...remote.flat()];
}

async function loadSources() {
  Bz.sources = await fetchSources();
  if (!Bz.cur || !Bz.sources.some((s) => s.key === Bz.cur)) {
    Bz.cur = (Bz.sources.find((s) => s.indexed && !s.peer) || Bz.sources[0] || {}).key || null;
    Bz.dir = null;
  }
  renderSourceList();
  // пользователь как раз открывает папку — не перебиваем его переход
  if (Bz.busy) return Bz.sources;
  const s = curSource();
  if (Bz.live && Bz.data && s && s.seg != null && !s.other_disk) {
    // источник только что попал в индекс — остаёмся в той же папке, но уже по индексу
    const path = Bz.data.path;
    try { const [, dir] = await api('locate', { path }); await openDir(dir); return Bz.sources; } catch { /* папки нет в индексе */ }
  }
  await openDir(Bz.dir, true);
  return Bz.sources;
}

function sourceStatus(s) {
  const now = Date.now() / 1000;
  if (s.peer && !s.peerOnline) return `<em class="st off">${tr('src.peerOffline')}</em>`;
  if (!s.peer && Bz.scanning === s.root) return `<em class="st on">${Bz.scanFiles ? trn('src.scanningN', Bz.scanFiles) : tr('src.scanning')}</em>`;
  if (s.kind !== 's3' && s.kind !== 'folder' && !s.online) return `<em class="st off">${tr('src.offline')}${s.disconnected_at ? ' · ' + fmtDate(s.disconnected_at) : ''}</em>`;
  if (s.queued) return `<em class="st">${tr('src.queued')}</em>`;
  if (s.connected_at && now - s.connected_at < RECENT && s.kind !== 's3') return `<em class="st on">${tr('src.connectedAt', { time: fmtDate(s.connected_at).split(', ')[1] })}</em>`;
  if (!s.indexed) return `<em class="st">${tr('src.notIndexed')}</em>`;
  return '';
}

function renderSourceList() {
  const item = (s) => {
    const used = s.total ? Math.round(((s.total - s.free) / s.total) * 100) : 0;
    const hist = [s.connected_at && tr('src.connected', { date: fmtDate(s.connected_at) }), s.disconnected_at && tr('src.disconnected', { date: fmtDate(s.disconnected_at) })].filter(Boolean).join(' · ');
    return `<div role="button" tabindex="0" class="src-item${s.key === Bz.cur ? ' active' : ''}${s.online ? '' : ' offline'}" data-key="${esc(s.key)}" title="${esc(hist || s.root)}">
      <span class="ico">${ICON[s.kind] || ICON.fixed}</span>
      <span class="txt"><b>${esc(s.title)}</b>
        ${s.total ? `<span class="usage${used > 90 ? ' full' : ''}"><i style="width:${used}%"></i></span>
          <small>${tr('src.free', { free: fmtSize(s.free), total: fmtSize(s.total) })}</small>` : ''}
        ${s.kind === 's3' && s.indexed ? `<small>${fmtSize(s.bytes)} · ${trn('common.files', s.files)}</small>` : ''}
        ${sourceStatus(s)}</span>
    </div>`;
  };
  const mine = Bz.sources.filter((s) => !s.peer);
  let html = (Pz.info.peers.length ? `<div class="grp">${esc(Pz.info.name || tr('peer.thisComputer'))} <span class="muted">· ${tr('peer.thisOne')}</span></div>` : '') + mine.map(item).join('');
  // связь с другими компьютерами не запустилась — говорим почему, а не молча прячем устройства
  if (Pz.info.error) {
    html += `<p class="p2p-err" title="${esc(Pz.info.error)}">${tr('peer.p2pError', { error: esc(Pz.info.error) })}</p>`;
  }
  for (const p of Pz.info.peers) {
    const list = Bz.sources.filter((s) => s.peer === p.id);
    const dir = accessDir(p, list);
    html += `<div class="grp peer" data-peer="${esc(p.id)}" title="${esc(accessHint(p, list))}">
      <span class="dot ${p.online ? 'on' : ''}"></span><b>${esc(p.name)}</b><span class="dir-ico${dir === '—' ? ' none' : ''}">${dir}</span>${p.outdated ? `<span class="outdated">· ${tr('peer.outdated')}</span>` : ''}</div>` +
      (list.length ? list.map(item).join('') : `<p class="grp-empty muted">${p.online ? tr('peer.noSources') : tr('peer.noData')}</p>`);
  }
  $('#src-list').innerHTML = html;
}
$('#src-list').addEventListener('click', (e) => {
  const b = e.target.closest('.src-item');
  if (!b) return;
  Bz.cur = b.dataset.key;
  Bz.dir = null;
  Bz.live = false;
  Bz.data = null;
  renderSourceList();
  openDir(null);
});

const curSource = () => Bz.sources.find((s) => s.key === Bz.cur);

async function openDir(dir, keepSel) {
  const s = curSource();
  if (!s) { $('#b-list').innerHTML = `<p class="more muted">${tr('browse.noSources')}</p>`; return; }
  if (s.seg == null || s.other_disk) {
    // источника ещё нет в индексе (или под буквой другой диск) — показываем папку прямо
    // с диска или из S3, не дожидаясь сканирования
    if (s.peer && !s.peerOnline) {
      Bz.data = null;
      Bz.rows = [];
      $('#b-crumbs').innerHTML = `<span class="crumb cur">${esc(s.root)}</span>`;
      $('#b-list').innerHTML = `<div class="empty-state"><p>${tr('browse.peerOffline', { name: `<b>${esc(s.peerName)}</b>` })}</p><p class="muted">${tr('browse.peerOffline.hint')}</p></div>`;
      $('#b-foot').textContent = '';
      return;
    }
    if (s.online) return openLive(Bz.live && Bz.data ? Bz.data.path : s.root, keepSel);
    Bz.data = null;
    Bz.rows = [];
    $('#b-crumbs').innerHTML = `<span class="crumb cur">${esc(s.title)}</span>`;
    $('#b-list').innerHTML = `<div class="empty-state"><p>${tr('browse.offlineNotIndexed', { name: `<b>${esc(s.title)}</b>` })}</p></div>`;
    $('#b-foot').textContent = '';
    return;
  }
  Bz.live = false;
  let data;
  const my = ++Bz.seq;
  try { data = await api('browse', { seg: s.seg, dir, sort: Bz.sort.by, desc: Bz.sort.desc }, s); } catch (e) { if (my === Bz.seq) toast(String(e)); return; }
  // пока ждали ответа, пользователь ушёл в другую папку или источник — этот ответ устарел
  if (my !== Bz.seq || Bz.cur !== s.key) return;
  const same = Bz.data && Bz.dir === dir;
  Bz.data = data;
  Bz.dir = dir;
  if (!(keepSel && same)) { Bz.sel = -1; Bz.shown = 2000; $('#b-list').scrollTop = 0; }
  afterNav(!same);
  if (Fz.query) runFind(); else renderBrowse();
  // открытую папку сверяем с диском или S3: изменения, пока программа была закрыта, тоже подтянутся
  if (!keepSel) refreshS3(s, dir);
}

// S3 не сообщает об изменениях: открытую папку сверяем с хранилищем одним запросом
async function refreshS3(s, dir) {
  let n = 0;
  try { n = await api('s3_refresh', { seg: s.seg, dir }, s); } catch { return; }
  if (n > 0 && Bz.cur === s.key && Bz.dir === dir && !Bz.live && !Bz.busy) {
    await openDir(dir, true);
    $('#b-foot').textContent += ` · ${tr(s.kind === 's3' ? 'browse.syncedS3' : 'browse.syncedDisk', { n: fmtNum(n) })}`;
  }
}

// папка, прочитанная напрямую: навигация по пути, а не по записям индекса
async function openLive(path, keepSel) {
  const s = curSource();
  let data;
  const my = ++Bz.seq;
  Bz.busy = true;
  if (!keepSel) $('#b-foot').textContent = tr('browse.reading');
  try { data = await api('browse_live', { root: s.root, path }, s); } catch (e) {
    if (my !== Bz.seq) return;
    Bz.busy = false;
    $('#b-list').innerHTML = `<div class="empty-state"><p>${tr('browse.openFailed')}</p><p class="muted">${esc(String(e))}</p>
      ${s.kind === 's3' ? `<div class="btns"><button data-s3edit="${esc(s.title)}">${tr('s3.edit')}</button></div>` : ''}</div>`;
    $('#b-foot').textContent = '';
    return;
  }
  if (my !== Bz.seq || Bz.cur !== s.key) return;
  Bz.busy = false;
  const same = Bz.live && Bz.data && Bz.data.path === data.path;
  Bz.live = true;
  Bz.data = { ...data, size: 0, files: 0, crumbs: [] };
  if (!(keepSel && same)) { Bz.sel = -1; Bz.shown = 2000; $('#b-list').scrollTop = 0; }
  afterNav(!same);
  if (Fz.query) runFind(); else renderBrowse();
}

function goUp() {
  if (Bz.live) { if (Bz.data?.parent) openLive(Bz.data.parent); return; }
  const c = Bz.data?.crumbs;
  if (c && c.length > 1) openDir(c[c.length - 2].idx ?? null);
}

// ---------- одно поле, как поиск в GitHub ----------
// Слева в поле — путь (части кликабельны), справа — ввод: имя для поиска в папке
// и вложенных, или путь для перехода (Enter). Backspace в пустом поле — на уровень выше.
// Щелчок в пустое поле — недавние папки.
const sepOf = (p) => (p.includes('://') || !p.includes('\\') ? '/' : '\\');
const looksLikePath = (q) => /^[a-zA-Z]:[\\/]?$|^[a-zA-Z]:[\\/]|^s3:\/\/|^\\\\|^\/(Users|Volumes|Applications|home|media|mnt)?(\/|$)/.test(q);

// части пути до текущей папки: [{label, idx}] для индекса или [{label, path}] для чтения напрямую
function crumbParts() {
  const s = curSource();
  if (!s) return [];
  // корень показываем путём (C: или s3://имя), а не названием диска — диск и так выбран слева
  // «C:\\» → «C:», а корень macOS «/» так и остаётся «/»
  const root = s.root.length > 1 ? s.root.replace(/[\\/]$/, '') : s.root;
  if (!Bz.data) return [{ label: root, path: s.root }];
  if (!Bz.live) return Bz.data.crumbs.map((c, i) => ({ label: i === 0 ? root : c.name, idx: c.idx ?? null }));
  const sep = sepOf(s.root);
  const parts = [{ label: root, path: s.root }];
  let acc = root;
  for (const r of Bz.data.path.slice(root.length).split(/[\\/]/).filter(Boolean)) { acc += sep + r; parts.push({ label: r, path: acc }); }
  return parts;
}

function renderCrumbs() {
  const parts = crumbParts();
  const sep = sepOf(curSource()?.root || '');
  // путь текстом, как токены в поиске GitHub: части подсвечены и кликабельны
  $('#b-crumbs').innerHTML = parts.map((p, i) => {
    const data = p.path != null ? `data-path="${esc(p.path)}"` : `data-idx="${p.idx ?? ''}"`;
    return `<span class="crumb${i === parts.length - 1 ? ' cur' : ''}" ${data} title="${tr('browse.open', { name: esc(p.label) })}">${esc(p.label)}</span><span class="sep">${esc(sep)}</span>`;
  }).join('');
  // длинный путь: видна его конечная часть
  $('#b-crumbs').scrollLeft = $('#b-crumbs').scrollWidth;
  $('#b-q').placeholder = tr('browse.q.placeholder');
}
let crumbTimer = null;
$('#b-crumbs').addEventListener('click', (e) => {
  const b = e.target.closest('.crumb');
  if (!b) { $('#b-q').focus(); return; }
  // второй щелчок двойного — это редактирование пути, а не переход
  clearTimeout(crumbTimer);
  if (e.detail > 1) return;
  crumbTimer = setTimeout(() => {
    clearQuery();
    if (b.dataset.path != null) openLive(b.dataset.path);
    else openDir(b.dataset.idx === '' ? null : +b.dataset.idx);
  }, 220);
});
// путь текущей папки текстом, как его пишет система: C:\Windows\System32, /Users/me, s3://bucket/dir
function curPath() {
  const s = curSource();
  const parts = crumbParts();
  if (!s || !parts.length) return '';
  if (parts.length === 1) return s.root;
  const sep = sepOf(s.root);
  const root = parts[0].label;
  return (root.endsWith(sep) ? root : root + sep) + parts.slice(1).map((p) => p.label).join(sep);
}
async function copyText(text, what) {
  try { await window.__TAURI__.clipboardManager.writeText(text); } catch { try { await navigator.clipboard.writeText(text); } catch { return; } }
  toast(tr('common.copied', { what, text }));
}
// двойной щелчок по пути — путь становится обычным текстом (выделен): скопировать, поправить, вставить другой
const Ed = { on: false, orig: '' };
function editPath() {
  const p = curPath();
  if (!p) return;
  Ed.on = true;
  Ed.orig = p;
  $('#omni').classList.add('editing');
  const q = $('#b-q');
  q.value = p;
  q.focus();
  q.select();
}
function endEdit() {
  if (!Ed.on) return;
  Ed.on = false;
  $('#omni').classList.remove('editing');
  if ($('#b-q').value.trim() === Ed.orig || looksLikePath($('#b-q').value.trim())) $('#b-q').value = '';
}
$('#b-crumbs').addEventListener('dblclick', (e) => { e.preventDefault(); editPath(); });
// подсказка переводится вместе с разметкой (applyI18n)
$('#b-crumbs').dataset.i18nTitle = 'browse.crumbs.title';
$('#b-crumbs').title = tr('browse.crumbs.title');

// щелчок по свободному месту поля — фокус на ввод
$('#omni').addEventListener('mousedown', (e) => {
  if (e.target.id === 'omni') { e.preventDefault(); $('#b-q').focus(); }
});

// переход по пути (введённому или из истории)
async function goPath(path, peer = curSource()?.peer) {
  const onDevice = (x) => (x.peer || null) === (peer || null);
  try {
    const [seg, dir] = peer ? await invoke('peer_call', { peer, method: 'locate', args: { path } }) : await invoke('locate', { path });
    const s = Bz.sources.find((x) => onDevice(x) && x.seg === seg && !x.other_disk);
    if (s) {
      Bz.cur = s.key;
      renderSourceList();
      await openDir(dir);
      return true;
    }
  } catch { /* в индексе такой папки нет — откроем напрямую */ }
  const low = path.toLowerCase();
  const s = Bz.sources
    .filter((x) => onDevice(x) && x.online && low.startsWith(x.root.toLowerCase().replace(/[\\/]$/, '')))
    .sort((a, b) => b.root.length - a.root.length)[0];
  if (!s) { toast(tr('browse.notFound', { path })); return false; }
  Bz.cur = s.key;
  renderSourceList();
  await openLive(path);
  return true;
}

// ---------- история путей и поисков, подсказки как в Google ----------
const HIST_KEY = 'filecat.history';
const QHIST_KEY = 'filecat.queries';
function loadKey(k) { try { return JSON.parse(localStorage.getItem(k)) || []; } catch { return []; } }
function saveKey(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* хранилище недоступно */ } }
const loadHist = () => loadKey(HIST_KEY);
// в истории помним и устройство: C:\ этого компьютера и C:\ ноутбука — разные места
function pushHist(path) {
  const peer = curSource()?.peer || null;
  const h = loadHist().filter((x) => !(x.path.toLowerCase() === path.toLowerCase() && (x.peer || null) === peer));
  h.unshift({ path, peer, t: Date.now() });
  saveKey(HIST_KEY, h.slice(0, 50));
}
// запрос запоминаем вместе с папкой, где искали
function pushQuery(q) {
  if (!q || looksLikePath(q) || !Bz.data) return;
  const where = Bz.data.path;
  const peer = curSource()?.peer || null;
  const h = loadKey(QHIST_KEY).filter((x) => !(x.q.toLowerCase() === q.toLowerCase() && x.where === where));
  h.unshift({ q, where, peer, t: Date.now() });
  saveKey(QHIST_KEY, h.slice(0, 50));
}
function forget(kind, key) {
  if (kind === 'path') saveKey(HIST_KEY, loadHist().filter((x) => x.path !== key));
  else saveKey(QHIST_KEY, loadKey(QHIST_KEY).filter((x) => `${x.q}\n${x.where}` !== key));
}

const D_ICON = {
  path: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M1.5 4.5v8h13v-7H7.5l-1.5-1.5h-4.5z" fill="none" stroke="currentColor"/></svg>',
  query: '<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="8" cy="8" r="6" fill="none" stroke="currentColor"/><path d="M8 4.5V8l2.5 1.5" fill="none" stroke="currentColor"/></svg>',
  go: '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3 8h9M8.5 4.5 12 8l-3.5 3.5" fill="none" stroke="currentColor"/></svg>',
  dup: '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="2" y="2" width="8.5" height="8.5" rx="1.5" fill="none" stroke="currentColor"/><rect x="5.5" y="5.5" width="8.5" height="8.5" rx="1.5" fill="none" stroke="currentColor"/></svg>',
};
const Dd = { items: [], sel: -1 };

// подсказки: пустое поле — недавнее; ввод — подходящее из истории путей и поисков
function suggestions(q) {
  const low = q.toLowerCase();
  const peerName = (id) => (id ? Pz.info.peers.find((p) => p.id === id)?.name || tr('peer.other') : '');
  const paths = loadHist().map((x) => ({ kind: 'path', path: x.path, peer: x.peer || null, dev: peerName(x.peer), key: x.path }));
  const queries = loadKey(QHIST_KEY).map((x) => ({ kind: 'query', q: x.q, where: x.where, peer: x.peer || null, dev: peerName(x.peer), key: `${x.q}\n${x.where}` }));
  if (!q) return [...queries.slice(0, 5), ...paths.slice(0, 8)];
  // «дуб», «dup» — найти дубликаты здесь или на всех дисках
  if (/^(dup|dub|дуб|копи)/i.test(q) && !dupParse(q)) {
    return [
      { kind: 'dup', tok: 'dup:size', label: tr('dup.sug.size'), hint: tr('dup.sug.size.hint') },
      { kind: 'dup', tok: 'dup:', label: tr('dup.sug.here', { folder: folderLabel() || tr('common.thisFolder') }), hint: tr('dup.sug.here.hint') },
      { kind: 'dup', tok: 'dup:all', label: tr('dup.sug.all'), hint: tr('dup.sug.all.hint') },
    ];
  }
  if (looksLikePath(q)) {
    return [{ kind: 'go', path: q }, ...paths.filter((x) => x.path.toLowerCase().startsWith(low) && x.path.toLowerCase() !== low).slice(0, 8)];
  }
  const nameOf = (p) => (baseName(p.replace(/[\\/]$/, '')) || p).toLowerCase();
  return [
    ...queries.filter((x) => x.q.toLowerCase().includes(low) && x.q.toLowerCase() !== low).slice(0, 5),
    ...paths.filter((x) => nameOf(x.path).includes(low)).slice(0, 5),
  ];
}

function renderDrop() {
  const q = $('#b-q').value.trim();
  const drop = $('#omni-drop');
  Dd.items = document.activeElement === $('#b-q') ? suggestions(q) : [];
  Dd.sel = -1;
  if (!Dd.items.length) { drop.classList.add('hidden'); return; }
  const here = Bz.data?.path;
  drop.innerHTML = Dd.items.map((x, i) => {
    const icon = D_ICON[x.kind] || D_ICON.dup;
    const body = x.kind === 'dup' ? `<b>${esc(x.label)}</b><small>${esc(x.hint)}</small>`
      : x.kind === 'go' ? tr('drop.go', { path: `<b>${esc(x.path)}</b>` })
      : x.kind === 'path' ? `<b>${esc(baseName(x.path.replace(/[\\/]$/, '')) || x.path)}</b><small>${x.dev ? esc(x.dev) + ' · ' : ''}${esc(x.path)}</small>`
      : `<b>${esc(x.q)}</b><small>${x.where === here ? tr('drop.inThisFolder') : tr('drop.in', { where: esc(x.where) })}</small>`;
    const del = x.kind === 'go' || x.kind === 'dup' ? '' : `<span class="del" data-del="${i}" title="${tr('drop.forget')}">×</span>`;
    return `<div class="sug" data-i="${i}"><span class="ico">${icon}</span><span class="txt">${body}</span>${del}</div>`;
  }).join('') + (q ? '' : `<div class="drop-hint">${tr('drop.hint')}</div>`);
  drop.classList.remove('hidden');
}
function markSug() {
  $$('#omni-drop .sug').forEach((el) => el.classList.toggle('sel', +el.dataset.i === Dd.sel));
  $(`#omni-drop .sug[data-i="${Dd.sel}"]`)?.scrollIntoView({ block: 'nearest' });
}
async function pickSug(i) {
  const x = Dd.items[i];
  if (!x) return;
  $('#omni-drop').classList.add('hidden');
  if (x.kind === 'dup') {
    $('#b-q').value = x.tok;
    pushQuery($('#b-q').value);
    runFind();
    return;
  }
  if (x.kind === 'query') {
    // прошлый поиск: в ту же папку, тот же запрос
    if (Bz.data?.path !== x.where || (curSource()?.peer || null) !== x.peer) { clearQuery(); await goPath(x.where, x.peer); }
    $('#b-q').value = x.q;
    runFind();
    return;
  }
  clearQuery();
  $('#b-q').blur();
  goPath(x.path, x.peer);
}
$('#omni-drop').addEventListener('mousedown', (e) => {
  e.preventDefault(); // фокус остаётся в поле
  const d = e.target.closest('[data-del]');
  if (d) { const x = Dd.items[+d.dataset.del]; forget(x.kind, x.key); renderDrop(); return; }
  const s = e.target.closest('.sug');
  if (s) pickSug(+s.dataset.i);
});
$('#b-q').addEventListener('focus', renderDrop);
$('#b-q').addEventListener('blur', () => { $('#omni-drop').classList.add('hidden'); endEdit(); });

// ---------- поиск по имени в открытой папке и вложенных ----------
const Fz = { query: '', data: null, timer: null, seq: 0, everywhere: false };
// поиск по смыслу: off — ещё ничего не посчитано, download, indexing, ready, error
const Sem = { state: 'off', done: 0, total: 0, error: '' };
const semText = () => Sem.state === 'download' ? tr('sem.download')
  : Sem.state === 'indexing' ? tr('sem.indexing', { pct: Sem.total ? Math.floor((Sem.done / Sem.total) * 100) : 0, done: fmtNum(Sem.done), total: fmtNum(Sem.total) })
  : Sem.state === 'ready' ? trn('sem.ready', Sem.done)
  : Sem.state === 'error' ? tr('sem.error', { error: Sem.error }) : tr('sem.off');
invoke('semantic_status').then((x) => Object.assign(Sem, x)).catch(() => {});
listen('semantic-status', ({ payload }) => {
  Object.assign(Sem, payload);
  if ($('#a-sem')) $('#a-sem').textContent = semText();
  if (Fz.query && Fz.data && !Fz.data.dups) renderBrowse();
});

// по смыслу — дополнением к найденному по имени, без повторов
async function semFind(q, my) {
  if (Sem.state === 'off' || !Sem.done) return;
  const s = curSource();
  if (!Fz.everywhere && (!s || s.peer || s.seg == null || Bz.live)) return;
  try {
    const r = await invoke('semantic_search', { query: q, all: Fz.everywhere, seg: Fz.everywhere ? null : s.seg, dir: Fz.everywhere ? null : Bz.dir ?? null });
    if (my !== Fz.seq || !Fz.data || Fz.data.dups) return;
    const have = new Set(Fz.data.items.map((it) => itemPath(it).toLowerCase()));
    Fz.data.sem = r.items.filter((it) => !have.has(it.full.toLowerCase()));
    renderBrowse();
  } catch (e) { devLog(`по смыслу: ${e}`); }
}
try { Fz.everywhere = localStorage.getItem('filecat.searchAll') === '1'; } catch { /* недоступно */ }
function setEverywhere(on) {
  Fz.everywhere = on;
  try { localStorage.setItem('filecat.searchAll', on ? '1' : '0'); } catch { /* недоступно */ }
  runFind();
}
const folderLabel = () => { const p = crumbParts(); return p.length ? p[p.length - 1].label : ''; };

function clearQuery() {
  $('#b-q').value = '';
  Fz.query = '';
  Fz.data = null;
}

async function runFind() {
  const q = $('#b-q').value.trim();
  Fz.query = looksLikePath(q) ? '' : q;
  if (!Fz.query) { Fz.data = null; dupStop(); if (Bz.data) renderBrowse(); return; }
  const dp = dupParse(Fz.query);
  if (dp) { runDups(dp); return; }
  dupStop();
  const s = curSource();
  const my = ++Fz.seq;
  if (Fz.everywhere) {
    const r = await searchEverywhere(q);
    if (my !== Fz.seq) return;
    Fz.data = r;
  } else if (Bz.live || !s || s.seg == null) {
    // источник ещё не в индексе — ищем только среди того, что видно в папке
    const words = q.toLowerCase().split(/\s+/).filter((w) => w && !w.includes(':'));
    const items = Bz.data.items.filter((it) => words.every((w) => it.name.toLowerCase().includes(w)));
    Fz.data = { items, total: items.length, ms: 0, onlyHere: true };
  } else {
    try { Fz.data = await api('search_folder', { seg: s.seg, dir: Bz.dir, query: q }, s); } catch (e) {
      if (my === Fz.seq) $('#b-foot').textContent = String(e);
      return;
    }
    if (my !== Fz.seq) return;
  }
  Bz.sel = -1;
  Bz.shown = 2000;
  $('#b-list').scrollTop = 0;
  renderBrowse();
  tabCapture();
  semFind(q, my);
}
// Везде: все источники этого компьютера одним запросом и, параллельно, подключённые
// компьютеры — каждый ищет у себя по тому, что открыл. Строки знают свой полный путь.
async function searchEverywhere(q) {
  const t = performance.now();
  // свои источники и копии индекса других компьютеров — одним вызовом
  const local = await invoke('search_all', { query: q }).catch((e) => ({ items: [], total: 0, covered: [], error: String(e) }));
  for (const it of local.items) {
    if (!it.peer) continue;
    const p = Pz.info.peers.find((x) => x.id === it.peer);
    // компьютер не в сети — показываем по копии, с датой
    if (!p?.online && it.asof) it.source += ` · ${tr('search.asOf', { date: fmtDate(it.asof) })}`;
  }
  const covered = new Set(local.covered || []);
  // остальных (старые версии без копий) спрашиваем по сети
  const peers = Bz.sources.filter((x) => x.peer && x.online && x.seg != null && !covered.has(x.peer)).map(async (x) => {
    try {
      const r = await api('search_folder', { seg: x.seg, dir: null, query: q }, x);
      const root = x.root.replace(/[\\/]+$/, '');
      const sep = sepOf(x.root);
      return {
        total: r.total,
        items: r.items.map((it) => {
          const place = it.place ? root + sep + it.place : x.root;
          return { ...it, seg: x.seg, peer: x.peer, full: joinPath(place, it.name), place, source: `${x.peerName} · ${x.title}` };
        }),
      };
    } catch { return { items: [], total: 0, failed: x.peerName }; }
  });
  const parts = [local, ...(await Promise.all(peers))];
  return {
    everywhere: true,
    items: parts.flatMap((p) => p.items),
    total: parts.reduce((a, p) => a + (p.total || 0), 0),
    ms: performance.now() - t,
    failed: [...new Set(parts.filter((p) => p.failed).map((p) => p.failed))],
    error: parts[0].error || '',
  };
}

$('#b-q').addEventListener('input', () => {
  renderDrop();
  clearTimeout(Fz.timer);
  Fz.timer = setTimeout(runFind, 200);
});
$('#b-q').addEventListener('keydown', async (e) => {
  const q = $('#b-q').value.trim();
  const open = !$('#omni-drop').classList.contains('hidden') && Dd.items.length;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault();
    if (open) {
      // стрелки ходят по подсказкам, как в Google
      const n = Dd.items.length;
      Dd.sel = e.key === 'ArrowDown' ? (Dd.sel + 1) % n : (Dd.sel <= 0 ? n - 1 : Dd.sel - 1);
      markSug();
    } else if (e.key === 'ArrowDown') {
      $('#b-q').blur();
      $('#b-list').focus();
      selectRow(0);
    }
  } else if (e.key === 'Enter') {
    e.preventDefault();
    if (open && Dd.sel >= 0) { pickSug(Dd.sel); return; }
    $('#omni-drop').classList.add('hidden');
    if (looksLikePath(q)) { clearQuery(); $('#b-q').blur(); await goPath(q); }
    else { clearTimeout(Fz.timer); pushQuery(q); runFind(); }
  } else if ((e.ctrlKey || e.metaKey) && (e.key === 'c' || e.key === 'с') && !$('#b-q').value) {
    e.preventDefault();
    const p = curPath();
    if (p) copyText(p, tr('common.path'));
  } else if (e.key === 'Escape') {
    if (Ed.on) { $('#b-q').value = ''; endEdit(); $('#b-q').blur(); return; }
    if (open) $('#omni-drop').classList.add('hidden');
    else if (q) { clearQuery(); renderBrowse(); }
    else $('#b-q').blur();
  } else if (e.key === 'Delete' && open && Dd.sel >= 0 && Dd.items[Dd.sel].kind !== 'go') {
    // Shift+Delete в Chrome, здесь просто Delete — убрать подсказку из истории
    e.preventDefault();
    const x = Dd.items[Dd.sel];
    forget(x.kind, x.key);
    renderDrop();
  } else if (e.key === 'Backspace' && !$('#b-q').value) {
    // как удаление последнего токена в GitHub: на уровень выше
    e.preventDefault();
    goUp();
  }
});

// ---------- дубликаты: dup: в поле поиска ----------
// «dup:size» — мгновенно, из индекса, на всех источниках: копии папок и файлы одинакового
// размера (кандидаты, не проверено). «dup:» — проверенные копии в открытой папке и вложенных,
// «dup:all» — на всех дисках и в S3. Остальное в запросе — обычные фильтры (ext:, size:, имя).
// Что делать с найденным, решает человек: строки — обычные файлы и папки.
const Dz = { id: 0, key: '', groups: new Map(), plan: null, progress: null, done: false, error: '', all: false, bysize: false, info: null, timer: null, started: 0 };
function dupParse(q) {
  let on = false, all = false, bysize = false;
  const rest = q.split(/\s+/).filter((t) => {
    const m = /^(dup|dub|дубли|дубликаты|копии):(all|все|всё|size|размер)?$/i.exec(t);
    if (!m) return true;
    on = true;
    const w = (m[2] || '').toLowerCase();
    if (w === 'size' || w === 'размер') bysize = true;
    else if (w) all = true;
    return false;
  });
  return on ? { all: all || bysize, bysize, rest: rest.join(' ') } : null;
}
function dupStop() {
  if (Dz.id && !Dz.done && !Dz.bysize) invoke('dup_stop').catch(() => {});
  Dz.key = '';
}
// мелочь — миллионы совпадений по размеру и почти ноль места: от 1 МБ, если размер не задан явно
const dupMin = (rest) => (/(^|\s)size:/i.test(rest) ? 1 : 1 << 20);

async function runDups(p) {
  const s = curSource();
  const key = `${p.bysize}|${p.all}|${p.rest}|${keyOf(s || {})}|${p.all ? '' : Bz.dir}`;
  if (key === Dz.key && Fz.data?.dups) { renderBrowse(); return; }
  if (!Dz.done && Dz.id && !Dz.bysize) invoke('dup_stop').catch(() => {});
  Dz.key = key;
  Object.assign(Dz, { id: Date.now(), groups: new Map(), plan: null, progress: null, done: false, error: '', info: null, all: p.all, bysize: p.bysize, started: Date.now() });
  Fz.data = { dups: true };
  Bz.sel = -1;
  Bz.shown = 2000;
  $('#b-list').scrollTop = 0;
  if (p.bysize) {
    renderBrowse();
    try {
      const r = await invoke('dup_sizes', { query: p.rest, minSize: dupMin(p.rest) });
      if (Dz.key !== key) return;
      for (const g of r.groups) Dz.groups.set(g.id, { ...g, exact: false });
      Dz.info = r;
    } catch (e) { Dz.error = String(e); }
    Dz.done = true;
    renderBrowse();
    return;
  }
  if (!p.all && s?.peer) { Dz.done = true; Dz.error = tr('dup.err.peer'); renderBrowse(); return; }
  if (!p.all && (!s || s.seg == null)) { Dz.done = true; Dz.error = tr('dup.err.indexing'); renderBrowse(); return; }
  renderBrowse();
  try {
    await invoke('dup_search', { id: Dz.id, scopes: p.all ? [] : [{ seg: s.seg, dir: Bz.dir }], all: p.all, query: p.rest, minSize: dupMin(p.rest), gentle: $('#s-gentle').checked });
  } catch (e) { Dz.done = true; Dz.error = String(e); renderBrowse(); }
}
// перерисовка не чаще пары раз в секунду: группы приходят пачками
function dupSoon() {
  if (Dz.timer) return;
  Dz.timer = setTimeout(() => { Dz.timer = null; if (Fz.data?.dups) renderBrowse(); }, 400);
}
listen('dup-plan', ({ payload }) => { if (payload.id === Dz.id) { Dz.plan = payload; dupSoon(); } });
listen('dup-groups', ({ payload }) => {
  if (payload.id !== Dz.id) return;
  for (const x of payload.items) {
    if (x.group) Dz.groups.set(x.group.id, x.group);
    else if (x.drop) Dz.groups.delete(x.drop);
  }
  dupSoon();
});
listen('dup-progress', ({ payload }) => {
  if (payload.id !== Dz.id) return;
  Dz.progress = payload.p;
  dupSoon();
});
listen('dup-done', ({ payload }) => {
  if (payload.id !== Dz.id) return;
  Dz.done = true;
  Dz.error = payload.error || '';
  if (Fz.data?.dups) renderBrowse();
});
const wastedOf = (g) => g.size * (g.files.length - 1);

function renderDups() {
  $$('#b-head [data-bsort]').forEach((b) => { b.classList.remove('sorted'); b.dataset.arrow = ''; });
  $('#b-head [data-bsort="files"]').textContent = tr('col.where');
  const groups = [...Dz.groups.values()].sort((a, b) => wastedOf(b) - wastedOf(a) || b.size - a.size);
  // строки — файлы и папки групп подряд; заголовки групп — не строки, их не выделить
  Bz.rows = [];
  const parts = [];
  for (const g of groups) {
    if (Bz.rows.length >= Bz.shown) break;
    const n = g.files.length;
    const what = g.is_dir ? `${trn('dup.dirs', n, { size: fmtSize(g.size) })}, ${trn('dup.eachFiles', g.files_inside)}` : trn('dup.copies', n, { size: fmtSize(g.size) });
    const badge = Dz.bysize ? `<span class="dup-badge maybe" title="${tr('dup.badge.size.title')}">${tr('dup.badge.size')}</span>`
      : g.exact ? `<span class="dup-badge ok" title="${tr('dup.badge.exact.title')}">${tr('dup.badge.exact')}</span>`
      : `<span class="dup-badge maybe" title="${tr('dup.badge.sample.title')}">${tr('dup.badge.sample')}</span>`;
    parts.push(`<div class="dup-head"><span class="ico ${g.is_dir ? 'dir' : ''}">${g.is_dir ? ICON.folder : ICON.file}</span><b>${esc(g.files[0].name)}</b><span class="muted">${what}</span>${badge}${g.files.some((f) => f.cloud) ? `<span class="dup-badge">${tr('dup.badge.s3')}</span>` : ''}<span class="spacer"></span><span class="dup-free">−${fmtSize(wastedOf(g))}</span></div>`);
    for (const f of g.files) {
      const i = Bz.rows.length;
      Bz.rows.push({ ...f, is_dir: !!g.is_dir, size: g.size, files: g.is_dir ? g.files_inside : 1, group: g.id });
      parts.push(`<div class="brow item dup${Bz.sels.has(i) || (!Bz.sels.size && i === Bz.sel) ? ' sel' : ''}" data-i="${i}">
        <span class="ico ${g.is_dir ? 'dir' : ''}">${g.is_dir ? ICON.folder : ICON.file}</span>
        <span class="name" title="${esc(f.full)}">${esc(f.name)} <small class="place">${esc(f.place)}</small></span>
        <span class="muted">${f.mtime ? fmtDate(f.mtime) : ''}</span>
        <span class="muted">${g.is_dir ? tr('common.folder') : typeOf({ name: f.name })}</span>
        <span class="num">${fmtSize(g.size)}</span>
        <span class="num muted" title="${esc(f.source)}">${esc(f.source)}</span></div>`);
    }
  }
  restoreSel();
  const total = groups.reduce((a, g) => a + g.files.length, 0);
  const more = total > Bz.rows.length ? `<div class="more"><button id="b-more">${tr('common.showMore', { n: fmtNum(total - Bz.rows.length) })}</button></div>` : '';
  const empty = Dz.done
    ? `<p class="more muted">${Dz.error ? esc(Dz.error) : Dz.bysize ? tr('dup.none.size') : tr('dup.none')}</p>`
    : `<p class="more muted">${Dz.bysize ? tr('dup.grouping') : tr('dup.searching')}</p>`;
  $('#b-list').innerHTML = parts.length ? parts.join('') + more : empty;
  $('#b-more')?.addEventListener('click', () => { Bz.shown += 3000; renderBrowse(); });

  // низ: что найдено, ход поиска, переключатели
  const freed = groups.reduce((a, g) => a + wastedOf(g), 0);
  const min = dupMin(dupParse(Fz.query)?.rest || '') > 1 ? ` · ${tr('dup.min')}` : '';
  let text;
  if (Dz.bysize) {
    const r = Dz.info;
    text = r ? `${tr('dup.sizeSummary', { dirs: fmtNum(r.dir_groups), files: fmtNum(r.file_groups), size: fmtSize(r.potential) })} · ${tr('common.ms', { n: r.ms.toFixed(0) })}${r.total > r.groups.length ? ` · ${tr('dup.shownLargest', { n: fmtNum(r.groups.length) })}` : ''}${min}`
      : tr('dup.grouping');
  } else {
    const p = Dz.progress, pl = Dz.plan;
    const found = groups.length ? tr('dup.found', { n: fmtNum(groups.length), exact: fmtNum(groups.filter((g) => g.exact).length), size: fmtSize(freed) }) : tr('dup.notYet');
    const prog = Dz.done ? (Dz.error ? ` · ${esc(Dz.error)}` : ` · ${tr('dup.checkedIn', { n: Math.max(1, Math.round((Date.now() - Dz.started) / 1000)) })}`)
      : p ? ` · ${tr('dup.progress', { done: fmtNum(p.files_done), total: fmtNum(p.files), size: fmtSize(p.bytes_read) })}` : pl ? ` · ${tr('dup.candidates', { n: fmtNum(pl.files) })}` : ` · ${tr('dup.picking')}`;
    text = `${Dz.all ? tr('dup.titleAll') : tr('dup.titleHere', { folder: esc(folderLabel()) })}: ${found}${prog}${min}`;
  }
  const btn = (k, t) => `<button data-dup="${k}">${t}</button>`;
  const switches = [
    !Dz.bysize && btn('size', tr('dup.btn.size')),
    (Dz.bysize || !Dz.all) && btn('all', tr('dup.btn.all')),
    !Dz.bysize && Dz.all && btn('here', tr('dup.btn.here', { folder: esc(folderLabel()) })),
    !Dz.done && !Dz.bysize && btn('stop', tr('common.stop')),
  ].filter(Boolean).join('');
  $('#b-foot').innerHTML = `${text} ${switches}`;
}
$('#b-foot').addEventListener('click', (e) => {
  const sc = e.target.closest('[data-scope]');
  if (sc) { setEverywhere(sc.dataset.scope === 'all'); return; }
  const b = e.target.closest('[data-dup]');
  if (!b) return;
  if (b.dataset.dup === 'stop') { invoke('dup_stop').catch(() => {}); return; }
  const p = dupParse(Fz.query) || { rest: '' };
  const tok = { size: 'dup:size', all: 'dup:all', here: 'dup:' }[b.dataset.dup];
  $('#b-q').value = [tok, p.rest].filter(Boolean).join(' ');
  runFind();
});

// после каждого перехода: путь в поле, история; запрос сбрасываем при смене папки
function afterNav(changed) {
  if (changed) { Bz.sels = new Set(); Bz.selKeys = new Set(); Bz.anchorKey = null; }
  if (changed && Fz.query) clearQuery();
  renderCrumbs();
  setTimeout(tabCapture);
  if (changed) setTimeout(renderDetails);
  if (Bz.data) pushHist(Bz.data.path);
}

const extOf = (n) => { const i = n.lastIndexOf('.'); return i > 0 ? n.slice(i + 1).toLowerCase() : ''; };
const typeOf = (it) => (it.is_dir ? tr('common.folder') : extOf(it.name) ? extOf(it.name).toUpperCase() : tr('common.file'));
const bKey = {
  name: (it) => it.name.toLowerCase(),
  mtime: (it) => it.mtime,
  type: (it) => typeOf(it).toLowerCase(),
  size: (it) => it.size,
  files: (it) => it.files,
};

function renderBrowse() {
  if (Fz.query && Fz.data?.dups) { renderDups(); return; }
  $$('#b-head [data-bsort]').forEach((b) => {
    const on = b.dataset.bsort === Bz.sort.by;
    b.classList.toggle('sorted', on);
    b.dataset.arrow = on ? (Bz.sort.desc ? '↓' : '↑') : '';
  });
  const k = bKey[Bz.sort.by], d = Bz.sort.desc ? -1 : 1;
  const finding = !!(Fz.query && Fz.data);
  const wide = finding && Fz.data.everywhere;
  $('#b-head [data-bsort="files"]').textContent = wide ? tr('col.where') : tr('col.files');
  const src = finding ? Fz.data.items : Bz.data.items;
  // папки всегда сверху, как в Проводнике
  Bz.rows = src.slice().sort((a, b) => (b.is_dir - a.is_dir) || d * cmp(k(a), k(b)) || cmp(a.name.toLowerCase(), b.name.toLowerCase()));
  const byName = Bz.rows.length;
  if (finding && Fz.data.sem?.length) Bz.rows = Bz.rows.concat(Fz.data.sem);
  restoreSel();
  const rows = Bz.rows.slice(0, Bz.shown);
  $('#b-list').innerHTML = rows.length
    ? rows.map((it, i) => `${i === byName ? `<div class="sem-head">${tr('sem.head')} <span class="muted">— ${tr('sem.head.hint')}</span></div>` : ''}<div class="brow item${it.score != null ? ' sem' : ''}${Bz.sels.has(i) || (!Bz.sels.size && i === Bz.sel) ? ' sel' : ''}${it.hidden ? ' hid' : ''}" data-i="${i}">
        <span class="ico ${it.is_dir ? 'dir' : ''}">${it.is_dir ? ICON.folder : ICON.file}</span>
        <span class="name" title="${esc(it.place ? joinPath(it.place, it.name) : it.name)}">${esc(it.name)}${it.place ? ` <small class="place">${esc(it.place)}</small>` : ''}</span>
        <span class="muted">${it.mtime ? fmtDate(it.mtime) : ''}</span>
        <span class="muted">${typeOf(it)}</span>
        <span class="num">${it.is_dir && Bz.live ? '' : fmtSize(it.size)}</span>
        <span class="num muted"${wide || it.score != null ? ` title="${esc(it.source || '')}"` : ''}>${wide || it.score != null ? esc(it.source || '') : it.is_dir && !Bz.live ? fmtNum(it.files) : ''}</span></div>`).join('') +
      (Bz.rows.length > Bz.shown ? `<div class="more"><button id="b-more">${tr('common.showMore', { n: fmtNum(Bz.rows.length - Bz.shown) })}</button></div>` : '')
    : `<p class="more muted">${finding ? tr('search.nothing') : tr('browse.empty')}</p>`;
  $('#b-more')?.addEventListener('click', () => { Bz.shown += 3000; renderBrowse(); });
  const s = curSource();
  if (finding) {
    const f = Fz.data;
    const shown = f.total > f.items.length ? ` · ${tr('search.shownFirst', { n: fmtNum(f.items.length) })}` : '';
    const where = f.everywhere
      ? ` · ${tr('search.everywhere')} · ${tr('common.ms', { n: f.ms.toFixed(0) })}${f.failed?.length ? ` · ${tr('search.noAnswer', { list: esc(f.failed.join(', ')) })}` : ''}${f.error ? ` · ${esc(f.error)}` : ''}`
      : f.onlyHere ? ` · ${tr('search.onlyHere')}` : ` · ${tr('search.inFolder', { folder: esc(folderLabel()) })} · ${tr('common.ms', { n: f.ms.toFixed(0) })}`;
    const btn = f.everywhere ? `<button data-scope="here">${tr('search.btn.here', { folder: esc(folderLabel() || tr('common.thisFolder')) })}</button>` : `<button data-scope="all">${tr('search.btn.all')}</button>`;
    const sem = f.sem?.length ? ` · ${tr('search.semCount', { n: fmtNum(f.sem.length) })}` : Sem.state === 'indexing' || Sem.state === 'download' ? ` · ${tr('search.semPreparing', { state: esc(semText()) })}` : '';
    $('#b-foot').innerHTML = `${tr('search.found', { n: fmtNum(f.total) })}${shown}${sem}${where} ${btn}`;
    return;
  }
  const dirs = Bz.data.items.filter((x) => x.is_dir).length;
  $('#b-foot').textContent = tr('browse.counts', { dirs: fmtNum(dirs), files: fmtNum(Bz.data.items.length - dirs) }) +
    (Bz.live ? ` · ${tr('browse.live')}` : ` · ${trn('browse.total', Bz.data.files, { size: fmtSize(Bz.data.size) })}`) +
    (Bz.data.total > Bz.data.items.length ? ` · ${tr('browse.shownFirstOf', { n: fmtNum(Bz.data.items.length), total: fmtNum(Bz.data.total) })}` : '') +
    (s && !s.online ? ` · ${tr('browse.offlineFromIndex')}` : '');
}
$$('#b-head [data-bsort]').forEach((b) => b.addEventListener('click', () => {
  const by = b.dataset.bsort;
  Bz.sort = { by, desc: Bz.sort.by === by ? !Bz.sort.desc : ['size', 'files', 'mtime'].includes(by) };
  Bz.sel = -1; // после пересортировки номер строки указывал бы на другой файл
  renderDetails();
  tabCapture();
  // большая папка приходит урезанной — пересортировать её может только программа
  if (!Bz.live && !Fz.query && Bz.data && Bz.data.total > Bz.data.items.length) openDir(Bz.dir, true);
  else renderBrowse();
}));

// файл своего компьютера открываем сразу, другого — сначала скачиваем во временную папку
async function openFile(it) {
  const s = curSource();
  const path = itemPath(it);
  const peer = it.full ? it.peer : s.peer;
  if (!peer) { call('open_path', { path }); return; }
  toast(tr('file.downloading', { name: it.name }), 60000);
  try { await invoke('peer_open', { peer, path }); toast(tr('file.opened', { name: it.name })); } catch (e) { toast(String(e), 8000); }
}
async function downloadFile(it) {
  const dir = await window.__TAURI__.dialog.open({ directory: true, title: tr('file.saveTo') });
  if (typeof dir !== 'string') return;
  try {
    await invoke('job_add', { kind: 'copy', items: [locOf(it)], dest: { peer: null, path: dir }, urls: [] });
    jobAdded();
  } catch (e) { toast(String(e), 8000); }
}
listen('download-progress', ({ payload: p }) => {
  if (p.size) toast(tr('file.downloadProgress', { name: baseName(p.path), pct: Math.floor((p.done / p.size) * 100), size: fmtSize(p.size) }), 60000);
});

const itemPath = (it) => it.full || joinPath(it.place ? joinPath(Bz.data.path, it.place) : Bz.data.path, it.name);
// ключ строки: имя (у найденного во вложенных — вместе с местом; у дубликатов — полный путь)
const rowKey = (it) => it.full || (it.place ? it.place + '\u0000' : '') + it.name;
function rememberSel() {
  Bz.selKeys = new Set([...Bz.sels].map((i) => Bz.rows[i]).filter(Boolean).map(rowKey));
  Bz.anchorKey = Bz.rows[Bz.sel] ? rowKey(Bz.rows[Bz.sel]) : null;
}
// после перерисовки строки могли сдвинуться — находим выделенные файлы заново
function restoreSel() {
  if (!Bz.selKeys) return;
  Bz.sels = new Set();
  Bz.sel = -1;
  Bz.rows.forEach((it, i) => {
    const k = rowKey(it);
    if (Bz.selKeys.has(k)) Bz.sels.add(i);
    if (k === Bz.anchorKey) Bz.sel = i;
  });
}

function selectRow(i, mode) {
  if (mode === 'toggle') {
    if (!Bz.sels.size && Bz.sel >= 0) Bz.sels.add(Bz.sel);
    Bz.sels.has(i) ? Bz.sels.delete(i) : Bz.sels.add(i);
  } else if (mode === 'range' && Bz.sel >= 0) {
    const [a, b] = [Math.min(Bz.sel, i), Math.max(Bz.sel, i)];
    Bz.sels = new Set(Array.from({ length: b - a + 1 }, (_, k) => a + k));
  } else {
    Bz.sels = new Set([i]);
  }
  if (mode !== 'range') Bz.sel = i;
  rememberSel();
  paintSel();
  $(`#b-list .item[data-i="${i}"]`)?.scrollIntoView({ block: 'nearest' });
  renderDetails();
}
// ---------- детали выбранного файла справа, с предпросмотром ----------
const Pv = { seq: 0, cache: new Map() };
function renderDetails() {
  const it = Bz.rows[Bz.sel];
  const box = $('#b-details');
  // у найденных дубликатов свой источник: другой диск или S3
  const s = it?.full ? Bz.sources.find((x) => (x.peer || null) === (it.peer || null) && x.seg === it.seg) || curSource() : curSource();
  // панель видна, только когда что-то выбрано
  $('.b-body').classList.toggle('with-details', !!(it && Bz.data && s));
  if (!it || !Bz.data || !s) { box.innerHTML = ''; return; }
  const path = itemPath(it);
  const local = it.full ? !it.cloud : !Bz.data.remote;
  box.innerHTML = `
    <button class="d-close" data-dact="close" title="${tr('details.close.title')}" aria-label="${tr('common.close')}">✕</button>
    <div class="d-preview" id="d-preview"><span class="ico big ${it.is_dir ? 'dir' : ''}">${it.is_dir ? ICON.folder : ICON.file}</span></div>
    <div class="d-name">${esc(it.name)}</div>
    <dl>
      <dt>${tr('col.type')}</dt><dd>${typeOf(it)}</dd>
      <dt>${it.is_dir ? tr('details.contentSize') : tr('col.size')}</dt><dd>${it.is_dir && Bz.live ? '—' : `${fmtSize(it.size)}${it.size >= 1024 ? ` <span class="muted">(${fmtNum(it.size)} ${tr('unit.B')})</span>` : ''}`}</dd>
      ${it.is_dir && !Bz.live ? `<dt>${tr('details.filesInside')}</dt><dd>${fmtNum(it.files)}</dd>` : ''}
      <dt>${tr('col.mtime')}</dt><dd>${it.mtime ? fmtDate(it.mtime) : '—'}</dd>
      <dt>${tr('common.path')}</dt><dd class="d-path">${esc(path)}</dd>
    </dl>
    <div class="d-actions">
      ${!it.is_dir && local ? `<button data-dact="open" title="${s.peer ? tr('details.open.peer.title') : tr('details.open.title')}">${tr('common.open')}</button>` : ''}
      ${!it.is_dir && local && s.peer ? `<button data-dact="download" title="${tr('details.download.title')}">${tr('common.download')}</button>` : ''}
      ${!it.is_dir && local && !s.peer ? `<button data-dact="reveal" title="${tr('details.reveal.title')}">${tr('common.reveal')}</button>` : ''}
      ${it.is_dir && local && !s.peer ? `<button data-dact="open" title="${tr('details.openDir.title')}">${tr('common.openExplorer')}</button>` : ''}
      <button data-dact="copy">${tr('common.copyPath')}</button>
    </div>`;
  if (!it.is_dir) loadPreview(s, path, it.size);
}
async function loadPreview(src, path, size) {
  const root = src.root;
  const my = ++Pv.seq;
  const spot = $('#d-preview');
  if (spot && /\.(jpe?g|png|gif|webp|bmp|svg|ico|txt|md|log|json|csv|xml|ya?ml|ini|toml|js|ts|py|rs|html|css|sql|sh|bat|ps1|cfg|conf|env)$/i.test(path)) {
    spot.insertAdjacentHTML('beforeend', `<small class="muted pv-wait">${tr('preview.loading')}</small>`);
  }
  const cacheKey = keyOf(src) + '>' + path;
  let pv = Pv.cache.get(cacheKey);
  if (!pv) {
    try { pv = (await api('preview', { root, path, size }, src)) || { kind: 'none' }; } catch (e) { pv = { kind: 'error', data: String(e) }; }
    // кэш небольшой: только последние просмотренные
    Pv.cache.set(cacheKey, pv);
    if (Pv.cache.size > 30) Pv.cache.delete(Pv.cache.keys().next().value);
  }
  if (my !== Pv.seq) return; // уже выбран другой файл
  const box = $('#d-preview');
  if (!box) return;
  box.querySelector('.pv-wait')?.remove();
  if (pv.kind === 'image') {
    // картинка грузится сама (для S3 — по временной ссылке); пока грузится — подпись
    const img = new Image();
    img.alt = '';
    box.insertAdjacentHTML('beforeend', `<small class="muted pv-wait">${tr('preview.loading')}</small>`);
    img.onload = () => { if (my === Pv.seq) box.replaceChildren(img); };
    img.onerror = () => { if (my === Pv.seq) { box.querySelector('.pv-wait')?.remove(); box.insertAdjacentHTML('beforeend', `<small class="muted">${tr('preview.imgFailed')}</small>`); Pv.cache.delete(cacheKey); } };
    img.src = pv.data;
  }
  else if (pv.kind === 'text') box.innerHTML = `<pre>${esc(pv.data)}</pre>`;
  else if (pv.kind === 'toobig') box.insertAdjacentHTML('beforeend', `<small class="muted">${tr('preview.tooBig')}</small>`);
  else if (pv.kind === 'error') box.insertAdjacentHTML('beforeend', `<small class="muted">${esc(pv.data)}</small>`);
}
$('#b-details').addEventListener('click', (e) => {
  const b = e.target.closest('[data-dact]');
  const it = Bz.rows[Bz.sel];
  if (!b || !it) return;
  if (b.dataset.dact === 'close') { clearSel(); return; }
  const path = itemPath(it);
  if (b.dataset.dact === 'open') openFile(it);
  else if (b.dataset.dact === 'download') downloadFile(it);
  else if (b.dataset.dact === 'reveal') call('reveal', { path });
  else if (b.dataset.dact === 'copy') navigator.clipboard.writeText(path).then(() => toast(tr('common.pathCopied')));
});

function activate(i) {
  const it = Bz.rows[i];
  if (!it) return;
  if (Fz.query) pushQuery(Fz.query);
  if (it.is_dir && it.full) { clearQuery(); goPath(it.full, it.peer || null); return; }
  if (it.is_dir) Bz.live ? openLive(itemPath(it)) : openDir(it.idx);
  else if (Fz.query && it.place != null && Bz.data.remote) goPath(joinPath(Bz.data.path, it.place));
  else if (!Bz.data.remote) openFile(it);
}
$('#b-list').addEventListener('click', (e) => {
  const r = e.target.closest('.item');
  if (r) selectRow(+r.dataset.i, e.ctrlKey ? 'toggle' : e.shiftKey ? 'range' : null);
  else if (!e.target.closest('button')) clearSel(); // щелчок по пустому месту снимает выбор
});
function clearSel() {
  Bz.sel = -1;
  Bz.sels = new Set();
  rememberSel();
  $$('#b-list .item.sel').forEach((x) => x.classList.remove('sel'));
  renderDetails();
}
$('#b-list').addEventListener('dblclick', (e) => { const r = e.target.closest('.item'); if (r) activate(+r.dataset.i); });
$('#b-list').addEventListener('keydown', (e) => {
  if (e.key === 'ArrowDown') { e.preventDefault(); selectRow(Math.min(Bz.sel + 1, Math.min(Bz.rows.length, Bz.shown) - 1)); }
  else if (e.key === 'ArrowUp') { e.preventDefault(); selectRow(Math.max(Bz.sel - 1, 0)); }
  else if (e.key === 'Enter') activate(Bz.sel);
  else if (e.key === 'Backspace') goUp();
  else if (e.key === 'Escape') clearSel();
});

// ---------- контекстное меню (правый клик) ----------
function showMenu(e, items) {
  e.preventDefault();
  const m = $('#ctx');
  m.innerHTML = items.filter(Boolean).map(([label, fn], i) => `<button data-i="${i}">${label}</button>`).join('');
  const fns = items.filter(Boolean).map((x) => x[1]);
  m.onclick = (ev) => { const b = ev.target.closest('button'); if (b) { hideMenu(); fns[+b.dataset.i](); } };
  m.classList.remove('hidden');
  const r = m.getBoundingClientRect();
  m.style.left = `${Math.min(e.clientX, innerWidth - r.width - 8)}px`;
  m.style.top = `${Math.min(e.clientY, innerHeight - r.height - 8)}px`;
}
const hideMenu = () => $('#ctx').classList.add('hidden');
document.addEventListener('mousedown', (e) => { if (!e.target.closest('#ctx')) hideMenu(); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') hideMenu(); });

$('#src-list').addEventListener('contextmenu', (e) => {
  const b = e.target.closest('.src-item');
  if (!b) return;
  const s = Bz.sources.find((x) => x.key === b.dataset.key);
  const remote = !!s.peer;
  const reach = !remote || s.peerOnline;
  showMenu(e, [
    !remote && Bz.scanning === s.root && [tr('ctx.stopScan'), () => invoke('cancel')],
    !remote && s.online && Bz.scanning !== s.root && [s.indexed ? tr('ctx.rescan') : tr('ctx.scan'), () => startScan([s.root])],
    reach && [tr('ctx.rename'), () => renameInline(b, s.title, async (name) => {
      await api('source_rename', { root: s.root, name }, s);
      // переименованное подключение S3 меняет корень — следим, чтобы выбор не потерялся
      if (s.kind === 's3' && name && Bz.cur === s.key) Bz.cur = remote ? `${s.peer}|s3://${name}` : `s3://${name}`;
      await loadSources();
    })],
    !remote && s.kind === 's3' && [tr('s3.edit'), () => openModal('s3', s.title)],
    !remote && s.kind !== 's3' && s.online && [tr('common.openExplorer'), () => call('open_path', { path: s.root })],
    reach && [tr('ctx.remove'), () => removeSource(s)],
  ]);
});

// подтверждение с понятным описанием, что именно произойдёт
async function removeSource(s) {
  const where = s.peer ? ` ${tr('common.onComputer', { name: s.peerName })}` : '';
  const what = tr(s.kind === 's3' ? 'remove.s3' : s.kind === 'folder' ? 'remove.folder' : 'remove.drive', { title: s.title, where });
  if (!(await ask(tr('remove.ask', { title: s.title, what }), { ok: tr('remove.ok'), danger: true }))) return;
  try { await api('source_remove', { root: s.root }, s); } catch (e) { toast(String(e), 6000); return; }
  if (Bz.cur === s.key) { Bz.cur = null; Bz.data = null; }
  toast(tr('remove.done', { title: s.title }));
  await loadSources();
  refreshStatus();
}

// переименование прямо в списке: Enter — сохранить, Esc — отменить, пустое — вернуть исходное
function renameInline(el, current, save) {
  const b = el.querySelector('b');
  if (!b) return;
  const input = document.createElement('input');
  input.className = 'rename';
  input.value = current;
  b.replaceWith(input);
  input.focus();
  input.select();
  let done = false;
  const finish = async (ok) => {
    if (done) return;
    done = true;
    if (ok && input.value.trim() !== current) {
      try { await save(input.value.trim()); return; } catch (e) { toast(String(e), 6000); }
    }
    input.replaceWith(b);
  };
  input.addEventListener('keydown', (e) => {
    e.stopPropagation();
    if (e.key === 'Enter') finish(true);
    if (e.key === 'Escape') finish(false);
  });
  input.addEventListener('click', (e) => e.stopPropagation());
  input.addEventListener('blur', () => finish(true));
}
$('#b-list').addEventListener('contextmenu', (e) => {
  const r = e.target.closest('.item');
  if (!r) {
    // пустое место — вставка и загрузка сюда
    if (!Bz.data || Fz.query) return;
    showMenu(e, [
      Clip.items.length && [tr('ctx.paste', { n: Clip.items.length }), clipPaste],
      [tr('ctx.downloadHere'), downloadDialog],
    ]);
    return;
  }
  if (!Bz.sels.has(+r.dataset.i)) selectRow(+r.dataset.i);
  const it = Bz.rows[+r.dataset.i];
  const local = !Bz.data.remote;
  const many = Bz.sels.size > 1;
  showMenu(e, [
    [tr('ctx.copy'), () => clipSet('copy')],
    [tr('ctx.cut'), () => clipSet('cut')],
    it.is_dir && Clip.items.length && !many && [tr('ctx.pasteInto', { name: it.name }), () => { const d = { peer: curSource()?.peer || null, path: itemPath(it) }; invoke('job_add', { kind: Clip.op === 'cut' ? 'move' : 'copy', items: Clip.items.map(({ peer, path }) => ({ peer, path })), dest: d, urls: [], confirmed: true }).then(() => jobAdded()).catch((er) => toast(String(er), 6000)); }],
    [many ? tr('ctx.trashN', { n: Bz.sels.size }) : tr('ctx.trash'), () => deleteSelected(false)],
    [many ? tr('ctx.deleteN', { n: Bz.sels.size }) : tr('ctx.delete'), () => deleteSelected(true)],
    [tr('common.open'), () => activate(+r.dataset.i)],
    it.is_dir && [tr('ctx.openNewTab'), () => tabNew(itemPath(it))],
    local && !curSource().peer && [tr('common.reveal'), () => call('reveal', { path: itemPath(it) })],
    !it.is_dir && local && curSource().peer && [tr('common.download'), () => downloadFile(it)],
    [tr('common.copyPath'), () => navigator.clipboard.writeText(itemPath(it)).then(() => toast(tr('common.pathCopied')))],
  ]);
});
document.addEventListener('click', (e) => {
  const b = e.target.closest('[data-scan]');
  if (b) startScan([b.dataset.scan]);
  const ed = e.target.closest('[data-s3edit]');
  if (ed) openModal('s3', ed.dataset.s3edit);
});

async function startScan(roots) {
  await call('scan', { roots, gentle: $('#s-gentle').checked });
  Bz.scanning = roots[0];
  Bz.scanFiles = 0;
  renderSourceList();
}

// подключение и отключение дисков
listen('drives-changed', async ({ payload: events }) => {
  for (const ev of events || []) toast(tr(ev.connected ? 'drive.connected' : 'drive.disconnected', { title: ev.title }), 5000);
  if (!$('#tab-sources').classList.contains('active')) return;
  if (events && events.length) { await loadSources(); return; }
  Bz.sources = await fetchSources();
  renderSourceList();
});

// ---------- окно «Добавить источник» ----------
let s3Editing = null;
async function openModal(type, s3name) {
  $('#add-modal').classList.remove('hidden');
  $('#m-types').classList.toggle('hidden', !!type);
  $('#m-local').classList.toggle('hidden', type !== 'local');
  $('#s3-form').classList.toggle('hidden', type !== 's3');
  $('#m-peer').classList.toggle('hidden', type !== 'peer');
  if (type === 'peer') {
    // по умолчанию — диски и папки этого компьютера, только просмотр; облака — не открываем
    renderSharePick($('#share-pick-invite'), { level: 'view', roots: Bz.sources.filter((s) => !s.peer && s.kind !== 's3').map((s) => s.root) }, false);
    $('#peer-code-out').value = '';
    $('#peer-copy').classList.add('hidden');
    $('#peer-wait').textContent = '';
    $('#peer-join-form').reset();
    PeerWait.known = null;
  }
  $('#m-back').classList.toggle('hidden', !type || !!s3name);
  $$('.m-status').forEach((x) => { x.textContent = ''; });
  $('#m-title').textContent = !type ? tr('add.title') : type === 'local' ? tr('add.local') : type === 'peer' ? tr('add.peer') : s3name ? tr('s3.title.edit', { name: s3name }) : tr('s3.title');
  if (type === 'local') { $('#m-local').reset(); $('#m-local [name=path]').focus(); }
  if (type === 's3') {
    const f = $('#s3-form');
    f.reset();
    s3Editing = s3name || null;
    f.elements.secret.type = 'password';
    $('#s3-eye').textContent = tr('s3.show');
    f.elements.secret.placeholder = s3name ? tr('s3.secret.saved') : '';
    s3Result(null);
    if (s3name) {
      const c = (await invoke('s3_list')).find((x) => x.name === s3name);
      if (c) for (const k of ['name', 'bucket', 'endpoint', 'region', 'prefix', 'access_key']) f.elements[k].value = c[k] || '';
    }
    f.elements.name.focus();
  }
  if (!type) $('#m-types button:not([disabled])').focus();
}
const closeModal = () => $('#add-modal').classList.add('hidden');
$('#src-add').addEventListener('click', () => openModal(null));
$('#m-close').addEventListener('click', closeModal);
$('#m-back').addEventListener('click', () => openModal(null));
$('#add-modal').addEventListener('mousedown', (e) => { if (e.target.id === 'add-modal') closeModal(); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('#add-modal').classList.contains('hidden')) closeModal(); });
$('#m-types').addEventListener('click', (e) => {
  const b = e.target.closest('button[data-type]:not([disabled])');
  if (b) openModal(b.dataset.type);
});
$('#m-local').addEventListener('submit', async (e) => {
  e.preventDefault();
  const p = e.target.elements.path.value.trim();
  try { await startScan([p]); } catch { return; }
  closeModal();
  Bz.cur = p;
  loadSources();
});
const s3Values = () => {
  const f = $('#s3-form'), v = (k) => f.elements[k].value.trim();
  return { cfg: { name: v('name'), bucket: v('bucket'), endpoint: v('endpoint'), region: v('region'), prefix: v('prefix'), access_key: v('access_key') }, secret: v('secret') };
};
// результат проверки под кнопками: зелёный — работает, красный — причина и подробности
function s3Result(kind, message, details) {
  const r = $('#s3-result');
  r.classList.toggle('hidden', !kind);
  if (!kind) return;
  r.className = `check ${kind}`;
  r.querySelector('b').textContent = message;
  r.querySelector('details').classList.toggle('hidden', !details);
  r.querySelector('pre').textContent = details || '';
}
$('#s3-eye').addEventListener('click', () => {
  const i = $('#s3-form').elements.secret;
  i.type = i.type === 'password' ? 'text' : 'password';
  $('#s3-eye').textContent = i.type === 'password' ? tr('s3.show') : tr('s3.hide');
});
$('#s3-cancel').addEventListener('click', closeModal);
// «Проверить» — проверяет то, что введено сейчас, ничего не сохраняя
$('#s3-test').addEventListener('click', async () => {
  const { cfg, secret } = s3Values();
  $('#s3-test').disabled = true;
  s3Result('busy', tr('s3.testing'));
  try {
    await invoke('s3_test', { cfg, secret, oldName: s3Editing });
    s3Result('ok', tr('s3.ok'));
  } catch (e) {
    s3Result('bad', e.message || String(e), e.details);
  } finally {
    $('#s3-test').disabled = false;
  }
});
// «Сохранить» — сохраняет всегда, без проверки
$('#s3-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const { cfg, secret } = s3Values();
  try {
    await invoke('s3_save', { cfg, secret, oldName: s3Editing });
  } catch (err) {
    s3Result('bad', String(err));
    return;
  }
  closeModal();
  Bz.cur = `s3://${cfg.name}`;
  Bz.dir = null;
  await loadSources();
  toast(tr('common.saved', { name: cfg.name }));
});

// выбор папки через системный диалог
$$('[data-browse]').forEach((b) => b.addEventListener('click', async () => {
  const input = $(b.dataset.browse);
  const dir = await window.__TAURI__.dialog.open({ directory: true, defaultPath: input.value.trim() || undefined });
  if (typeof dir === 'string') input.value = dir;
}));
let sideTimer;
listen('scan-progress', ({ payload: p }) => {
  const first = Bz.scanning !== p.root;
  Bz.scanning = p.root;
  Bz.scanFiles = p.files;
  // статус в левой панели обновляем не чаще раза в секунду
  if (first) renderSourceList();
  else if (!sideTimer) sideTimer = setTimeout(() => { sideTimer = null; renderSourceList(); }, 1000);
  const txt = `${p.root}: ${trn('common.files', p.files)}, ${trn('common.folders', p.dirs)}, ${fmtSize(p.bytes)}`;
  $('#st-task').textContent = tr('scan.progress', { txt });
});
listen('scan-done', async ({ payload: error }) => {
  Bz.scanning = null;
  Bz.scanFiles = 0;
  $('#st-task').textContent = '';
  if (error) toast(error === 'scan cancelled' ? tr('scan.stopped') : error);
  else toast(tr('scan.done'));
  await loadSources();
  await refreshStatus();
});

// ---------- живые обновления ----------
// индекс меняется по событиям системы: обновляем счётчик и открытую папку
let liveTimer;
listen('index-changed', () => {
  clearTimeout(liveTimer);
  liveTimer = setTimeout(() => {
    refreshStatus();
    if ($('#tab-sources').classList.contains('active') && Bz.data && !Bz.live && !Bz.busy) openDir(Bz.dir, true);
  }, 1000);
});

// ---------- о программе ----------
async function loadAbout() {
  const a = await invoke('about');
  $('#a-version').textContent = a.version;
  $('#a-data').textContent = a.data_dir;
  $("#a-index").textContent = tr('about.indexInfo', { entries: fmtNum(a.entries), sources: a.sources, size: fmtSize(a.index_bytes) });
  $('#a-sem').textContent = semText();
}
$('#a-open').addEventListener('click', () => call('open_path', { path: $('#a-data').textContent }));
$('#a-repo').addEventListener('click', (e) => { e.preventDefault(); call('open_repo'); });

// ---------- обновления: GitHub Releases, подпись проверяется перед установкой ----------
const Up = { found: null, busy: false, msg: null };
// сообщение — функцией, чтобы при смене языка перерисовать его на новом
function updMsg(m) { Up.msg = typeof m === 'function' ? m : () => m; $('#a-upd-msg').textContent = Up.msg(); }
function updRender() {
  if (Up.msg) $('#a-upd-msg').textContent = Up.msg();
  $('#a-upd').textContent = Up.found ? tr('update.install') : tr('update.check');
}
async function updCheck(quiet) {
  if (Up.busy) return;
  Up.busy = true;
  if (!quiet) updMsg(() => tr('update.checking'));
  try {
    const u = await invoke('update_check');
    Up.found = u.version ? u : null;
    if (Up.found) {
      updMsg(() => (u.date ? tr('update.availableDate', { version: u.version, date: u.date }) : tr('update.available', { version: u.version })));
      $('#a-upd').textContent = tr('update.install');
      $('#a-upd-notes').textContent = u.notes || '';
      $('#a-upd-notes').classList.toggle('hidden', !u.notes);
      $('#menu-btn').classList.add('has-update');
    } else if (!quiet) {
      updMsg(() => tr('update.latest', { version: u.current }));
    }
  } catch (e) {
    if (!quiet) updMsg(String(e));
  } finally {
    Up.busy = false;
  }
}
async function updInstall() {
  if (Up.busy) return;
  if (!(await ask(tr('update.ask', { version: Up.found.version }), { ok: tr('update.ok') }))) return;
  Up.busy = true;
  $('#a-upd').disabled = true;
  updMsg(() => tr('update.downloading'));
  try {
    await invoke('update_install');
  } catch (e) {
    updMsg(String(e));
    $('#a-upd').disabled = false;
    Up.busy = false;
  }
}
$('#a-upd').addEventListener('click', () => (Up.found ? updInstall() : updCheck(false)));
listen('update-progress', ({ payload: p }) => {
  updMsg(() => tr('update.downloadingN', { n: p.total ? `${Math.floor((p.done / p.total) * 100)}%` : fmtSize(p.done) }));
});
// тихая проверка вскоре после запуска и раз в сутки: только метка на вкладке «О программе»
setTimeout(() => updCheck(true), 15000);
setInterval(() => updCheck(true), 24 * 3600 * 1000);

// ---------- вкладки: у каждой свой источник, папка, сортировка и запрос; всё переживает перезапуск ----------
const TABS_KEY = 'filecat.tabs';
const Tb = { list: [], active: null };
const newId = () => Math.random().toString(36).slice(2, 10);

function tabsLoad() {
  try {
    const t = JSON.parse(localStorage.getItem(TABS_KEY));
    if (t && Array.isArray(t.list) && t.list.length) { Tb.list = t.list; Tb.active = t.active; }
  } catch { /* хранилище недоступно — начнём с одной вкладки */ }
  if (!Tb.list.length) { const id = newId(); Tb.list = [{ id, root: null, path: null, title: '' }]; Tb.active = id; }
  if (!Tb.list.some((t) => t.id === Tb.active)) Tb.active = Tb.list[0].id;
}
function tabsSave() { try { localStorage.setItem(TABS_KEY, JSON.stringify({ list: Tb.list, active: Tb.active })); } catch { /* недоступно */ } }
const activeTab = () => Tb.list.find((t) => t.id === Tb.active);

// снимок текущего состояния — в активную вкладку
function tabCapture() {
  const t = activeTab();
  if (!t) return;
  t.root = Bz.cur;
  t.peer = curSource()?.peer || null;
  t.path = Bz.data?.path || Bz.cur;
  t.title = folderLabel() || Bz.cur || '';
  t.sort = { ...Bz.sort };
  t.query = $('#b-q').value.trim();
  tabsSave();
  renderTabs();
}

function renderTabs() {
  $('#ftabs').innerHTML = Tb.list.map((t) => `
    <div class="ftab${t.id === Tb.active && $('#tab-sources').classList.contains('active') ? ' active' : ''}" data-id="${t.id}" title="${esc(t.path || '')}">
      <span class="t">${esc(t.title || tr('tabs.new'))}</span>
      ${Tb.list.length > 1 ? `<span class="x" data-close="${t.id}" title="${tr('tabs.close.title')}">✕</span>` : ''}
    </div>`).join('');
}

// открыть состояние вкладки: источник, сортировка, папка, запрос
async function tabRestore(t) {
  Bz.sort = t.sort ? { ...t.sort } : { by: 'name', desc: false };
  clearQuery();
  Bz.data = null;
  Bz.live = false;
  Bz.dir = null;
  if (t.root && Bz.sources.some((s) => s.key === t.root)) Bz.cur = t.root;
  renderSourceList();
  const src = curSource();
  if (t.path && src && t.path !== src.root) await goPath(t.path, src.peer);
  else await openDir(null);
  if (t.query) { $('#b-q').value = t.query; await runFind(); }
}

async function tabSwitch(id) {
  if (!$('#tab-sources').classList.contains('active')) showView('sources');
  if (id === Tb.active) return;
  tabCapture();
  Tb.active = id;
  tabsSave();
  renderTabs();
  await tabRestore(activeTab());
  tabCapture();
}

async function tabNew(path) {
  tabCapture();
  const t = { id: newId(), root: Bz.cur, path: path || Bz.data?.path || Bz.cur, title: '', sort: { ...Bz.sort } };
  const i = Tb.list.findIndex((x) => x.id === Tb.active);
  Tb.list.splice(i + 1, 0, t);
  Tb.active = t.id;
  tabsSave();
  renderTabs();
  if (!$('#tab-sources').classList.contains('active')) showView('sources');
  await tabRestore(t);
  tabCapture();
}

async function tabClose(id) {
  if (Tb.list.length < 2) return;
  const i = Tb.list.findIndex((x) => x.id === id);
  Tb.list.splice(i, 1);
  if (id === Tb.active) {
    Tb.active = Tb.list[Math.min(i, Tb.list.length - 1)].id;
    tabsSave();
    renderTabs();
    await tabRestore(activeTab());
  } else {
    tabsSave();
    renderTabs();
  }
}

$('#ftabs').addEventListener('mousedown', (e) => {
  const x = e.target.closest('[data-close]');
  const t = e.target.closest('.ftab');
  if (!t) return;
  if (x || e.button === 1) { e.preventDefault(); tabClose(t.dataset.id); return; } // средняя кнопка — закрыть
  if (e.button === 0) tabSwitch(t.dataset.id);
});
$('#ftab-new').addEventListener('click', () => tabNew());
document.addEventListener('keydown', (e) => {
  if (!e.ctrlKey) return;
  const k = e.key.toLowerCase();
  if (k === 't' || k === 'е') { e.preventDefault(); tabNew(); }
  else if (k === 'w' || k === 'ц') { e.preventDefault(); tabClose(Tb.active); }
  else if (e.key === 'Tab') {
    e.preventDefault();
    const i = Tb.list.findIndex((x) => x.id === Tb.active);
    const n = Tb.list.length;
    tabSwitch(Tb.list[(i + (e.shiftKey ? n - 1 : 1)) % n].id);
  }
});
// средняя кнопка на папке — открыть её в новой вкладке
$('#b-list').addEventListener('mousedown', (e) => {
  if (e.button !== 1) return;
  const r = e.target.closest('.item');
  const it = r && Bz.rows[+r.dataset.i];
  if (it && it.is_dir) { e.preventDefault(); tabNew(itemPath(it)); }
});

// ---------- выделение нескольких: Ctrl — добавить, Shift — диапазон, Ctrl+A — всё ----------
Bz.sels = new Set();
function paintSel() {
  $$('#b-list .item').forEach((r) => r.classList.toggle('sel', Bz.sels.has(+r.dataset.i)));
}
function selectedItems() {
  const idx = Bz.sels.size ? [...Bz.sels] : Bz.sel >= 0 ? [Bz.sel] : [];
  return idx.sort((a, b) => a - b).map((i) => Bz.rows[i]).filter(Boolean);
}
// место в задаче: путь + устройство
const locOf = (it) => ({ peer: it.full ? it.peer || null : curSource()?.peer || null, path: itemPath(it) });
const hereLoc = () => (Bz.data ? { peer: curSource()?.peer || null, path: Bz.data.path } : null);

// ---------- буфер: копировать / вырезать / вставить (между вкладками и компьютерами тоже) ----------
const Clip = { op: null, items: [] };
function clipSet(op) {
  const items = selectedItems();
  if (!items.length) return;
  Clip.op = op;
  Clip.items = items.map((it) => ({ ...locOf(it), name: it.name }));
  // пути — и в системный буфер: так понятно, что скопировано последним (файлы здесь или ссылка где-то ещё)
  Clip.text = Clip.items.map((x) => x.path).join('\n');
  window.__TAURI__?.clipboardManager?.writeText(Clip.text).catch(() => {});
  toast(tr(op === 'cut' ? 'clip.cut' : 'clip.copied', { what: items.length === 1 ? items[0].name : trn('common.items', items.length) }));
}
async function clipPaste() {
  const dest = hereLoc();
  if (!Clip.items.length || !dest) return;
  if (Fz.query) { toast(tr('clip.clearSearch')); return; }
  const kind = Clip.op === 'cut' ? 'move' : 'copy';
  try {
    await invoke('job_add', { kind, items: Clip.items.map(({ peer, path }) => ({ peer, path })), dest, urls: [], confirmed: true });
    jobAdded();
  } catch (e) { toast(String(e), 6000); return; }
  if (kind === 'move') { Clip.op = null; Clip.items = []; }
}

// ---------- удаление: Delete — в корзину, Shift+Delete — навсегда (с подтверждением) ----------
async function deleteSelected(forever) {
  const items = selectedItems();
  if (!items.length) return;
  const s = curSource();
  const cloud = items.some((it) => itemPath(it).startsWith('s3://'));
  const peerOf = items[0].full ? items[0].peer : s.peer;
  const where = peerOf ? ` ${tr('common.onComputer', { name: Pz.info.peers.find((p) => p.id === peerOf)?.name || s.peerName || tr('common.otherShort') })}` : '';
  const bytes = items.reduce((a, it) => a + (it.size || 0), 0);
  // у дубликатов имена одинаковые — показываем, какие именно копии
  const what = items[0].full
    ? (items.length === 1 ? items[0].full : `${trn('common.files', items.length)}:\n${items.slice(0, 6).map((it) => it.full).join('\n')}${items.length > 6 ? '\n…' : ''}`)
    : items.length === 1 ? tr('common.quoted', { name: items[0].name }) : trn('common.items', items.length);
  // в S3 корзины нет — там удаление всегда навсегда
  if (forever || cloud) {
    const why = cloud && !forever ? `\n\n${tr('del.s3')}` : '';
    if (!(await ask(tr('del.forever.ask', { what, where, size: fmtSize(bytes) }) + why, { ok: tr('del.forever.ok'), danger: true }))) return;
  } else if (!(await ask(tr('del.trash.ask', { what, where }), { ok: tr('del.trash.ok') }))) return;
  try {
    await invoke('job_add', { kind: forever || cloud ? 'delete' : 'trash', items: items.map(locOf), dest: null, urls: [], confirmed: true });
    jobAdded();
  } catch (e) { toast(String(e), 6000); }
}

// ---------- загрузка по ссылке в открытую папку ----------
function downloadDialog() {
  const dest = hereLoc();
  if (!dest) return;
  $('#dl-dest').textContent = (curSource()?.peer ? curSource().peerName + ' · ' : '') + dest.path;
  $('#dl-urls').value = '';
  $('#dl-modal').classList.remove('hidden');
  $('#dl-urls').focus();
}
$('#dl-cancel').addEventListener('click', () => $('#dl-modal').classList.add('hidden'));
$('#dl-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const urls = $('#dl-urls').value.split(/\s+/).map((u) => u.trim()).filter((u) => /^https?:\/\//i.test(u));
  if (!urls.length) { toast(tr('dl.noLinks')); return; }
  try {
    await invoke('job_add', { kind: 'download', items: [], dest: hereLoc(), urls });
    $('#dl-modal').classList.add('hidden');
    jobAdded();
  } catch (err) { toast(String(err), 6000); }
});

// клавиши в списке файлов
$('#b-list').addEventListener('keydown', (e) => {
  const k = e.key.toLowerCase();
  if (e.ctrlKey && (k === 'c' || k === 'с')) { e.preventDefault(); clipSet('copy'); }
  else if (e.ctrlKey && (k === 'x' || k === 'ч')) { e.preventDefault(); clipSet('cut'); }
  else if (e.ctrlKey && (k === 'v' || k === 'м')) { e.preventDefault(); smartPaste(); }
  else if (e.ctrlKey && (k === 'a' || k === 'ф')) { e.preventDefault(); Bz.sels = new Set(Bz.rows.slice(0, Bz.shown).map((_, i) => i)); rememberSel(); paintSel(); }
  else if (e.key === 'Delete') { e.preventDefault(); deleteSelected(e.shiftKey); }
});

// ---------- панель задач внизу ----------
const TK = { list: [], open: false, height: 200 };
try { TK.height = +localStorage.getItem('filecat.tasksH') || 200; TK.open = localStorage.getItem('filecat.tasksShown') === '1'; } catch { /* недоступно */ }
const ACTIVE = ['queued', 'running', 'paused'];
// подписи вида и состояния задачи — функциями: так они следуют за сменой языка
const KIND = (k) => (I18N.en[`task.kind.${k}`] ? tr(`task.kind.${k}`) : k);
const STATE = (k) => (I18N.en[`task.state.${k}`] ? tr(`task.state.${k}`) : k);

function tasksOpen(on) {
  TK.open = on;
  try { localStorage.setItem('filecat.tasksShown', on ? '1' : '0'); } catch { /* недоступно */ }
  renderTasks();
}
const fmtEta = (s) => (s < 60 ? tr('time.s', { n: Math.ceil(s) }) : s < 3600 ? tr('time.min', { n: Math.round(s / 60) }) : tr('time.h', { n: (s / 3600).toFixed(1) }));

// таблица задач: строка на объект; действие над несколькими объектами — группа со сводной строкой
TK.fold = new Set();   // свёрнутые группы
TK.unfold = new Set(); // большие группы, которые развернули вручную
const devName = (peer) => {
  if (!peer || peer === Pz.info?.id) return '';
  return Pz.info?.peers?.find((p) => p.id === peer)?.name || tr('peer.other');
};
const withDev = (peer, path) => (devName(peer) ? `${devName(peer)} · ${path}` : path);
const shortPath = (path) => {
  const parts = path.split(/[\\/]/).filter(Boolean);
  if (parts.length <= 3) return path;
  const sep = path.includes('\\') ? '\\' : '/';
  return `${parts[0]}${sep}…${sep}${parts.slice(-2).join(sep)}`;
};
const withDevShort = (peer, path) => (devName(peer) ? `${devName(peer)} · ${shortPath(path)}` : shortPath(path));
const parentDir = (p) => {
  const t = p.replace(/[\\/]+$/, '');
  const i = Math.max(t.lastIndexOf('\\'), t.lastIndexOf('/'));
  return i > 0 ? t.slice(0, t[i - 1] === ':' ? i + 1 : i) : t;
};
const urlHost = (u) => { try { return new URL(u).host; } catch { return u; } };
const pctOf = (j) => (j.state === 'done' ? 100 : j.bytes_total ? Math.min(100, (j.bytes_done / j.bytes_total) * 100) : j.files_total ? (j.files_done / j.files_total) * 100 : 0);

function jobCells(j) {
  const it = j.items && j.items[0];
  const many = (j.items?.length || 0) + (j.urls?.length || 0) > 1;
  const name = j.urls?.length ? (many ? trn('common.links', j.urls.length) : baseName(j.urls[0].split('?')[0]) || j.urls[0])
    : it ? (many ? trn('common.items', j.items.length) : baseName(it.path)) : j.title;
  const from = j.urls?.length ? urlHost(j.urls[0]) : it ? withDevShort(it.peer, parentDir(it.path)) : '';
  const to = j.kind === 'trash' ? tr('task.to.trash') : j.kind === 'delete' ? tr('task.to.delete') : j.dest ? withDevShort(j.dest.peer, j.dest.path) : '';
  const full = (j.urls?.length ? j.urls[0] : it ? withDev(it.peer, it.path) : j.title) + (j.dest ? `\n→ ${withDev(j.dest.peer, j.dest.path)}` : '');
  return { name, from, to, full };
}

function progressCell(pct, sub, cls = '') {
  return `<span class="c-prog"><span class="bar ${cls}"><i style="width:${pct.toFixed(1)}%"></i></span><span class="pct">${Math.floor(pct)}%</span>${sub ? `<small class="muted">${sub}</small>` : ''}</span>`;
}

function jobRow(j, inGroup) {
  const c = jobCells(j);
  const running = j.state === 'running';
  const sub = j.bytes_total ? tr('common.ofTotal', { done: fmtSize(j.bytes_done), total: fmtSize(j.bytes_total) }) : j.files_total > 1 ? tr('common.ofTotal', { done: fmtNum(j.files_done), total: fmtNum(j.files_total) }) : '';
  const eta = running && j.speed > 0 && j.bytes_total > j.bytes_done ? fmtEta((j.bytes_total - j.bytes_done) / j.speed) : '';
  const btns = [
    running && `<button data-jact="pause" data-id="${j.id}" title="${tr('task.pause')}">❚❚</button>`,
    j.state === 'paused' && `<button data-jact="resume" data-id="${j.id}" title="${tr('task.resume')}">▶</button>`,
    ACTIVE.includes(j.state) && `<button data-jact="cancel" data-id="${j.id}" class="x" title="${tr('task.cancel')}">✕</button>`,
    ['failed', 'cancelled', 'interrupted'].includes(j.state) && `<button data-jact="retry" data-id="${j.id}" title="${tr('task.retry')}">↻</button>`,
    j.dest && !j.dest.peer && !j.dest.path.startsWith('s3://') && j.state === 'done' && `<button data-jact="open" data-id="${j.id}" title="${tr('task.openDest')}">📂</button>`,
  ].filter(Boolean).join('');
  const err = j.error || (j.errors?.length ? j.errors[0] : '');
  return `<div class="trow ${j.state}${inGroup ? ' sub' : ''}" title="${esc(c.full + (j.current && ACTIVE.includes(j.state) ? `\n${tr('task.current', { path: j.current })}` : '') + (err ? `\n${err}` : ''))}">
    <span class="c-fold"></span>
    <span class="c-what"><small class="kind">${KIND(j.kind)}</small>${esc(c.name)}</span>
    <span class="c-path">${esc(c.from)}</span>
    <span class="c-path">${esc(c.to)}</span>
    ${progressCell(pctOf(j), sub, j.state)}
    <span class="c-num">${running && j.speed ? tr('common.perSec', { size: fmtSize(j.speed) }) : ''}</span>
    <span class="c-num">${eta}</span>
    <span class="c-state">${STATE(j.state)}${err ? ` <span class="t-err">${esc(err)}</span>` : ''}</span>
    <span class="c-btns">${btns}</span>
  </div>${j.errors && j.errors.length > 1 ? `<details class="t-errs"><summary>${tr('task.errors', { n: j.errors.length })}</summary>${j.errors.slice(0, 50).map((e) => `<div>${esc(e)}</div>`).join('')}</details>` : ''}`;
}

function groupRow(g, list, folded) {
  const n = list.length;
  const act = list.filter((j) => ACTIVE.includes(j.state));
  const done = list.filter((j) => j.state === 'done').length;
  const bad = list.filter((j) => ['failed', 'cancelled', 'interrupted'].includes(j.state)).length;
  const pct = list.reduce((a, j) => a + pctOf(j), 0) / n;
  const speed = list.reduce((a, j) => a + (j.state === 'running' ? j.speed || 0 : 0), 0);
  const known = list.every((j) => j.bytes_total || j.state === 'done');
  const left = list.reduce((a, j) => a + (j.state === 'done' ? 0 : Math.max(0, j.bytes_total - j.bytes_done)), 0);
  const eta = act.length && speed > 0 && known ? fmtEta(left / speed) : '';
  const state = act.length ? (list.some((j) => j.state === 'running') ? 'running' : list.some((j) => j.state === 'paused') ? 'paused' : 'queued') : bad ? 'failed' : 'done';
  const j0 = list[0], c = jobCells({ ...j0, items: list.flatMap((j) => j.items || []), urls: list.flatMap((j) => j.urls || []) });
  const from = new Set(list.map((j) => jobCells(j).from)).size === 1 ? c.from : tr('task.variousDirs');
  const btns = act.length ? `<button data-jact="cancel-group" data-id="${g}" class="x" title="${tr('task.cancelGroup.title')}">✕ ${tr('task.all')}</button>` : '';
  return `<div class="trow group ${state}" data-group="${g}">
    <span class="c-fold">${folded ? '▸' : '▾'}</span>
    <span class="c-what"><small class="kind">${KIND(j0.kind)}</small>${esc(c.name)}</span>
    <span class="c-path">${esc(from)}</span>
    <span class="c-path">${esc(c.to)}</span>
    ${progressCell(pct, tr('task.groupDone', { done, n }), state)}
    <span class="c-num">${speed ? tr('common.perSec', { size: fmtSize(speed) }) : ''}</span>
    <span class="c-num">${eta}</span>
    <span class="c-state">${act.length ? STATE(state) : bad ? tr('task.failedN', { n: bad }) : STATE('done')}</span>
    <span class="c-btns">${btns}</span>
  </div>`;
}

function renderTasks() {
  const panel = $('#tasks');
  panel.classList.toggle('collapsed', !TK.open);
  panel.style.height = TK.open ? `${TK.height}px` : '';
  const active = TK.list.filter((j) => ACTIVE.includes(j.state)).length;
  $('#tasks-count').textContent = TK.list.length ? `· ${active ? tr('tasks.running', { n: active }) : tr('tasks.allDone')}` : '';
  $('#st-tasks').innerHTML = active ? `<i class="spin"></i>${tr('tasks.busy', { n: active })}` : tr('tasks.title');
  $('#st-tasks').classList.toggle('busy', !!active);
  $('#st-tasks').classList.toggle('on', TK.open);
  if (!TK.open) return;
  // новые сверху; задачи одной группы — вместе, под сводной строкой
  const blocks = [], groups = new Map();
  for (const j of TK.list.slice().reverse()) {
    if (!j.group) { blocks.push({ job: j }); continue; }
    if (!groups.has(j.group)) { const b = { group: j.group, list: [] }; groups.set(j.group, b); blocks.push(b); }
    groups.get(j.group).list.push(j);
  }
  const html = blocks.map((b) => {
    if (b.job) return jobRow(b.job, false);
    const list = b.list.slice().reverse(); // внутри группы — по порядку
    if (list.length === 1) return jobRow(list[0], false);
    // большие группы свёрнуты, пока их не раскроют
    const folded = TK.fold.has(b.group) || (list.length > 30 && !TK.unfold.has(b.group));
    return groupRow(b.group, list, folded) + (folded ? '' : list.map((j) => jobRow(j, true)).join(''));
  }).join('');
  $('#tasks-list').innerHTML = TK.list.length
    ? `<div class="thead"><span></span><span>${tr('task.col.what')}</span><span>${tr('task.col.from')}</span><span>${tr('task.col.to')}</span><span>${tr('task.col.done')}</span><span>${tr('task.col.speed')}</span><span>${tr('task.col.eta')}</span><span>${tr('task.col.state')}</span><span></span></div>${html}`
    : `<p class="muted t-empty">${tr('tasks.empty')}</p>`;
}
$('#tasks-clear').addEventListener('click', () => invoke('jobs_clear'));
$('#tasks-hide').addEventListener('click', () => tasksOpen(false));
$('#st-tasks').addEventListener('click', () => tasksOpen(!TK.open));
function jobAdded() {
  toast(TK.open ? tr('tasks.added') : tr('tasks.addedHint'));
}
$('#tasks-list').addEventListener('click', (e) => {
  const b = e.target.closest('[data-jact]');
  if (!b) {
    // клик по сводной строке группы — свернуть или развернуть
    const g = e.target.closest('.trow.group');
    if (!g) return;
    const id = +g.dataset.group;
    const folded = g.querySelector('.c-fold').textContent === '▸';
    if (folded) { TK.fold.delete(id); TK.unfold.add(id); } else { TK.fold.add(id); TK.unfold.delete(id); }
    renderTasks();
    return;
  }
  if (b.dataset.jact === 'cancel-group') { invoke('job_cancel_group', { group: +b.dataset.id }); return; }
  const id = +b.dataset.id;
  const j = TK.list.find((x) => x.id === id);
  if (b.dataset.jact === 'pause') invoke('job_pause', { id, pause: true });
  else if (b.dataset.jact === 'resume') invoke('job_pause', { id, pause: false });
  else if (b.dataset.jact === 'cancel') invoke('job_cancel', { id });
  else if (b.dataset.jact === 'retry') invoke('job_retry', { id });
  else if (b.dataset.jact === 'open' && j?.dest) call('open_path', { path: j.dest.path });
});
// высота панели — перетаскиванием верхнего края
$('#tasks-grip').addEventListener('mousedown', (e) => {
  e.preventDefault();
  if (!TK.open) tasksOpen(true);
  const y0 = e.clientY, h0 = TK.height;
  const move = (ev) => { TK.height = Math.max(90, Math.min(window.innerHeight - 220, h0 + (y0 - ev.clientY))); $('#tasks').style.height = `${TK.height}px`; };
  const up = () => {
    document.removeEventListener('mousemove', move);
    document.removeEventListener('mouseup', up);
    try { localStorage.setItem('filecat.tasksH', String(TK.height)); } catch { /* недоступно */ }
  };
  document.addEventListener('mousemove', move);
  document.addEventListener('mouseup', up);
});
let tasksDone = new Set();
listen('jobs', ({ payload }) => {
  TK.list = payload || [];
  renderTasks();
  // задача завершилась — открытая папка могла измениться
  const finished = TK.list.filter((j) => !ACTIVE.includes(j.state) && !tasksDone.has(j.id));
  if (finished.length) {
    finished.forEach((j) => tasksDone.add(j.id));
    // в найденных дубликатах удалённое и перемещённое больше не показываем
    const gone = new Set(finished.filter((j) => ['trash', 'delete', 'move'].includes(j.kind) && j.state !== 'cancelled' && j.state !== 'interrupted').flatMap((j) => (j.items || []).map((x) => x.path)));
    if (gone.size && Dz.groups.size) {
      for (const [id, g] of Dz.groups) {
        const files = g.files.filter((f) => !gone.has(f.full));
        if (files.length > 1) Dz.groups.set(id, { ...g, files });
        else Dz.groups.delete(id);
      }
    }
    if (Bz.data && !Bz.busy) { if (Bz.live) openLive(Bz.data.path, true); else openDir(Bz.dir, true); }
  }
});
(async () => {
  try { TK.list = await invoke('jobs_list'); } catch { TK.list = []; }
  TK.list.forEach((j) => { if (!ACTIVE.includes(j.state)) tasksDone.add(j.id); });
  renderTasks();
})();

listen('replicas-changed', () => { if (Fz.query && Fz.data?.everywhere) runFind(); });

// ---------- старт ----------
(async () => {
  await refreshStatus();
  tabsLoad();
  renderTabs();
  Bz.sources = await fetchSources();
  renderSourceList();
  const t = activeTab();
  if (!t.root || !Bz.sources.some((x) => x.key === t.root)) t.root = (Bz.sources.find((x) => x.indexed && !x.peer) || Bz.sources[0] || {}).key || null;
  await tabRestore(t);
  tabCapture();
  devLog(`интерфейс готов: источников ${Bz.sources.length}, папка ${Bz.data?.path || '—'}`);
})();

// устройство появилось в сети, ушло из неё или сопряжено новое
listen('peers-changed', () => { if ($('#tab-sources').classList.contains('active')) loadSources(); });
$('#src-list').addEventListener('contextmenu', (e) => {
  const g = e.target.closest('.grp.peer');
  if (!g) return;
  const p = Pz.info.peers.find((x) => x.id === g.dataset.peer);
  showMenu(e, [
    [tr('peer.ctx.access'), () => openShareModal(p)],
    [tr('ctx.rename'), () => renameInline(g, p.name, async (name) => { await call('p2p_rename', { peer: p.id, name }); await loadSources(); })],
    [tr('peer.ctx.unpair'), async () => {
      if (!(await ask(tr('peer.unpair.ask', { name: p.name }), { ok: tr('peer.unpair.ok'), danger: true }))) return;
      await call('p2p_remove', { peer: p.id });
      loadSources();
    }],
  ]);
});

// ---------- сопряжение с другим компьютером ----------
const PeerWait = { known: null };
$('#peer-invite').addEventListener('click', async () => {
  let code;
  const share = readSharePick($('#share-pick-invite'));
  if (!share.roots.length) { $('#peer-wait').textContent = tr('peer.pickOne'); return; }
  try { code = await invoke('p2p_invite', { share }); } catch (e) { $('#peer-wait').textContent = String(e); return; }
  $('#peer-code-out').value = code;
  $('#peer-copy').classList.remove('hidden');
  $('#peer-wait').textContent = tr('peer.waiting');
  // запоминаем, кто уже был, чтобы заметить нового
  PeerWait.known = new Set(Pz.info.peers.map((p) => p.id));
  $('#peer-code-out').select();
});
$('#peer-copy').addEventListener('click', () => {
  navigator.clipboard.writeText($('#peer-code-out').value).then(() => toast(tr('peer.codeCopied')));
});
$('#peer-join-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const code = $('#peer-code-in').value.trim();
  if (!code) return;
  $('#peer-join').disabled = true;
  $('#peer-join-status').textContent = tr('peer.connecting');
  try {
    const name = await invoke('p2p_join', { code });
    closeModal();
    toast(tr('peer.connected', { name }), 6000);
    await loadSources();
  } catch (err) {
    $('#peer-join-status').textContent = String(err);
  } finally {
    $('#peer-join').disabled = false;
  }
});
// пока открыт код-приглашение — ждём, когда другой компьютер его введёт
listen('peers-changed', async () => {
  if (!PeerWait.known || $('#m-peer').classList.contains('hidden')) return;
  const info = await invoke('p2p_info');
  const fresh = info.peers.find((p) => !PeerWait.known.has(p.id));
  if (!fresh) return;
  PeerWait.known = null;
  closeModal();
  toast(tr('peer.connected', { name: fresh.name }), 6000);
  loadSources();
});
// источник выбирается и с клавиатуры
$('#src-list').addEventListener('keydown', (e) => {
  const it = e.target.closest('.src-item');
  if (it && e.target === it && (e.key === 'Enter' || e.key === ' ')) { e.preventDefault(); it.click(); }
});

// ---------- Ctrl+V: файлы, скопированные в FileHunter, или ссылки из буфера обмена ----------
async function readClipboard() {
  try { return (await window.__TAURI__.clipboardManager.readText()) || ''; } catch { /* в буфере не текст */ }
  try { return (await navigator.clipboard.readText()) || ''; } catch { return ''; }
}
async function smartPaste() {
  if (!Bz.data) return;
  const text = (await readClipboard()).trim();
  const urls = text && text !== Clip.text ? [...new Set(text.match(/https?:\/\/[^\s"'<>]+/gi) || [])] : [];
  if (urls.length) return pasteLinks(urls);
  if (Clip.items.length) return clipPaste();
  toast(tr('paste.empty'));
}
async function pasteLinks(urls) {
  const dest = hereLoc();
  const where = (curSource()?.peer ? curSource().peerName + ' · ' : '') + dest.path;
  toast(urls.length === 1 ? tr('paste.checkingOne') : tr('paste.checkingN', { n: urls.length }), 30000);
  let probes;
  try { probes = await invoke('url_probe', { urls }); } catch (e) { toast(String(e), 6000); return; }
  const good = probes.filter((p) => p.ok && !p.html);
  const pages = probes.filter((p) => p.ok && p.html);
  const bad = probes.filter((p) => !p.ok);
  const line = (p) => `• ${p.name} — ${p.size != null ? fmtSize(p.size) : tr('paste.sizeUnknown')}${p.resumable ? '' : `, ${tr('paste.noResume')}`}`;
  let msg = '';
  if (good.length) msg += `${tr('paste.downloadTo', { where })}\n${good.map(line).join('\n')}`;
  if (pages.length) msg += `${msg ? '\n\n' : ''}${tr('paste.pages')}\n${pages.map((p) => `• ${p.url}`).join('\n')}`;
  if (bad.length) msg += `${msg ? '\n\n' : ''}${tr('paste.unavailable')}\n${bad.map((p) => `• ${p.url} — ${p.error}`).join('\n')}`;
  const take = good.length ? good : pages;
  if (!take.length) { toast(tr('paste.cannot', { errors: bad.map((p) => p.error).join('; ') }), 8000); return; }
  if (!(await ask(msg, { ok: good.length ? tr('paste.ok') : tr('paste.okAnyway') }))) return;
  try {
    await invoke('job_add', { kind: 'download', items: [], dest, urls: take.map((p) => p.url) });
    jobAdded();
  } catch (e) { toast(String(e), 6000); }
}
// Ctrl+V и когда фокус не в списке (но не в полях ввода — там обычная вставка)
document.addEventListener('keydown', (e) => {
  const k = e.key.toLowerCase();
  if (!e.ctrlKey || !(k === 'v' || k === 'м')) return;
  if (e.target.closest('input, textarea, #b-list') || !$('#tab-sources').classList.contains('active')) return;
  if (!$('#add-modal').classList.contains('hidden') || !$('#ask-modal').classList.contains('hidden')) return;
  e.preventDefault();
  smartPaste();
});

// ---------- что этот компьютер открывает другим ----------
// направление доступа: ← вы видите его, → он видит ваше, ⇄ в обе стороны
const sharesOut = (p) => !!(p.share && p.share.roots && p.share.roots.length && p.share.level && p.share.level !== 'none');
function accessDir(p, list) {
  const inn = list.length > 0, out = sharesOut(p);
  return inn && out ? '⇄' : inn ? '←' : out ? '→' : '—';
}
function accessHint(p, list) {
  const lines = [tr(p.online ? 'peer.online' : 'peer.offline', { name: p.name })];
  lines.push(list.length ? trn('peer.youSee', list.length) : tr('peer.youSeeNothing'));
  lines.push(sharesOut(p)
    ? tr('peer.itSees', { n: p.share.roots.length, level: tr(p.share.level === 'full' ? 'share.full' : 'share.viewOnly') })
    : tr('peer.itSeesNothing'));
  if (!list.length && !sharesOut(p)) lines.push(tr('peer.noAccess'));
  if (p.outdated) lines.push(tr('peer.outdated.hint'));
  lines.push('', tr('peer.ctxHint'));
  return lines.join('\n');
}
function shareLabel(sh) {
  if (!sh || !sh.roots || !sh.roots.length || sh.level === 'none' || !sh.level) return tr('share.none');
  return tr('share.label', { n: sh.roots.length, level: tr(sh.level === 'full' ? 'share.full' : 'share.view') });
}
// список своих источников с галочками и уровень доступа
function renderSharePick(box, share, allowNone = true) {
  const mine = Bz.sources.filter((s) => !s.peer);
  const has = (r) => (share.roots || []).some((x) => x.toLowerCase() === r.toLowerCase());
  const level = share.level && share.level !== 'none' ? share.level : 'view';
  box.innerHTML = `
    <div class="sp-list">${mine.map((s) => `<label class="sp-item"><input type="checkbox" value="${esc(s.root)}" ${has(s.root) ? 'checked' : ''}>
      <span><b>${esc(s.title)}</b> <small class="muted">${esc(s.kind === 's3' ? tr('share.s3') : s.root)}</small></span></label>`).join('')}</div>
    <div class="sp-level">
      <label><input type="radio" name="lv-${box.id}" value="view" ${level === 'view' ? 'checked' : ''}> ${tr('share.level.view')}</label>
      <label><input type="radio" name="lv-${box.id}" value="full" ${level === 'full' ? 'checked' : ''}> ${tr('share.level.full')}</label>
    </div>`;
}
function readSharePick(box) {
  const roots = [...box.querySelectorAll('.sp-list input:checked')].map((i) => i.value);
  const level = box.querySelector('.sp-level input:checked')?.value || 'view';
  return { level: roots.length ? level : 'none', roots };
}
let shareFor = null;
async function openShareModal(p) {
  shareFor = p;
  let share = p.share;
  try { share = await invoke('p2p_share_get', { peer: p.id }); } catch { /* возьмём из списка */ }
  $('#share-title').textContent = tr('share.titleFor', { name: p.name });
  renderSharePick($('#share-pick-edit'), share || { level: 'none', roots: [] });
  $('#share-modal').classList.remove('hidden');
}
$('#share-cancel').addEventListener('click', () => $('#share-modal').classList.add('hidden'));
$('#share-save').addEventListener('click', async () => {
  const share = readSharePick($('#share-pick-edit'));
  try { await invoke('p2p_share_set', { peer: shareFor.id, share }); } catch (e) { toast(String(e), 6000); return; }
  $('#share-modal').classList.add('hidden');
  toast(share.roots.length ? tr('share.saved', { name: shareFor.name, n: share.roots.length }) : tr('share.savedNone', { name: shareFor.name }), 5000);
  loadSources();
});

// F5 — перезагрузить интерфейс (в тестовой версии подхватывает правки из папки проекта)
document.addEventListener('keydown', (e) => { if (e.key === 'F5') { e.preventDefault(); location.reload(); } });
