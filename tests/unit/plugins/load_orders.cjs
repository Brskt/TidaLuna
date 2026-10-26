// Plays a plugin's load and unload texts in every order, each in a fresh realm holding the
// early-runtime registry, and checks the copies left running against the manager's load.
// usage: bun load_orders.cjs <texts.json> <plugin_loads.js>
const vm = require('node:vm');
const fs = require('node:fs');

const [, , textsPath, registryPath] = process.argv;
const texts = JSON.parse(fs.readFileSync(textsPath, 'utf8'));
const registry = fs.readFileSync(registryPath, 'utf8');

function orders(events) {
    if (events.length <= 1) return [events];
    const out = [];
    events.forEach((first, i) => {
        for (const rest of orders(events.filter((_, j) => j !== i))) out.push([first, ...rest]);
    });
    return out;
}

function realm() {
    const ctx = vm.createContext({ console: { error() {}, log() {} } });
    vm.runInContext(
        'var window = globalThis; var self = globalThis;' +
            'globalThis.__live = new Set();' +
            'globalThis.cefQuery = function () {};',
        ctx,
    );
    // Wrapped as the preload wraps it.
    vm.runInContext('(function(){\n' + registry + '\n})();', ctx);
    return ctx;
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

async function scenario(name, events, want) {
    let wrong = 0;
    let first = null;
    const all = orders(events);
    for (const order of all) {
        const ctx = realm();
        for (const [kind, id] of order) {
            vm.runInContext(kind === 'load' ? texts.load[id] : texts.unload[id], ctx);
            await settle();
        }
        const live = JSON.parse(vm.runInContext('JSON.stringify([...__live].sort())', ctx));
        if (JSON.stringify(live) !== JSON.stringify(want)) {
            wrong++;
            first ??= `${JSON.stringify(order)} left ${JSON.stringify(live)}`;
        }
    }
    console.log(`${name}: ${all.length} orders, ${wrong} wrong${first ? `, first: ${first}` : ''}`);
    return { played: all.length, wrong };
}

(async () => {
    const results = [
        await scenario(
            'restart',
            [['load', 1], ['unload', 1], ['load', 2], ['unload', 2], ['load', 3]],
            [3],
        ),
        await scenario('mirror', [['load', 3], ['load', 4], ['unload', 3]], [4]),
    ];
    const played = results.reduce((n, r) => n + r.played, 0);
    const wrong = results.reduce((n, r) => n + r.wrong, 0);
    // 120 + 6: anything else means the scenarios did not run as written.
    if (played !== 126) {
        console.log(`played ${played} orders, expected 126`);
        process.exit(2);
    }
    process.exit(wrong === 0 ? 0 : 1);
})().catch((e) => {
    console.log('harness error', (e && e.stack) || e);
    process.exit(3);
});
