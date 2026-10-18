// The codec is not there yet: the cover request is made and refused with a 503, as at every
// start-up. `onRequest` sees each request's url, for a test that counts or records them.

export const refusingTransport = (onRequest?: (url: string) => void) => {
	(globalThis as any).fetch = (url: string) => {
		onRequest?.(url);
		return Promise.resolve({
			ok: false,
			status: 503,
			headers: { get: () => "the codec is still installing" },
			arrayBuffer: () => Promise.reject(new Error("a refused body has none")),
		});
	};
};
