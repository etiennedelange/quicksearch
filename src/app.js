const MIN_QUERY_CHARS = 3;
const DEBOUNCE_MS = 120;
const ZOOM_MIN = 0.6;
const ZOOM_MAX = 3.0;
const ZOOM_STEP = 0.1;
const DIVIDER_MIN_RATIO = 0.25;
const DIVIDER_MAX_RATIO = 0.85;

const state = {
  config: null,
  mode: 'files', // 'files' | 'folders' | 'both' | 'content'
  searchRegex: false,
  caseMode: 'smart', // 'smart' | 'sensitive' | 'insensitive'
  disabledPaths: [],
  results: [],
  selIndex: 0,
  generation: 0,
  // Searches asked for whose `qs-begin` hasn't come back yet. While this is
  // above zero, `generation` still names the *previous* search, so nothing
  // arriving can be assumed to belong to the query now in the box.
  pendingBegins: 0,
  stateWord: 'IDLE',
  indexStatus: null,
  debounceTimer: null,
  searchStartedAt: null,
  elapsed: 0,
  awaitingFreshBegin: false,
  failedGeneration: null,
};

// 'files', 'folders', and 'both' all search the same background filename
// index (see filecache.rs) rather than walking file contents with rg —
// everywhere that distinction matters (starting index polling, the INDEXING
// state word, the no-excerpt layout) keys off this instead of enumerating
// every non-content mode by name.
function isIndexMode(mode) {
  return mode === 'files' || mode === 'folders' || mode === 'both';
}

const FLUSH_MS = 80; // matches quicksearch's own _flush_results cadence

let indexPollTimer = null;
let elapsedTickTimer = null;

// Self-rescheduling wall-clock tick so `elapsed` visibly counts up on its
// own — mirrors quicksearch's `_flush_results`, which reschedules itself via
// `root.after(FLUSH_MS, ...)` unconditionally while a search is in flight,
// not just when a Tauri event happens to arrive. Without this, the
// colophon only repaints on qs-batch, so a stretch of the walk that isn't
// producing new matches reads as frozen even though the search is still
// running.
function startElapsedTick() {
  stopElapsedTick();
  elapsedTickTimer = setInterval(() => {
    updateElapsed();
    refreshColophon();
  }, FLUSH_MS);
}

function stopElapsedTick() {
  if (elapsedTickTimer) {
    clearInterval(elapsedTickTimer);
    elapsedTickTimer = null;
  }
}

function startIndexPolling() {
  stopIndexPolling();
  const poll = () => {
    invoke('get_index_status').then((status) => {
      state.indexStatus = status;
      refreshColophon();
    });
  };
  poll();
  indexPollTimer = setInterval(poll, 300);
}

function stopIndexPolling() {
  if (indexPollTimer) {
    clearInterval(indexPollTimer);
    indexPollTimer = null;
  }
}

// Every example is common-denominator regex syntax valid in both the
// content-search engine (rg's own Rust `regex` crate) and the filename
// engine (the `regex` crate — the same crate ripgrep itself uses), so one
// list serves both modes' tooltip.
const REGEX_EXAMPLES = [
  ['.', 'any character'],
  ['.*', 'zero or more of anything'],
  ['\\d+', 'one or more digits'],
  ['\\w+', 'word characters'],
  ['^', 'start of line'],
  ['$', 'end of line'],
  ['foo|bar', '"foo" or "bar"'],
  ['[abc]', 'one of a, b, c'],
  ['\\bTODO\\b', 'whole word "TODO"'],
];

// The verified-working subset of the original's keyboard legend — only
// bindings this port actually implements are listed, so the footer never
// promises a shortcut that does nothing.
const FOOTER_LEGEND = [
  ['Enter', 'open'],
  ['^Enter', 'folder'],
  ['Tab', 'mode'],
  ['Alt+R', 'regex'],
  ['Alt+C', 'case'],
  ['Alt+M', 'mounts'],
  ['↑↓', 'select'],
  ['Esc', 'dismiss'],
];

async function invoke(cmd, payload = {}) {
  return window.__TAURI__.core.invoke(cmd, payload);
}

// Serialize preference writes and make failure visible. Source/match controls
// adopt their new state only after a successful save; zoom/divider previews
// are restored from the backend if their save fails.
let runtimeSaveQueue = Promise.resolve();
function saveRuntimeState(patch) {
  const operation = runtimeSaveQueue.then(async () => {
    try {
      const config = await invoke('save_runtime_state', { patch });
      for (const key of Object.keys(patch)) state.config[key] = config[key];
      return config;
    } catch (error) {
      try {
        const config = await invoke('get_config');
        for (const key of Object.keys(patch)) state.config[key] = config[key];
        if ('zoom' in patch) applyZoom(config.zoom, false);
        if ('divider_ratio' in patch) setDividerRatio(config.divider_ratio);
      } catch (_) {
        // The original write failure is the useful error to show.
      }
      showNotice(`Settings were not saved: ${error}`);
      return null;
    }
  });
  runtimeSaveQueue = operation.then(() => {}, () => {});
  return operation;
}

async function init() {
  state.config = await invoke('get_config');
  state.searchRegex = !!state.config.search_regex;
  state.caseMode = state.config.case_mode || 'smart';
  state.disabledPaths = state.config.disabled_paths || [];
  applyZoom(state.config.zoom || 1.0, false);
  setDividerRatio(state.config.divider_ratio || 0.63);
  setupDividerDrag();

  const input = document.getElementById('queryInput');

  // The search listeners are registered — and awaited — before anything can
  // dispatch a search. `pendingBegins` is only ever cleared by `qs-begin`, so
  // a begin that arrived before its listener existed would leave the count
  // stuck above zero and no result would ever render again.
  await Promise.all([
    window.__TAURI__.event.listen('qs-begin', (event) => {
      // Emitted synchronously before the search worker thread spawns — using
      // this (rather than the `search` invoke's own resolved promise) to
      // update state.generation closes a race where a late-arriving event
      // from a just-superseded generation could still pass the `generation
      // !== state.generation` check because the invoke's `.then()` hadn't
      // run yet, and get rendered as if it belonged to the new search.
      if (event.payload > state.generation) {
        state.generation = event.payload;
      }
      state.awaitingFreshBegin = false;
      state.pendingBegins = Math.max(0, state.pendingBegins - 1);
    }),

    window.__TAURI__.event.listen('qs-batch', (event) => {
      const { generation, rows } = event.payload;
      if (!isCurrentGeneration(generation)) return;
      state.results.push(...rows);
      updateElapsed();
      renderList();
      refreshColophon();
    }),

    window.__TAURI__.event.listen('qs-done', (event) => {
      const { generation, idle } = event.payload;
      if (!isCurrentGeneration(generation)) return;
      if (state.failedGeneration === generation) return;
      state.stateWord = idle ? 'IDLE' : state.results.length > 0 ? 'DONE' : 'EMPTY';
      stopElapsedTick();
      updateElapsed();
      refreshColophon();
      renderList();
    }),

    window.__TAURI__.event.listen('qs-error', (event) => {
      const { generation, message } = event.payload;
      if (!isCurrentGeneration(generation)) return;
      state.stateWord = 'FAULT';
      state.failedGeneration = generation;
      stopElapsedTick();
      showNotice(message);
      refreshColophon();
    }),

    window.__TAURI__.event.listen('qs-warning', (event) => {
      const { generation, message } = event.payload;
      if (!isCurrentGeneration(generation)) return;
      showNotice(message, 'warning');
      refreshColophon();
    }),
  ]);

  input.addEventListener('input', onInputChanged);
  input.focus();

  setupHeadwordCaret(input);

  document.addEventListener('keydown', onKeyDown);
  document.addEventListener('wheel', onWheel, { passive: false });
  document.addEventListener('mousedown', (e) => {
    if (!e.target.closest('#contextMenu')) closeContextMenu();
  });
  // Capturing, not bubbling: the result list's own scroll doesn't bubble,
  // and a menu left floating over rows it no longer points at is worse than
  // just closing it.
  document.addEventListener('scroll', closeContextMenu, true);

  renderColophon();
  renderList();
  renderFooter();

  window.__TAURI__.event.listen('tauri://focus', () => {
    input.focus();
    input.select();
    // Polling has to be restarted here, not only in toggleMode: the window
    // is dismissed by paths that never reach this script (the tray icon, the
    // global hotkey, the title bar, Alt+F4), and stopIndexPolling on the way
    // out would otherwise leave the index reading frozen on the way back in.
    if (isIndexMode(state.mode)) startIndexPolling();
  });

  window.__TAURI__.event.listen('tauri://blur', () => {
    // Nothing here is worth a 300ms IPC round-trip while the page is not
    // being looked at.
    stopIndexPolling();
    closeContextMenu();
  });

  // Pushed the moment the watcher patches the index or a full rebuild
  // starts — a real signal, not a guess from polling silence. Only visible
  // if the index reading is currently on screen; missed while in content
  // mode is fine, there's nothing to flash there anyway.
  window.__TAURI__.event.listen('qs-index-activity', () => {
    pulseIndexDot();
  });

  if (isIndexMode(state.mode)) startIndexPolling();

  // The window is created hidden (`"visible": false` in tauri.conf.json) and
  // this is what reveals it. Everything above has already run — config
  // loaded, zoom and divider applied, colophon/list/footer rendered — so the
  // window appears once, at its saved size, with the real UI in it, instead
  // of appearing at the 900x560 default, painting white, and then jumping.
  //
  // Deliberately a direct call rather than waiting on requestAnimationFrame:
  // a hidden window's frame callbacks can be throttled indefinitely, so
  // waiting for one risks never revealing the window at all. The configured
  // window background colour covers the frame before the first paint lands.
  invoke('frontend_ready');
}

// Whether an event belongs to the query currently in the box.
//
// The generation equality alone isn't enough: `runSearch` clears the results
// synchronously but `state.generation` only catches up when `qs-begin`
// arrives, so in between it still names the search that was just
// superseded — and a batch that search's worker emitted just before being
// killed would match it and land in the list we had just emptied.
function isCurrentGeneration(generation) {
  return !state.awaitingFreshBegin && state.pendingBegins === 0 && generation === state.generation;
}

function onInputChanged() {
  updateHeadwordCaret();
  clearTimeout(state.debounceTimer);
  // Invalidate the old query immediately. The backend cancellation is cheap
  // and prevents a whole-drive content search from continuing during typing;
  // the new query remains debounced below.
  state.generation += 1;
  state.awaitingFreshBegin = true;
  state.failedGeneration = null;
  invoke('cancel_search').catch(() => {});
  const configuredContentDebounce = Number(state.config?.content_search_debounce_ms);
  const contentDebounce = Number.isFinite(configuredContentDebounce)
    ? Math.min(2000, Math.max(200, configuredContentDebounce))
    : 450;
  const delay = state.mode === 'content' && state.config?.content_search_quiet
    ? contentDebounce
    : DEBOUNCE_MS;
  state.debounceTimer = setTimeout(runSearch, delay);
}

let caretMeasureCanvas = null;

function setupHeadwordCaret(input) {
  caretMeasureCanvas = document.createElement('canvas');

  const reposition = () => updateHeadwordCaret();
  input.addEventListener('click', reposition);
  input.addEventListener('keyup', reposition);
  input.addEventListener('focus', reposition);
  input.addEventListener('select', reposition);

  updateHeadwordCaret();
}

function updateHeadwordCaret() {
  const input = document.getElementById('queryInput');
  const caret = document.getElementById('headwordCaret');
  if (document.activeElement !== input) {
    caret.style.display = 'none';
    return;
  }
  caret.style.display = '';

  const ctx = caretMeasureCanvas.getContext('2d');
  ctx.font = getComputedStyle(input).font;
  const upToCursor = input.value.slice(0, input.selectionStart ?? 0);
  const textWidth = ctx.measureText(upToCursor).width;

  const fontSizePx = parseFloat(getComputedStyle(input).fontSize);
  caret.style.left = `${textWidth}px`;
  caret.style.height = `${Math.round(fontSizePx * 1.05)}px`;

  // Restart the blink animation so the caret is solid the instant the
  // cursor moves, then resumes blinking — matches a real text cursor's
  // "don't blink away right after you moved it" behavior.
  caret.style.animation = 'none';
  void caret.offsetWidth; // force reflow so the animation restart takes effect
  caret.style.animation = '';
}

function updateElapsed() {
  if (state.searchStartedAt) {
    state.elapsed = (Date.now() - state.searchStartedAt) / 1000;
  }
}

function runSearch() {
  const pattern = document.getElementById('queryInput').value.trim();
  hideNotice();
  state.debounceTimer = null;

  if (pattern.length < MIN_QUERY_CHARS) {
    state.results = [];
    state.selIndex = 0;
    state.stateWord = 'IDLE';
    state.searchStartedAt = null;
    state.elapsed = 0;
    stopElapsedTick();
    renderColophon();
    renderList();
    // Still tell the backend: search::start bumps the generation and kills
    // whatever rg is still running for the previous query before checking
    // the length itself, so without this call an in-flight rg from the last
    // valid query is orphaned rather than stopped (mirrors quicksearch.py's
    // cancel_search(idle=True) on the emptied-box case).
    dispatchSearch(pattern);
    return;
  }

  state.results = [];
  state.selIndex = 0;
  state.searchStartedAt = Date.now();
  state.elapsed = 0;
  state.stateWord = isIndexMode(state.mode) ? 'INDEXING' : 'WALKING';
  startElapsedTick();
  renderColophon();
  renderList();

  dispatchSearch(pattern);
}

function dispatchSearch(pattern) {
  state.awaitingFreshBegin = true;
  state.pendingBegins += 1;
  invoke('search', {
    pattern,
    mode: state.mode,
    searchRegex: state.searchRegex,
    caseMode: state.caseMode,
  })
    .then((generation) => {
      if (generation > state.generation) {
        state.generation = generation;
      }
    })
    // `qs-begin` is what normally clears the pending count, and the backend
    // emits it before this promise can settle. A rejected invoke never emits
    // one, so it has to be cleared here or nothing would ever render again.
    .catch(() => {
      state.pendingBegins = Math.max(0, state.pendingBegins - 1);
    });
}

// Value nodes the elapsed tick writes into directly. Rebuilding the whole
// colophon at the tick's 12.5Hz was both wasteful and wrong: it destroyed the
// element the pointer was hovering, so `mouseleave` never fired on it and the
// regex tooltip stuck on screen.
const colophonNodes = {
  shape: null,
  stateWord: null,
  stateDot: null,
  found: null,
  elapsed: null,
  index: null,
  indexDot: null,
};

let indexPulseTimer = null;

function isSearchRunning() {
  return state.stateWord === 'INDEXING' || state.stateWord === 'WALKING';
}

function createActivityDot() {
  const dot = document.createElement('span');
  dot.className = 'activity-dot dot-hidden';
  return dot;
}

// Reconciles both activity dots against current state — cheap enough to run
// on every colophon repaint (the elapsed tick included) alongside the plain
// textContent writes, so "a search is running" and "the index just moved"
// never lag behind what's actually happening.
function syncActivityDots() {
  if (colophonNodes.stateDot) {
    const running = isSearchRunning();
    colophonNodes.stateDot.classList.toggle('dot-hidden', !running);
    colophonNodes.stateDot.classList.toggle('blinking', running);
  }
  if (colophonNodes.indexDot) {
    const status = state.indexStatus;
    const ready = !!status && status.state === 'ready';
    const building = !!status && status.state === 'building';
    const refreshing = !!status && !!status.refreshing;
    colophonNodes.indexDot.classList.toggle('dot-hidden', !ready && !building && !refreshing);
    colophonNodes.indexDot.classList.toggle('dot-outline', ready && !status.watched);
    colophonNodes.indexDot.classList.toggle('dot-watched', ready && !!status.watched);
    colophonNodes.indexDot.classList.toggle('blinking', building || refreshing);
    colophonNodes.indexDot.classList.toggle('dot-ready', ready && !refreshing);
    colophonNodes.indexDot.classList.toggle('dot-error', ready && !!status?.error);
    if (refreshing) {
      colophonNodes.indexDot.title = 'refreshing the filename index now';
    } else if (ready) {
      const minutes = state.config?.filename_index_refresh_minutes || 60;
      colophonNodes.indexDot.title = status.watched
        ? 'watching this source live for changes'
        : `not watched live — rechecked every ${minutes} min`;
      if (status.error) colophonNodes.indexDot.title += `; last save failed: ${status.error}`;
    }
  }
}

// A one-off flash for a watcher patch or a rebuild starting — distinct from
// the steady `blinking` a running search gets. Restarting a still-running
// animation needs a reflow in between, or the browser coalesces the
// class-remove/class-add into nothing and the flash never restarts.
function pulseIndexDot() {
  const dot = colophonNodes.indexDot;
  if (!dot) return;
  dot.classList.remove('pulsing');
  void dot.offsetWidth;
  dot.classList.add('pulsing');
  clearTimeout(indexPulseTimer);
  indexPulseTimer = setTimeout(() => dot.classList.remove('pulsing'), 1200);
}

// Everything that changes the colophon's *structure* rather than the text
// inside it. A change here forces a full rebuild; anything else is a
// textContent write.
function colophonShape() {
  return JSON.stringify([
    state.mode,
    state.searchRegex,
    state.caseMode,
    state.config?.paths || [],
    state.disabledPaths,
  ]);
}

function indexReadingText() {
  const status = state.indexStatus;
  if (!status) return '—';
  if (status.state === 'ready') {
    if (status.refreshing) return `${status.count.toLocaleString()} paths · refreshing`;
    return status.error
      ? `${status.count.toLocaleString()} paths · save failed`
      : `${status.count.toLocaleString()} paths`;
  }
  if (status.state === 'building') return `building — ${status.count.toLocaleString()} paths so far`;
  return 'cold';
}

// The cheap path: update the readings that actually move during a search.
function refreshColophon() {
  if (colophonShape() !== colophonNodes.shape) {
    renderColophon(); // syncs the activity dots itself
    return;
  }
  if (colophonNodes.stateWord) colophonNodes.stateWord.textContent = state.stateWord;
  if (colophonNodes.found) colophonNodes.found.textContent = String(state.results.length);
  if (colophonNodes.elapsed) colophonNodes.elapsed.textContent = `${state.elapsed.toFixed(1)}s`;
  if (colophonNodes.index) colophonNodes.index.textContent = indexReadingText();
  syncActivityDots();
}

function modeLabel(mode) {
  return mode === 'files' ? 'Filenames'
    : mode === 'folders' ? 'Folders'
    : mode === 'both' ? 'Filenames + Folders'
    : 'Contents';
}

function renderColophon() {
  document.getElementById('modeIndicator').textContent = modeLabel(state.mode);
  document.body.classList.toggle('files-mode', isIndexMode(state.mode));

  const el = document.getElementById('colophon');
  // This replaces whatever the pointer may be hovering, so no `mouseleave`
  // will ever fire for it.
  hideTooltip();
  el.innerHTML = '';
  colophonNodes.stateWord = null;
  colophonNodes.stateDot = null;
  colophonNodes.found = null;
  colophonNodes.elapsed = null;
  colophonNodes.index = null;
  colophonNodes.indexDot = null;

  const addReading = (key, value, opts = {}) => {
    if (el.children.length > 0) {
      const sep = document.createElement('span');
      sep.className = 'reading-sep';
      sep.textContent = '·';
      el.appendChild(sep);
    }
    const wrap = document.createElement('span');
    wrap.className = 'reading';
    const label = document.createElement('span');
    label.className = 'reading-key';
    label.textContent = `${key}: `;
    wrap.appendChild(label);
    const val = document.createElement('span');
    val.className = 'reading-value' + (opts.bold ? ' reading-bold' : '');
    val.textContent = value;
    wrap.appendChild(val);
    el.appendChild(wrap);
    return { wrap, value: val };
  };

  const stateReading = addReading('state', state.stateWord);
  colophonNodes.stateWord = stateReading.value;
  colophonNodes.stateDot = createActivityDot();
  stateReading.wrap.appendChild(colophonNodes.stateDot);

  {
    colophonNodes.found = addReading('found', String(state.results.length)).value;
    colophonNodes.elapsed = addReading('elapsed', `${state.elapsed.toFixed(1)}s`).value;
    {
      // Filenames/Folders mode has no rg --stats reading at all (it never
      // runs rg in JSON/stats mode) — it shows the shared name index's own
      // state instead, matching the original's `_render_colophon` files-mode
      // branch (`index: {cached:,} paths` / "cold").
      const indexReading = addReading('index', indexReadingText(), { bold: true });
      colophonNodes.index = indexReading.value;
      colophonNodes.indexDot = createActivityDot();
      indexReading.wrap.appendChild(colophonNodes.indexDot);
    }
  }
  // Bolded (same treatment as `index`) so it stands out from the plain
  // match/case settings readings next to it — this is the one that changes
  // what column the other readings even mean.
  addReading('mode', modeLabel(state.mode), { bold: true });
  const matchEl = addReading('match', state.searchRegex ? 'regex' : isIndexMode(state.mode) ? 'name' : 'literal').wrap;
  // Always hoverable, even reading "literal"/"name" — that's exactly the
  // state a viewer wondering "can I regex this?" is looking at.
  wireHoverTooltip(matchEl, regexTooltipLines);
  addReading('case', state.caseMode);
  if (state.mode === 'content') {
    addReading('mounts', state.config?.content_search_follow_mounts ? 'follow' : 'bounded');
  }

  const paths = state.config.paths || [];
  if (paths.length > 0) {
    const sep = document.createElement('span');
    sep.className = 'reading-sep';
    sep.textContent = '·';
    el.appendChild(sep);

    const label = document.createElement('span');
    label.className = 'reading-key';
    label.textContent = 'sources: ';
    el.appendChild(label);

    paths.forEach((path, i) => {
      if (i > 0) el.appendChild(document.createTextNode(' '));
      const span = document.createElement('span');
      span.className = 'source-reading' + (state.disabledPaths.includes(path) ? ' source-off' : '');
      span.textContent = path;
      span.title = 'Click to toggle this source';
      span.addEventListener('click', () => toggleSource(path));
      el.appendChild(span);
    });
  }

  colophonNodes.shape = colophonShape();
  syncActivityDots();
}

async function toggleSource(path) {
  if (state.savingSource) return;
  clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  const active = (state.config.paths || []).filter((p) => !state.disabledPaths.includes(p));
  if (!state.disabledPaths.includes(path) && active.length <= 1) return;
  const disabled = state.disabledPaths.includes(path)
    ? state.disabledPaths.filter((p) => p !== path)
    : [...state.disabledPaths, path];

  state.savingSource = true;
  try {
    const config = await saveRuntimeState({ disabled_paths: disabled });
    if (!config) return;
    state.disabledPaths = config.disabled_paths;
    await invoke('invalidate_file_index');
    renderColophon();
    runSearch();
  } catch (error) {
    showNotice(`Could not update search sources: ${error}`);
  } finally {
    state.savingSource = false;
  }
}

function wireHoverTooltip(el, linesFn) {
  el.classList.add('hoverable-reading');
  let showing = false;
  // The content is fixed for as long as the pointer stays on this reading,
  // so it is built and measured once on entry. Rebuilding the lines and
  // forcing a synchronous layout on every mousemove was pure overhead.
  el.addEventListener('mouseenter', (e) => {
    showing = true;
    showTooltip(e.clientX, e.clientY, linesFn());
  });
  el.addEventListener('mousemove', (e) => {
    if (showing) positionTooltip(e.clientX, e.clientY);
  });
  el.addEventListener('mouseleave', () => {
    showing = false;
    hideTooltip();
  });
}

function regexTooltipLines() {
  const width = Math.max(...REGEX_EXAMPLES.map(([pattern]) => pattern.length));
  return ['REGEX (Rust regex syntax — same engine as ripgrep)', ...REGEX_EXAMPLES.map(([pattern, note]) => `${pattern.padEnd(width)}  ${note}`)];
}

let tooltipSize = { width: 0, height: 0 };

function showTooltip(x, y, lines) {
  const tooltip = document.getElementById('tooltip');
  tooltip.textContent = lines.join('\n');
  tooltip.hidden = false;
  // Measured once here, while the content is fresh; `positionTooltip` then
  // reuses it instead of forcing a layout per mousemove.
  const rect = tooltip.getBoundingClientRect();
  tooltipSize = { width: rect.width, height: rect.height };
  positionTooltip(x, y);
}

function positionTooltip(x, y) {
  const tooltip = document.getElementById('tooltip');
  const left = Math.min(x + 12, window.innerWidth - tooltipSize.width - 4);
  const top = Math.min(y + 20, window.innerHeight - tooltipSize.height - 4);
  tooltip.style.left = `${Math.max(4, left)}px`;
  tooltip.style.top = `${Math.max(4, top)}px`;
}

function hideTooltip() {
  document.getElementById('tooltip').hidden = true;
}

function renderFooter() {
  const el = document.getElementById('footer');
  el.innerHTML = '';
  FOOTER_LEGEND.forEach(([key, label]) => {
    const item = document.createElement('span');
    item.className = 'footer-item';
    const k = document.createElement('span');
    k.className = 'footer-key';
    k.textContent = key;
    const l = document.createElement('span');
    l.className = 'footer-label';
    l.textContent = ' ' + label;
    item.appendChild(k);
    item.appendChild(l);
    el.appendChild(item);
  });
}

// How much of `state.results` is already on the page, and which row currently
// carries the selection — so a streaming batch can append instead of redraw.
let renderedCount = 0;
let renderedSelIndex = -1;

function renderList() {
  const column = document.getElementById('citationColumn');

  if (state.results.length === 0) {
    column.innerHTML = '';
    renderedCount = 0;
    renderedSelIndex = -1;
    renderIdleMessage(column);
    return;
  }

  // Append only what is new. Rebuilding every already-drawn row on each
  // arriving batch is quadratic in the result count — invisible at the
  // default max_results of 30, and painful at a configured few thousand.
  if (renderedCount === 0) {
    column.innerHTML = ''; // clears the idle panel
    renderedSelIndex = -1;
  }
  for (let index = renderedCount; index < state.results.length; index += 1) {
    column.appendChild(buildCitation(state.results[index], index));
  }
  renderedCount = state.results.length;
  paintSelection();
}

function buildCitation(row, index) {
  const line = document.createElement('div');
  line.className = 'citation';
  line.addEventListener('click', () => {
    state.selIndex = index;
    paintSelection();
  });
  line.addEventListener('dblclick', () => openSelected());
  line.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    state.selIndex = index;
    paintSelection();
    openContextMenu(e.clientX, e.clientY);
  });

  const locator = document.createElement('span');
  locator.className = 'locator';

  // Left empty here and filled by `paintSelection`, which owns the marker.
  const manicule = document.createElement('span');
  manicule.className = 'manicule';
  locator.appendChild(manicule);

  locator.appendChild(document.createTextNode(`${index + 1}. ${shortenPath(row.dir ?? '')}`));
  (row.basename_segments || []).forEach((seg) => {
    const span = document.createElement('span');
    if (seg.hit) span.className = 'rubric';
    span.textContent = seg.text;
    locator.appendChild(span);
  });
  if (row.is_dir) {
    // A bare trailing separator, styled the same as the rest of the
    // (ink-fade) locator text, disappeared into the path's own backslashes —
    // this is the one thing distinguishing a folder row from a file row once
    // both share a result list (Both mode), so it needs its own weight/color
    // to actually read as a marker rather than more path.
    const marker = document.createElement('span');
    marker.className = 'dir-marker';
    marker.textContent = '\\';
    locator.appendChild(marker);
  }
  if (row.line > 1) {
    locator.appendChild(document.createTextNode(`:${row.line}`));
  }
  line.appendChild(locator);

  const excerpt = document.createElement('span');
  excerpt.className = 'excerpt';
  if (row.segments && row.segments.length > 0) {
    row.segments.forEach((seg) => {
      const span = document.createElement('span');
      if (seg.hit) span.className = 'rubric';
      span.textContent = seg.text;
      excerpt.appendChild(span);
    });
  }
  line.appendChild(excerpt);

  return line;
}

// Moves the selection by touching only the two rows that change.
function paintSelection() {
  if (renderedSelIndex === state.selIndex) return;
  const rows = document.getElementById('citationColumn').children;

  const previous = rows[renderedSelIndex];
  if (previous) {
    previous.classList.remove('citation-selected');
    previous.querySelector('.manicule').textContent = '';
  }
  const next = rows[state.selIndex];
  if (next) {
    next.classList.add('citation-selected');
    next.querySelector('.manicule').textContent = '☞';
  }
  renderedSelIndex = state.selIndex;
}

function renderIdleMessage(column) {
  const query = document.getElementById('queryInput').value.trim();
  const note = document.createElement('div');
  note.className = 'idle-note';

  if (query && query.length < MIN_QUERY_CHARS) {
    note.textContent = `A headword of at least ${MIN_QUERY_CHARS} letters, please — a shorter one cites the whole shelf.`;
    column.appendChild(note);
    return;
  }
  if (query && (state.stateWord === 'DONE' || state.stateWord === 'EMPTY')) {
    const kind =
      state.mode === 'files' ? 'file names'
      : state.mode === 'folders' ? 'folder names'
      : state.mode === 'both' ? 'file or folder names'
      : 'file contents';
    note.textContent = `No citations for “${query}” in ${kind}.`;
    column.appendChild(note);
    return;
  }
  if (query) {
    note.textContent = `Searching sources for “${query}” …`;
    column.appendChild(note);
    return;
  }

  // No query yet: list what's configured, matching the original's
  // "Sources consulted" idle panel.
  const heading = document.createElement('div');
  heading.className = 'idle-heading';
  heading.textContent = 'Sources consulted';
  column.appendChild(heading);

  const paths = state.config.paths && state.config.paths.length > 0 ? state.config.paths : ['(no paths configured)'];
  paths.forEach((path) => {
    const row = document.createElement('div');
    row.className = 'idle-source' + (state.disabledPaths.includes(path) ? ' source-off' : '');
    row.textContent = `— ${path}`;
    column.appendChild(row);
  });

  const indexRow = document.createElement('div');
  indexRow.className = 'idle-source';
  indexRow.textContent = '— name index  …';
  column.appendChild(indexRow);

  invoke('get_index_status').then(({ state: cacheState, count }) => {
    let note;
    if (cacheState === 'ready') note = `${count.toLocaleString()} paths`;
    else if (cacheState === 'building') note = `building — ${count.toLocaleString()} paths so far`;
    else note = 'cold — Tab builds it on first use';
    indexRow.textContent = `— name index  ${note}`;
  });
}

function shortenPath(path) {
  const maxLen = 70;
  if (path.length <= maxLen) return path;
  return '…' + path.slice(-(maxLen - 1));
}

function onKeyDown(e) {
  if (e.key === 'Escape' && !document.getElementById('contextMenu').hidden) {
    closeContextMenu();
    e.preventDefault();
    return;
  }
  if (e.key === 'Escape') {
    const input = document.getElementById('queryInput');
    if (input.value) {
      // First press clears the query rather than dismissing the window
      // outright, matching quicksearch's own Escape convention (clear,
      // then a second press leaves). Goes through the normal short-query
      // path immediately (not the debounce timer) so it stops a running
      // search/index build the same way emptying the box by hand does.
      clearTimeout(state.debounceTimer);
      input.value = '';
      updateHeadwordCaret();
      runSearch();
    } else {
      // Already empty: the real dismiss, which cancels anything still
      // running (search or index build) before hiding.
      hideWindow();
    }
    e.preventDefault();
    return;
  }
  if (e.key === 'Tab') {
    toggleMode();
    e.preventDefault();
    return;
  }
  if (e.altKey && (e.key === 'r' || e.key === 'R')) {
    toggleRegex();
    e.preventDefault();
    return;
  }
  if (e.altKey && (e.key === 'c' || e.key === 'C')) {
    cycleCaseMode();
    e.preventDefault();
    return;
  }
  if (e.altKey && (e.key === 'm' || e.key === 'M')) {
    toggleMountTraversal();
    e.preventDefault();
    return;
  }
  if (e.key === 'ArrowDown') {
    moveSelection(1);
    e.preventDefault();
    return;
  }
  if (e.key === 'ArrowUp') {
    moveSelection(-1);
    e.preventDefault();
    return;
  }
  if (e.key === 'Enter' && e.ctrlKey) {
    openSelectedLocation();
    e.preventDefault();
    return;
  }
  if (e.key === 'Enter') {
    openSelected();
    e.preventDefault();
  }
}

function moveSelection(delta) {
  if (state.results.length === 0) return;
  state.selIndex = Math.max(0, Math.min(state.results.length - 1, state.selIndex + delta));
  paintSelection();
  document.querySelector('.citation-selected')?.scrollIntoView({ block: 'nearest' });
}

const MODE_CYCLE = ['files', 'content', 'both', 'folders'];

function toggleMode() {
  clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  state.mode = MODE_CYCLE[(MODE_CYCLE.indexOf(state.mode) + 1) % MODE_CYCLE.length];
  if (isIndexMode(state.mode)) {
    startIndexPolling();
  } else {
    stopIndexPolling();
    state.indexStatus = null;
  }
  renderColophon();
  runSearch();
}

async function toggleRegex() {
  if (state.savingMatch) return;
  state.savingMatch = true;
  clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  try {
    const config = await saveRuntimeState({ search_regex: !state.searchRegex });
    if (!config) return;
    state.searchRegex = !!config.search_regex;
    renderColophon();
    runSearch();
  } finally {
    state.savingMatch = false;
  }
}

async function cycleCaseMode() {
  if (state.savingMatch) return;
  state.savingMatch = true;
  clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  const order = ['smart', 'sensitive', 'insensitive'];
  try {
    const config = await saveRuntimeState({ case_mode: order[(order.indexOf(state.caseMode) + 1) % order.length] });
    if (!config) return;
    state.caseMode = config.case_mode;
    renderColophon();
    runSearch();
  } finally {
    state.savingMatch = false;
  }
}

function openSelected() {
  const row = state.results[state.selIndex];
  if (!row) return;
  invoke('open_result', { path: row.path, line: row.line });
  hideWindow();
}

function openSelectedLocation() {
  const row = state.results[state.selIndex];
  if (!row) {
    showNotice('No citation selected — search for something first.');
    return;
  }
  invoke('open_result_location', { path: row.path }).catch((err) => showNotice(String(err)));
}

// Puts the selected row's actual file/folder on the clipboard (Windows
// CF_HDROP), not just its path text — pasting elsewhere in Explorer copies
// or moves the real thing, exactly like Explorer's own Copy/Cut.
function copySelected(cut) {
  const row = state.results[state.selIndex];
  if (!row) return;
  invoke('copy_result', { path: row.path, cut }).catch((err) => showNotice(String(err)));
}

// Copy/Cut have no `key` hint: unlike Open/Open-containing-folder, they
// aren't bound to Ctrl+C/Ctrl+X globally — the headword input owns those
// for its own text selection, and overriding them app-wide would silently
// steal a normal "copy the query text I selected" keystroke.
const CONTEXT_MENU_ITEMS = [
  { label: 'Open', key: 'Enter', action: openSelected },
  { label: 'Open containing folder', key: '^Enter', action: openSelectedLocation },
  { sep: true },
  { label: 'Copy', action: () => copySelected(false) },
  { label: 'Cut', action: () => copySelected(true) },
];

function closeContextMenu() {
  document.getElementById('contextMenu').hidden = true;
}

// Right-click menu for the selected row, styled to match the rest of the
// app rather than a native Windows menu. Each item's action reuses the same
// function a keyboard shortcut would call — the menu is a discoverability
// aid for commands that already exist, not a second implementation of them.
function openContextMenu(x, y) {
  hideTooltip();
  const menu = document.getElementById('contextMenu');
  menu.innerHTML = '';

  CONTEXT_MENU_ITEMS.forEach((entry) => {
    if (entry.sep) {
      const sep = document.createElement('div');
      sep.className = 'context-menu-sep';
      menu.appendChild(sep);
      return;
    }
    const item = document.createElement('div');
    item.className = 'context-menu-item';
    const label = document.createElement('span');
    label.textContent = entry.label;
    item.appendChild(label);
    if (entry.key) {
      const key = document.createElement('span');
      key.className = 'context-menu-key';
      key.textContent = entry.key;
      item.appendChild(key);
    }
    item.addEventListener('click', () => {
      closeContextMenu();
      entry.action();
    });
    menu.appendChild(item);
  });

  menu.hidden = false;
  // Placed once to get real dimensions from layout, then clamped so it
  // never opens off the (fairly small) app window — a right-click near the
  // bottom-right corner would otherwise draw a menu partly off-screen.
  menu.style.left = `${x}px`;
  menu.style.top = `${y}px`;
  const rect = menu.getBoundingClientRect();
  const maxLeft = Math.max(4, window.innerWidth - rect.width - 4);
  const maxTop = Math.max(4, window.innerHeight - rect.height - 4);
  menu.style.left = `${Math.min(x, maxLeft)}px`;
  menu.style.top = `${Math.min(y, maxTop)}px`;
}

function hideWindow() {
  // Routed through the backend (not a direct window.hide()) so it also
  // cancels a running search/index build — matches quicksearch's hide(),
  // which calls cancel_search() before withdrawing the window.
  stopElapsedTick();
  stopIndexPolling();
  clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  state.generation += 1;
  state.awaitingFreshBegin = true;
  state.failedGeneration = null;
  invoke('hide_window');
}

async function toggleMountTraversal() {
  if (state.savingMatch) return;
  state.savingMatch = true;
  clearTimeout(state.debounceTimer);
  state.debounceTimer = null;
  try {
    const follow = !state.config?.content_search_follow_mounts;
    const config = await saveRuntimeState({ content_search_follow_mounts: follow });
    if (!config) return;
    renderColophon();
    runSearch();
  } finally {
    state.savingMatch = false;
  }
}

let zoomSaveTimer = null;

function onWheel(e) {
  if (!e.ctrlKey) return;
  e.preventDefault();
  const current = state.config.zoom || 1.0;
  const next = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, current + (e.deltaY < 0 ? ZOOM_STEP : -ZOOM_STEP)));
  applyZoom(next, true);
}

function applyZoom(zoom, persist) {
  state.config.zoom = zoom;
  document.documentElement.style.setProperty('--zoom', zoom.toFixed(2));
  if (persist) {
    clearTimeout(zoomSaveTimer);
    zoomSaveTimer = setTimeout(() => saveRuntimeState({ zoom }), 300);
  }
}

function setDividerRatio(ratio) {
  document.documentElement.style.setProperty('--divider-pct', `${(ratio * 100).toFixed(2)}%`);
}

function setupDividerDrag() {
  const handle = document.getElementById('dividerHandle');
  const wrap = document.querySelector('.citation-column-wrap');
  let dragging = false;
  let dividerSaveTimer = null;

  handle.addEventListener('mousedown', (e) => {
    dragging = true;
    handle.classList.add('dragging');
    e.preventDefault();
  });

  document.addEventListener('mousemove', (e) => {
    if (!dragging) return;
    const rect = wrap.getBoundingClientRect();
    if (rect.width <= 0) return; // not laid out yet — ignore rather than divide by zero
    let ratio = (e.clientX - rect.left) / rect.width;
    ratio = Math.max(DIVIDER_MIN_RATIO, Math.min(DIVIDER_MAX_RATIO, ratio));
    setDividerRatio(ratio);

    clearTimeout(dividerSaveTimer);
    dividerSaveTimer = setTimeout(() => saveRuntimeState({ divider_ratio: ratio }), 300);
  });

  document.addEventListener('mouseup', () => {
    if (!dragging) return;
    dragging = false;
    handle.classList.remove('dragging');
  });
}

function showNotice(message, tone = 'error') {
  const el = document.getElementById('notice');
  el.textContent = message;
  el.classList.toggle('notice-warning', tone === 'warning');
  el.hidden = false;
}

function hideNotice() {
  const el = document.getElementById('notice');
  el.classList.remove('notice-warning');
  el.hidden = true;
}

document.addEventListener('DOMContentLoaded', init);
