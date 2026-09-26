// Run with: node --test scripts/frontend-settings.test.cjs
// Exercises the actual app handlers with mocked IPC; no installed config or UI.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');

function harness({ failSave = false } = {}) {
  const calls = [];
  const notices = [];
  const styles = {};
  let saved = {
    paths: ['C:\\', 'D:\\'], disabled_paths: [], search_regex: false,
    case_mode: 'smart', zoom: 1.4, divider_ratio: 0.63,
  };
  const context = vm.createContext({
    setTimeout, clearTimeout, setInterval, clearInterval,
    document: {
      addEventListener() {},
      documentElement: { style: { setProperty: (key, value) => { styles[key] = value; } } },
    },
    window: { __TAURI__: { core: { invoke: async (command, payload) => {
      calls.push(command);
      if (command === 'get_config') return { ...saved };
      if (command === 'save_runtime_state') {
        if (failSave) throw new Error('disk is read-only');
        saved = { ...saved, ...payload.patch };
        return { ...saved };
      }
    } } } },
    recordNotice: message => notices.push(message),
    recordSearch: () => calls.push('search'),
  });
  vm.runInContext(source + `
    showNotice = recordNotice;
    renderColophon = () => {};
    runSearch = recordSearch;
    globalThis.api = { state, toggleSource, toggleRegex, cycleCaseMode, saveRuntimeState, applyZoom };
  `, context);
  const api = context.api;
  api.state.config = { ...saved };
  api.state.disabledPaths = [];
  return { api, calls, notices, styles, saved: () => saved };
}

test('failed source save leaves sources unchanged and does not rebuild or search', async () => {
  const h = harness({ failSave: true });
  await h.api.toggleSource('D:\\');
  assert.deepEqual(Array.from(h.api.state.disabledPaths), []);
  assert.equal(h.calls.includes('invalidate_file_index'), false);
  assert.equal(h.calls.includes('search'), false);
  assert.equal(h.api.state.savingSource, false);
  assert.match(h.notices[0], /Settings were not saved.*disk is read-only/);
});

test('successful source save precedes index invalidation and search', async () => {
  const h = harness();
  await h.api.toggleSource('D:\\');
  assert.deepEqual(Array.from(h.api.state.disabledPaths), ['D:\\']);
  assert.deepEqual(h.calls, ['save_runtime_state', 'invalidate_file_index', 'search']);
});

test('failed matching preference saves keep current search behavior', async () => {
  const h = harness({ failSave: true });
  await h.api.toggleRegex();
  await h.api.cycleCaseMode();
  assert.equal(h.api.state.searchRegex, false);
  assert.equal(h.api.state.caseMode, 'smart');
  assert.equal(h.calls.includes('search'), false);
  assert.equal(h.notices.length, 2);
});

test('failed visual preference saves restore the backend values', async () => {
  const h = harness({ failSave: true });
  h.api.applyZoom(2, false);
  await h.api.saveRuntimeState({ zoom: 2, divider_ratio: 0.8 });
  assert.equal(h.api.state.config.zoom, 1.4);
  assert.equal(h.styles['--zoom'], '1.40');
  assert.equal(h.styles['--divider-pct'], '63.00%');
});

test('queued writes preserve independent preferences', async () => {
  const h = harness();
  await Promise.all([
    h.api.saveRuntimeState({ zoom: 1.8 }),
    h.api.saveRuntimeState({ divider_ratio: 0.7 }),
  ]);
  assert.equal(h.saved().zoom, 1.8);
  assert.equal(h.saved().divider_ratio, 0.7);
  assert.equal(h.api.state.config.zoom, 1.8);
  assert.equal(h.api.state.config.divider_ratio, 0.7);
});
