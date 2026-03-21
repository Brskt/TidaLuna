import { trace } from ".";

let originalCefQuery: typeof window.cefQuery | null = null;

export const startIpcLog = () => {
	if (originalCefQuery) return; // Already hooked
	originalCefQuery = window.cefQuery;

	window.cefQuery = (params) => {
		let parsed: { channel?: string; args?: any[] } | undefined;
		try {
			parsed = JSON.parse(params.request);
		} catch {}

		const channel = parsed?.channel ?? "???";
		const args = parsed?.args ?? [];
		trace.log("JS → Rust", channel, ...args);

		return originalCefQuery!({
			request: params.request,
			onSuccess: (response: string) => {
				trace.log("Rust → JS", channel, response.length > 200 ? response.slice(0, 200) + "…" : response);
				params.onSuccess?.(response);
			},
			onFailure: (errorCode: number, errorMessage: string) => {
				trace.err("Rust → JS ERR", channel, errorCode, errorMessage);
				params.onFailure?.(errorCode, errorMessage);
			},
		});
	};
};

export const stopIpcLog = () => {
	if (originalCefQuery) {
		window.cefQuery = originalCefQuery;
		originalCefQuery = null;
	}
};
