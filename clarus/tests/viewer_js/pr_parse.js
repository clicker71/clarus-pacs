// Harness: run the viewer's own PR parser (PD/SW/ANN/SQ/SU/IE) in Node
// against real bytes. No DOM: the sliced region has no DOM calls.
//
// Manual verification for the embedded mini-viewer parser: the Rust gates
// (pr_overlay_smoke.rs) pin the parser's SOURCE shape, this harness EXECUTES
// the real parser slice from the served asset. The slice boundary vars
// (RSUID/RSUIDS) are declared before the slice in the asset; they are
// injected here because the slice ends where they begin.
//
// Run from the repo root:
//   node crates/clarus-server/tests/viewer_js/pr_parse.js [pr-file.dcm]
const fs = require('fs');
const path = require('path');

const root = path.resolve(__dirname, '..', '..', '..', '..');
const html = fs.readFileSync(
  path.join(root, 'crates', 'clarus-server', 'assets', 'clarus-viewer.html'),
  'utf8'
);
const from = html.indexOf('var VR=new Set');
const to = html.indexOf('var PT=[');
if (from < 0 || to < 0) throw new Error('parser region not found');
const core = html.slice(from, to);

function run(label, file) {
  const buf = fs.readFileSync(file);
  const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
  // The parser uses a DataView; expose it.
  const fn = new Function(
    'DataView',
    'var RSUID=null,RSUIDS=[];\n' + core + `
;return (function(){
  var h=PD(arguments[1]);
  return {nr:h.NR||null, ga:(h.GA||[]).map(function(a){return {t:a.t,r:a.r&&a.r.length,u:a.u,f:a.f,x:a.x}}), ris:RIS, rsuid:RSUID};
}).apply(null, arguments);`
  );
  const out = fn(DataView, ab);
  console.log(label + ':', JSON.stringify(out, null, 0));
  return out;
}

run(
  'fixture',
  path.join(root, 'crates', 'clarus-server', 'tests', 'fixtures', 'pr_minimal.dcm')
);
if (process.argv[2]) {
  run('file', path.resolve(process.argv[2]));
}
