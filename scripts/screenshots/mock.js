// Fake Tauri backend so QuickSearch's frontend runs in a plain browser.
// Stubs window.__TAURI__.core.invoke and .event.listen, the two surfaces
// app.js actually talks to (see src/app.js's own `invoke` wrapper).
(() => {
  const listeners = {};
  function on(event, cb) {
    (listeners[event] ??= []).push(cb);
    return Promise.resolve(() => {
      listeners[event] = (listeners[event] || []).filter((f) => f !== cb);
    });
  }
  function emit(event, payload) {
    (listeners[event] || []).forEach((cb) => cb({ event, payload }));
  }

  const CONFIG = {
    paths: ['C:\\', 'D:\\'],
    exclude_dirs: ['node_modules', 'target', '.git'],
    editor_command: ['code', '-g', '{file}:{line}'],
    hotkey: 'Ctrl+Alt+Space',
    max_results: 30,
    max_per_file: 5,
    zoom: 1.0,
    divider_ratio: 0.63,
    search_regex: false,
    case_mode: 'smart',
    disabled_paths: [],
    content_search_quiet: true,
    content_search_debounce_ms: 450,
    content_search_follow_mounts: false,
    filename_index_persist: true,
    filename_index_refresh_minutes: 60,
  };

  const INDEX_STATUS = { state: 'ready', count: 287342, watched: true, refreshing: false, error: null };

  // Row shapes match src-tauri/src/search.rs's output: `segments` and
  // `basename_segments` are [{text, hit}] runs painting the rubric highlight.
  const seg = (text, hit) => ({ text, hit: !!hit });

  const FILENAME_ROWS = [
    { dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\', basename_segments: [seg('search.rs')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\', basename_segments: [seg('config.rs')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\', basename_segments: [seg('index_'), seg('store.rs')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\', basename_segments: [seg('index_'), seg('watcher.rs')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\src\\', basename_segments: [seg('app.js')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\', basename_segments: [seg('Cargo.toml')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\gen\\schemas\\', basename_segments: [seg('acl-manifests.json')], is_dir: false, line: 1, segments: [] },
    { dir: 'C:\\Users\\etienne\\quicksearch\\docs\\', basename_segments: [seg('PRODUCT.md')], is_dir: false, line: 1, segments: [] },
    { dir: 'D:\\Backups\\configs\\', basename_segments: [seg('search_config.bak')], is_dir: false, line: 1, segments: [] },
    { dir: 'D:\\Projects\\notes\\scripts\\screenshots\\', basename_segments: [seg('capture.mjs')], is_dir: false, line: 1, segments: [] },
  ];

  // Highlights `query` wherever it appears in the fake filenames above, the
  // way search.rs's real matcher would.
  function highlightFilenames(query) {
    const q = query.toLowerCase();
    return FILENAME_ROWS.map((row) => {
      const segs = [];
      for (const s of row.basename_segments) {
        const text = s.text;
        const i = text.toLowerCase().indexOf(q);
        if (i === -1) {
          segs.push(seg(text, false));
        } else {
          if (i > 0) segs.push(seg(text.slice(0, i), false));
          segs.push(seg(text.slice(i, i + q.length), true));
          if (i + q.length < text.length) segs.push(seg(text.slice(i + q.length), false));
        }
      }
      return { ...row, basename_segments: segs };
    });
  }

  const CONTENT_ROWS = [
    {
      dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\',
      basename_segments: [seg('index_store.rs')],
      is_dir: false,
      line: 42,
      segments: [seg('    let count = '), seg('index', true), seg('_store.rows().count();')],
    },
    {
      dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\',
      basename_segments: [seg('filecache.rs')],
      is_dir: false,
      line: 118,
      segments: [seg('// rebuild the '), seg('index', true), seg(' when the watcher misses an event')],
    },
    {
      dir: 'C:\\Users\\etienne\\quicksearch\\src-tauri\\src\\',
      basename_segments: [seg('index_watcher.rs')],
      is_dir: false,
      line: 7,
      segments: [seg('pub fn spawn_'), seg('index', true), seg('_watcher(paths: &[PathBuf]) {')],
    },
    {
      dir: 'C:\\Users\\etienne\\quicksearch\\docs\\',
      basename_segments: [seg('PRODUCT.md')],
      is_dir: false,
      line: 61,
      segments: [seg('The filename '), seg('index', true), seg(' is saved as a SQLite catalog in %APPDATA%.')],
    },
  ];

  let generation = 0;

  const handlers = {
    get_config: () => CONFIG,
    save_runtime_state: ({ patch }) => Object.assign(CONFIG, patch),
    get_index_status: () => INDEX_STATUS,
    frontend_ready: () => null,
    // The real backend bumps the shared search generation on cancel too
    // (see app.js's onInputChanged) — mirrored here so the frontend's own
    // per-keystroke generation count and the backend's stay in lockstep.
    cancel_search: () => { generation++; return null; },
    invalidate_file_index: () => null,
    open_result: () => null,
    open_result_location: () => null,
    copy_result: () => null,
    hide_window: () => null,
    search: ({ pattern, mode }) => {
      const gen = ++generation;
      const rows = mode === 'content' ? CONTENT_ROWS : highlightFilenames(pattern);
      queueMicrotask(() => emit('qs-begin', gen));
      setTimeout(() => emit('qs-batch', { generation: gen, rows }), 60);
      setTimeout(() => emit('qs-done', { generation: gen, idle: false }), 120);
      return gen;
    },
  };

  window.__TAURI__ = {
    core: {
      invoke: async (cmd, payload = {}) => {
        const h = handlers[cmd];
        return h ? h(payload) : null;
      },
    },
    event: { listen: on },
  };
})();
