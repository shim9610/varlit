// Exercise the actual committed dashboard, including its default presentation.
const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const path = require('node:path');
const root = path.resolve(__dirname, '../../docs/performance/workload-storage-20261104');
const data = JSON.parse(fs.readFileSync(path.join(root, 'current-comparison.json'), 'utf8'));
const html = fs.readFileSync(path.join(root, 'index.html'), 'utf8');
const script = html.match(/<script>([\s\S]*?)<\/script>/)[1];
const controls = {};
for (const key of ['scope', 'search', 'family', 'engine', 'metric', 'counts', 'rows']) {
  controls[key] = {value: key === 'metric' ? 'write' : '', children: [],
    replaceChildren() { this.children = []; }, append(value) { this.children.push(value); }};
}
const document = {
  querySelector(selector) { return controls[selector.slice(1)]; },
  querySelectorAll() { return []; },
  createElement() { return {children: [], classList: {add() {}}, append(value) { this.children.push(value); }}; },
};
const context = {document};
vm.createContext(context);
vm.runInContext(script, context);
assert.equal(vm.runInContext('JSON.stringify(DATA)', context), JSON.stringify(data),
  'Dashboard must embed the current exported data');
const original = JSON.parse(fs.readFileSync(path.join(root, 'comparison.json'), 'utf8'));
for (const [name, row] of Object.entries(original.profiles)) {
  for (const [field, value] of Object.entries(row)) {
    assert.deepEqual(data.profiles[name][field], value, `Original ${name}.${field} must remain intact`);
  }
}
const names = () => controls.rows.children.map(row => row.children[0].textContent);
const historical = name => name.startsWith('objects-') || name.startsWith('individual-');
const all = new Set([...Object.keys(data.profiles), ...data.failures.map(row => row.profile.name)]);
assert.deepEqual(new Set(names()), new Set([...all].filter(name => !historical(name))));
assert(names()[0].startsWith('grouped-'), 'Corrected grouped cases must lead the default view');
controls.scope.value = 'historical'; context.render();
assert.deepEqual(new Set(names()), new Set([...all].filter(historical)));
controls.scope.value = 'all'; context.render();
assert.deepEqual(new Set(names()), all, 'Failed/refused configurations must remain available');
controls.scope.value = ''; controls.family.value = 'grouped-'; controls.engine.value = 'hdf5'; context.render();
const hdf = Object.entries(data.profiles).filter(([name, row]) => name.startsWith('grouped-') && row.profile.engine === 'hdf5');
assert.deepEqual(new Set(names()), new Set(hdf.map(([name]) => name)));
for (const ratio of data.comparisons.filter(row => row.profile.startsWith('grouped-'))) {
  assert.equal(ratio.ratio, data.profiles[ratio.sqlite].metrics[ratio.metric] / data.profiles[ratio.profile].metrics[ratio.metric]);
}
console.log('Integrated dashboard defaults, controls, refusals, filters and grouped ratios pass');

controls.scope.value = 'validation'; controls.family.value = ''; controls.engine.value = ''; context.render();
const validation = Object.keys(data.profiles).filter(name => name.startsWith('readcheck-'));
assert.equal(validation.length, 69);
assert.deepEqual(new Set(names()), new Set(validation));
for (const row of controls.rows.children) {
  const name = row.children[0].textContent;
  const selected = data.profiles[name];
  const full = Object.values(data.profiles).find(x => x.presentation === 'validation' && x.profile.workload === selected.profile.workload && x.profile.variant === 'full');
  assert.equal(row.children[10].textContent, '—', 'Do not invent cross-cohort SQLite ratios');
  assert.equal(row.children[11].textContent, (full.metrics.write / selected.metrics.write).toFixed(3));
}
console.log('Independent read-check scope, separate ratios and 69 measured profiles pass');

controls.scope.value = 'mmap'; context.render();
const mmapRows = Object.entries(data.profiles).filter(([, row]) => row.presentation === 'mmap');
assert.equal(mmapRows.length, 102);
assert.deepEqual(new Set(names()), new Set(mmapRows.map(([name]) => name)));
for (const [name, row] of mmapRows) {
  assert.equal(row.observations, 7);
  assert(row.metric_spread.completion.min <= row.metrics.completion);
  assert(row.metric_spread.completion.max >= row.metrics.completion);
  const p = row.profile;
  if (p.live || p.kind === 'mixed') {
    const readers = p.config?.readers ?? p.readers;
    assert(row.probe.sampled_processes >= readers + 2, `${name}: missing reader processes`);
    assert(row.metrics.min_reader_during_fraction > 0, `${name}: no observed live reads`);
  }
  if (p.variant === 'sqlite-m0') assert.equal(row.probe.primary_mmap_virtual_bytes, 0);
  if (p.variant.startsWith('sqlite-') && p.variant !== 'sqlite-m0') {
    assert(row.probe.primary_mmap_unique_bytes > 0, `${name}: primary database mapping unobserved`);
  }
}
const references = new Map();
for (const ratio of data.comparisons.filter(r => r.profile.startsWith('mmapcheck-'))) {
  const row = data.profiles[ratio.profile], sql = data.profiles[ratio.sqlite];
  assert.equal(row.profile.cohort, sql.profile.cohort);
  assert.equal(row.profile.workload, sql.profile.workload);
  assert.equal(ratio.ratio, sql.metrics[ratio.metric] / row.metrics[ratio.metric]);
  assert(!references.has(ratio.profile) || references.get(ratio.profile) === ratio.sqlite,
    'Do not splice SQLite configurations across write/read/CPU metrics');
  references.set(ratio.profile, ratio.sqlite);
}
controls.family.value = 'live-'; controls.engine.value = 'sqlite'; context.render();
const liveSql = mmapRows.filter(([, r]) => r.profile.live && r.profile.mode === 'sql-chunks');
assert.deepEqual(new Set(names()), new Set(liveSql.map(([name]) => name)));
console.log('SQLite mmap cohorts, equal-work references, actual mappings and live reader coverage pass');

controls.scope.value = 'append-only'; controls.family.value = ''; controls.engine.value = ''; context.render();
const appendRows = Object.entries(data.profiles).filter(([, row]) => row.presentation === 'append-only');
assert.equal(appendRows.length, 159);
assert.deepEqual(new Set(names()), new Set(appendRows.map(([name]) => name)));
assert.equal(data.profiles[names()[0]].profile.cohort, 'primary-final-v1', 'Final runtime must lead ownership view');
assert.equal(controls.rows.children[0].children[1].textContent, 'final implementation');
for (const row of controls.rows.children) {
  const selected = data.profiles[row.children[0].textContent];
  const p = selected.profile;
  const calibrated = p.cohort.startsWith('calibration');
  assert.equal(selected.observations, calibrated ? 30 : 7);
  assert(selected.probe.sampled_processes >= 2);
  assert(selected.metric_spread.completion.min <= selected.metrics.completion);
  assert.equal(row.children[10].textContent, '—', 'Ownership cohort has no new SQLite comparison');
  const guarded = appendRows.find(([, r]) => r.profile.cohort === p.cohort && r.profile.workload === p.workload
    && r.profile.variant === (calibrated ? 'guarded' : 'after-guarded'))[1];
  assert.equal(row.children[11].textContent, (guarded.metrics.write / selected.metrics.write).toFixed(3));
  const manifest = data.append_only.manifests[p.cohort];
  if (calibrated) assert.equal(Object.keys(manifest.binaries).length, 1);
  assert.equal(data.append_only.memory_event_deltas[p.cohort].oom, 0);
  assert.equal(data.append_only.memory_event_deltas[p.cohort].oom_kill, 0);
}
controls.family.value = 'session-live'; context.render();
assert(names().every(n => data.profiles[n].profile.workload.startsWith('session-live')));
assert.equal(names().length, 75);
console.log('Append-only before/after/cohort filters, thread probes and nonpooled ownership ratios pass');
