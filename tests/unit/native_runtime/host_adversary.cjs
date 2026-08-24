// Usage: bun host_adversary.cjs <path to native-host.cjs>
//
// The other half of the sandbox harness. `host_probe.cjs` asserts named properties at named
// sites: it catches a regression at a site someone already thought about. This one asks the
// opposite question: it corrupts every own member of the six unfrozen prototypes and every
// global binding, ONE AT A TIME, and after each corruption re-runs a fixed suite of gate
// oracles. It does not know what a defect looks like; it only knows what a gate must still
// answer. That is why it found a hole thirteen hand-written fixes had walked past.
//
// The sweep lives in a sibling .js file rather than a template string inside this one, on
// purpose: `host_probe.cjs` embeds its assertions that way, and a backtick or `${` in a
// comment silently terminates the literal, which cost four broken runs in one session. A
// plain file has no such edge.
const fs = require("fs");
const os = require("os");
const path = require("path");

const HOST = process.argv[2];
if (!HOST) {
    process.stderr.write("usage: bun host_adversary.cjs <path to native-host.cjs>\n");
    process.exit(2);
}

const src = fs.readFileSync(HOST, "utf8");
const sweep = fs.readFileSync(path.join(__dirname, "host_adversary_sweep.js"), "utf8");

// Fixtures the oracles need: a dataDir the facade may touch, and a file outside it that every
// containment gate must refuse.
const scratch = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "luna-adversary-")));
const dataDir = path.join(scratch, "data");
fs.mkdirSync(dataDir);
fs.writeFileSync(path.join(dataDir, "inside.txt"), "x");
const outsideFile = path.join(scratch, "outside.txt");
fs.writeFileSync(outsideFile, "x");
const runtimeDir = path.join(scratch, "run");
fs.mkdirSync(runtimeDir);

// Same loading shape as host_probe.cjs: the sweep is appended into the host's own function
// body, so every host-private name (assertRead, isSafe, canonicalize, makeRequireProxy,
// evalPlugin, respond, pendingFetches, rl, ...) is reachable by bare identifier. The host has
// no export surface; this is the only way to reach the gates as the host itself sees them.
// Both halves of the concatenation are files from this repository (the host under test and
// the sweep beside it), never input from anywhere else, and this runs only under cargo test.
const fn = new Function(
    "require", "module", "exports", "__dirname", "__filename", "probeFixtures",
    src + "\n" + sweep,
);
fn(require, { exports: {} }, {}, path.dirname(HOST), HOST, {
    scratch, dataDir, outsideFile, runtimeDir, hostPath: HOST,
});
// No exit here: the sweep owns the exit code, because only it knows whether a gate broke.
