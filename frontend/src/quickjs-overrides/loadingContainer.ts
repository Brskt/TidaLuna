// QuickJS override: no DOM available.

const stub = { innerText: "", style: { opacity: "" }, innerHTML: "" };
export const getOrCreateLoadingContainer = () => ({
	loadingContainer: stub,
	messageContainer: stub,
});
