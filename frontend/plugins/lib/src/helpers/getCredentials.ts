export type TidalCredentials = {
	clientId: string;
	clientUniqueKey: string;
	expires: number;
	grantedScopes: string[];
	requestedScopes: string[];
	token: string;
	userId: string;
};

/** @deprecated Token is now managed server-side by Rust. Use TidalApi.fetch() instead — authentication is handled transparently. */
export const getCredentials = async (): Promise<never> => {
	throw new Error("getCredentials() removed for security. Use TidalApi.fetch() — authentication is handled transparently.");
};
