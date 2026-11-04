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
