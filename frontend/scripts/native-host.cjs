const readline = require("readline");

const modules = {};

const rl = readline.createInterface({ input: process.stdin });

rl.on("line", async (line) => {
	let cmd;
	try {
		cmd = JSON.parse(line);
	} catch {
		return;
	}

	const { id, type } = cmd;
	if (!id) return;

	try {
		if (type === "register") {
			const { name, code } = cmd;
			const m = { exports: {} };
			const fn = new Function("module", "exports", "require", code);
			fn(m, m.exports, require);
			modules[name] = m.exports;
			const exports = Object.keys(m.exports);
			respond(id, { ok: true, exports });
		} else if (type === "call") {
			const { name, fn: fnName, args } = cmd;
			const mod = modules[name];
			if (!mod) {
				respondError(id, `module '${name}' not registered`);
				return;
			}
			const member = mod[fnName];
			if (typeof member === "function") {
				const result = await member(...(args || []));
				respond(id, { ok: true, result: result ?? null });
			} else {
				respond(id, { ok: true, result: member ?? null });
			}
		} else if (type === "cleanup") {
			const { name } = cmd;
			delete modules[name];
			respond(id, { ok: true });
		} else {
			respondError(id, `unknown command type: ${type}`);
		}
	} catch (e) {
		respondError(id, e?.message || String(e));
	}
});

function respond(id, data) {
	process.stdout.write(JSON.stringify({ id, ...data }) + "\n");
}

function respondError(id, error) {
	process.stdout.write(JSON.stringify({ id, error }) + "\n");
}
