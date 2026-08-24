// Preloaded by `bunfig.toml` before any test module is imported.
//
// Two module-load-time couplings force this to run first: `src/audio-proxy.ts` reads
// `HTMLMediaElement.prototype`'s `src` descriptor as it loads, and `src/ipc.ts` captures
// `window.cefQuery` into a const on the same tick. A stub installed inside a test would arrive
// after both.

import { GlobalRegistrator } from "@happy-dom/global-registrator";

GlobalRegistrator.register();

export type SentIpc = { channel: string; args: unknown[] };

// Every `sendIpc` the code under test issued, in order.
const sent: SentIpc[] = [];

(window as any).cefQuery = ({ request }: { request: string }) => {
	const { channel, args } = JSON.parse(request);
	sent.push({ channel, args });
};

(globalThis as any).__LUNAR_TEST_IPC__ = sent;
