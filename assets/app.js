'use strict';
const PAGE = 50, GALLERY_PAGE = 60;
const state = {
  archive: null,
  start: 0, total: 0,
  mode: 'messages',
  searching: false,
  galleryLoaded: 0, galleryTotal: 0, year: null,
  replyCache: new Map(),
};
const $ = (id) => document.getElementById(id);
const els = {
  select: $('archive-select'), search: $('search'), searchBtn: $('search-btn'), searchClear: $('search-clear'),
  dateFrom: $('date-from'), dateTo: $('date-to'), fAtt: $('f-att'), fLink: $('f-link'), fPin: $('f-pin'),
  viewInfo: $('view-info'), list: $('message-list'), pagerTop: $('pager-top'), pagerBottom: $('pager-bottom'),
  gallery: $('gallery'), yearBar: $('year-bar'), galleryMore: $('gallery-more'), stats: $('stats'),
};

async function api(path) {
  let url = path;
  if (state.archive) url += (path.includes('?') ? '&' : '?') + 'archive=' + encodeURIComponent(state.archive);
  const r = await fetch(url);
  if (!r.ok) {
    let msg = r.statusText;
    try { msg = (await r.json()).error || msg; } catch (e) {}
    throw new Error(msg);
  }
  return r.json();
}

function filterQS() {
  const p = [];
  if (els.dateFrom.value) p.push('from=' + els.dateFrom.value);
  if (els.dateTo.value) p.push('to=' + els.dateTo.value);
  if (els.fAtt.checked) p.push('has_attachment=true');
  if (els.fLink.checked) p.push('has_link=true');
  if (els.fPin.checked) p.push('pinned=true');
  return p.length ? '&' + p.join('&') : '';
}

function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}
function fmtTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  return isNaN(d) ? esc(ts) : d.toLocaleString();
}
function linkify(text) {
  return esc(text).replace(/https?:\/\/\S+/gi, (m) => '<a href="' + m + '" target="_blank" rel="noopener">' + m + '</a>');
}
function extOf(name) {
  const m = /\.([A-Za-z0-9]{1,5})$/.exec(name || '');
  return m ? m[1].toLowerCase() : '';
}
const IMAGE_EXTS = new Set(['png','jpg','jpeg','gif','webp','bmp']);
const VIDEO_EXTS = new Set(['mp4','webm','mov','mkv']);

// -- archives ---------------------------------------------------------------
async function loadArchives() {
  const data = await api('/api/archives');
  els.select.innerHTML = '';
  for (const a of data.archives) {
    const opt = document.createElement('option');
    opt.value = a.name;
    opt.textContent = a.chat + ' (' + a.name + ')';
    els.select.appendChild(opt);
  }
  if (data.selected) {
    state.archive = data.selected;
    els.select.value = data.selected;
  } else if (data.archives.length) {
    await selectArchive(data.archives[0].name);
    return;
  }
  await refresh();
}

async function selectArchive(name) {
  await api('/api/select?archive=' + encodeURIComponent(name));
  state.archive = name;
  state.start = 0; state.searching = false;
  els.select.value = name;
  await refresh();
}

// -- messages ---------------------------------------------------------------
async function loadMessages(start) {
  const data = await api(`/api/messages?start=${start}&count=${PAGE}${filterQS()}`);
  state.start = data.start; state.total = data.total;
  renderMessages(data.messages, null);
  renderPagers();
  els.viewInfo.textContent = `${data.total} message(s)` + (filterQS() ? ' (filtered)' : '');
}

async function jumpTo(id) {
  const data = await api(`/api/messages?anchor=${encodeURIComponent(id)}&count=${PAGE}${filterQS()}`);
  state.start = data.start; state.total = data.total;
  renderMessages(data.messages, id);
  renderPagers();
  const el = els.list.querySelector('.msg.anchor');
  if (el) el.scrollIntoView({ block: 'center' });
}

function renderMessages(messages, anchorId) {
  els.list.innerHTML = '';
  for (const msg of messages) els.list.appendChild(renderMessage(msg, anchorId === msg.id));
  if (!messages.length) els.list.innerHTML = '<p style="color:var(--fg-dim)">No messages.</p>';
}

function renderMessage(msg, isAnchor) {
  const wrap = document.createElement('div');
  wrap.className = 'msg' + (isAnchor ? ' anchor' : '');
  wrap.dataset.id = msg.id;
  const head = document.createElement('div');
  head.className = 'head';
  head.innerHTML = `<span class="author">${esc(msg.author)}</span><span>${fmtTime(msg.timestamp)}${msg.edited_timestamp ? ' (edited)' : ''}</span>` +
    (msg.pinned ? '<span class="pinned">pinned</span>' : '');
  wrap.appendChild(head);
  if (msg.reply_to) wrap.appendChild(replyLine(msg.reply_to));
  const content = document.createElement('div');
  content.className = 'content';
  content.innerHTML = linkify(msg.content || '');
  wrap.appendChild(content);
  if (msg.attachments && msg.attachments.length) {
    const atts = document.createElement('div');
    atts.className = 'atts';
    for (const att of msg.attachments) {
      if (IMAGE_EXTS.has(extOf(att.filename || att.url))) {
        const img = document.createElement('img');
        img.src = att.url; img.alt = att.filename || ''; img.loading = 'lazy';
        atts.appendChild(img);
      } else {
        const a = document.createElement('a');
        a.href = att.url + '?download'; a.textContent = att.filename || att.url;
        atts.appendChild(a);
      }
    }
    wrap.appendChild(atts);
  }
  return wrap;
}

function replyLine(replyId) {
  const line = document.createElement('div');
  line.className = 'reply-line';
  line.innerHTML = '<span>&#8627; </span><span class="reply-body">loading&hellip;</span>';
  line.addEventListener('click', () => jumpTo(replyId));
  resolveReply(replyId).then((info) => {
    const body = line.querySelector('.reply-body');
    if (info) {
      body.innerHTML = `<span class="reply-author">${esc(info.author || 'unknown')}</span>: ${esc((info.content || '').slice(0, 80))}`;
    } else {
      body.textContent = '(message not in archive)';
    }
  });
  return line;
}

async function resolveReply(id) {
  if (state.replyCache.has(id)) return state.replyCache.get(id);
  try {
    const data = await api('/api/resolve?ids=' + encodeURIComponent(id));
    const info = data[id] || null;
    state.replyCache.set(id, info);
    return info;
  } catch (e) { return null; }
}

function renderPagers() {
  const mk = () => {
    const frag = document.createDocumentFragment();
    const newer = document.createElement('button');
    newer.textContent = 'Newer';
    newer.disabled = state.start <= 0;
    newer.addEventListener('click', () => loadMessages(Math.max(0, state.start - PAGE)));
    const older = document.createElement('button');
    older.textContent = 'Older';
    older.disabled = state.start + PAGE >= state.total;
    older.addEventListener('click', () => loadMessages(state.start + PAGE));
    frag.appendChild(newer); frag.appendChild(older);
    return frag;
  };
  els.pagerTop.innerHTML = ''; els.pagerTop.appendChild(mk());
  els.pagerBottom.innerHTML = ''; els.pagerBottom.appendChild(mk());
}

// -- search -----------------------------------------------------------------
async function doSearch() {
  const q = els.search.value.trim();
  if (!q) return;
  state.searching = true;
  els.searchClear.classList.remove('hidden');
  const data = await api('/api/search?q=' + encodeURIComponent(q) + filterQS());
  els.list.innerHTML = '';
  if (!data.results.length) {
    els.list.innerHTML = '<p style="color:var(--fg-dim)">No matches.</p>';
  }
  for (const r of data.results) {
    const wrap = document.createElement('div');
    wrap.className = 'msg';
    wrap.innerHTML = `<div class="head"><span class="author">${esc(r.author)}</span><span>${fmtTime(r.timestamp)}</span></div>` +
      `<div class="content">${linkify(r.content)}</div>`;
    wrap.addEventListener('click', () => jumpTo(r.id));
    els.list.appendChild(wrap);
  }
  els.pagerTop.innerHTML = ''; els.pagerBottom.innerHTML = '';
  els.viewInfo.textContent = `${data.results.length} match(es)${data.truncated ? ' (truncated)' : ''} for "${esc(data.query)}"`;
}

function clearSearch() {
  state.searching = false;
  els.search.value = '';
  els.searchClear.classList.add('hidden');
  loadMessages(0);
}

// -- gallery ----------------------------------------------------------------
async function loadYears() {
  const data = await api('/api/attachment-years');
  els.yearBar.innerHTML = '';
  const all = document.createElement('span');
  all.className = 'chip' + (state.year === null ? ' active' : '');
  all.textContent = 'all';
  all.addEventListener('click', () => { state.year = null; state.galleryLoaded = 0; refreshGallery(); });
  els.yearBar.appendChild(all);
  for (const y of data.years) {
    const chip = document.createElement('span');
    chip.className = 'chip' + (state.year === y.year ? ' active' : '');
    chip.textContent = `${y.year} (${y.count})`;
    chip.addEventListener('click', () => { state.year = y.year; state.galleryLoaded = 0; refreshGallery(); });
    els.yearBar.appendChild(chip);
  }
}

async function refreshGallery() {
  await loadYears();
  els.gallery.innerHTML = '';
  state.galleryLoaded = 0;
  await loadMoreGallery();
}

async function loadMoreGallery() {
  const yq = state.year ? `&year=${encodeURIComponent(state.year)}` : '';
  const data = await api(`/api/attachments?start=${state.galleryLoaded}&count=${GALLERY_PAGE}${yq}`);
  state.galleryLoaded = data.start + data.attachments.length;
  state.galleryTotal = data.total;
  els.galleryMore.disabled = state.galleryLoaded >= data.total;
  for (const att of data.attachments) {
    const item = document.createElement('div');
    item.className = 'g-item';
    const ext = extOf(att.filename || att.url);
    if (IMAGE_EXTS.has(ext)) {
      const img = document.createElement('img');
      img.src = att.url; img.loading = 'lazy';
      item.appendChild(img);
    } else if (VIDEO_EXTS.has(ext)) {
      const v = document.createElement('video');
      v.src = att.url; v.controls = true; v.preload = 'none';
      item.appendChild(v);
    }
    const name = document.createElement('div');
    name.className = 'g-name';
    name.textContent = `${att.filename || att.url} (${att.date || '?'})`;
    item.appendChild(name);
    if (att.message_id) item.addEventListener('click', () => { showPane('messages'); jumpTo(att.message_id); });
    els.gallery.appendChild(item);
  }
}

// -- stats ------------------------------------------------------------------
async function loadStats() {
  const s = await api('/api/stats');
  const grid = (items) => '<div class="stat-grid">' + items.map(([l, n]) =>
    `<div class="stat"><div class="n">${n}</div><div class="l">${esc(l)}</div></div>`).join('') + '</div>';
  const table = (title, rows, cols) => {
    if (!rows.length) return '';
    return `<h3>${esc(title)}</h3><table><tr>${cols.map((c) => `<th>${esc(c)}</th>`).join('')}</tr>` +
      rows.map((r) => '<tr>' + cols.map((c) => `<td>${esc(r[c])}</td>`).join('') + '</tr>').join('') + '</table>';
  };
  els.stats.innerHTML =
    grid([
      ['messages', s.total_messages],
      ['attachments', s.total_attachments],
      ['attachment bytes', s.attachment_bytes],
      ['edited', s.edited], ['replies', s.replies], ['pinned', s.pinned], ['with attachments', s.with_attachments],
    ]) +
    table('Authors', s.authors, ['name', 'count']) +
    table('Messages per month', s.months, ['month', 'count']) +
    table('Attachment types', s.attachment_types, ['ext', 'count']);
}

// -- wiring -----------------------------------------------------------------
function showPane(mode) {
  state.mode = mode;
  for (const [btn, pane] of [['view-messages', 'pane-messages'], ['view-gallery', 'pane-gallery'], ['view-stats', 'pane-stats']]) {
    $(btn).classList.toggle('active', btn === 'view-' + mode);
    $(pane).classList.toggle('hidden', btn !== 'view-' + mode);
  }
  if (mode === 'gallery') refreshGallery();
  if (mode === 'stats') loadStats();
}

async function refresh() {
  if (state.searching && els.search.value.trim()) return doSearch();
  if (state.mode === 'messages') await loadMessages(state.start || 0);
  else if (state.mode === 'gallery') await refreshGallery();
  else await loadStats();
}

els.select.addEventListener('change', () => selectArchive(els.select.value));
els.searchBtn.addEventListener('click', doSearch);
els.searchClear.addEventListener('click', clearSearch);
els.search.addEventListener('keydown', (e) => { if (e.key === 'Enter') doSearch(); });
for (const el of [els.dateFrom, els.dateTo, els.fAtt, els.fLink, els.fPin]) {
  el.addEventListener('change', () => { state.start = 0; refresh(); });
}
$('view-messages').addEventListener('click', () => showPane('messages'));
$('view-gallery').addEventListener('click', () => showPane('gallery'));
$('view-stats').addEventListener('click', () => showPane('stats'));
els.galleryMore.addEventListener('click', loadMoreGallery);


