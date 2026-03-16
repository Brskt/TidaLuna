import { findModuleProperty } from "@luna/core";
import { store } from "../redux/store";

type TidalCredentials = {
	clientId: string;
	clientUniqueKey: string;
	expires: number;
	grantedScopes: string[];
	requestedScopes: string[];
	token: string;
	userId: string;
};

type GetCredentials = () => Promise<TidalCredentials>;
export const getCredentials = async (): Promise<TidalCredentials> => {
	// Strategy 1: upstream — find getCredentials in TIDAL's webpack modules
	const getCredentialsFn = findModuleProperty<GetCredentials>((key, value) => key === "getCredentials" && typeof value === "function")?.value;
	if (getCredentialsFn) {
		const creds = await getCredentialsFn();
		if (creds) return creds;
	}

	// Strategy 2: CEF fallback — extract from Redux store + captured Bearer token
	const state = store.getState();
	const session = state?.session;
	if (!session?.clientId) throw new Error("Could not find Tidal credentials (no session in Redux store)");

	const token = (window as any).__LUNAR_CAPTURED_TOKEN__ ?? "";
	if (!token) throw new Error("Tidal OAuth token not yet captured (no API request observed yet)");

	return {
		clientId: session.clientId,
		clientUniqueKey: session.clientUniqueKey ?? "",
		expires: 0,
		grantedScopes: [],
		requestedScopes: [],
		token,
		userId: String(session.userId ?? ""),
	};
};
