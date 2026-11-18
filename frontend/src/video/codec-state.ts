import { invokeIpc, onIpcEvent, sendIpc } from "../ipc";

/** The backend's event and the page's pull request share one name. */
const CHANNEL = "settings.video_codec_state";

const KINDS = ["starting", "off", "unsupported", "downloading", "ready", "failed"] as const;

type CodecPhase = { kind: (typeof KINDS)[number]; reason?: string };
type CodecSnapshot = { seq: number; enabled: boolean; phase: CodecPhase };

let current: CodecSnapshot | null = null;
let watching = false;

const parse = (raw: unknown): CodecSnapshot | null => {
	const s = raw as Partial<CodecSnapshot> | null;
	if (!s || typeof s.seq !== "number" || typeof s.enabled !== "boolean") return null;
	const phase = s.phase as CodecPhase | undefined;
	if (!phase || !KINDS.includes(phase.kind)) return null;
	return { seq: s.seq, enabled: s.enabled, phase };
};

// A pull answer can land after a newer push: only a higher seq replaces what is held.
const accept = (raw: unknown) => {
	const next = parse(raw);
	if (!next || (current && next.seq <= current.seq)) return;
	current = next;
};

export function watchCodecState(): void {
	if (watching) return;
	watching = true;
	onIpcEvent(CHANNEL, accept);
	invokeIpc(CHANNEL).then(accept, (e) => console.warn("[video] the codec state pull failed:", e));
}

export function codecReady(): boolean {
	return current?.phase.kind === "ready";
}

/** The request a clip asks with: answered with the snapshot once the backend's step settled. */
const ASK_CHANNEL = "settings.video_codec_ask";

/** What an ask came to: a codec to decode with, or why there is none. */
export type CodecAnswer = { ready: true } | { ready: false; refusal: string };

const answerNow = (): CodecAnswer => (codecReady() ? { ready: true } : { ready: false, refusal: codecRefusal() });

/**
 * Asks the backend for the codec and waits for its step, at most `timeoutMs`. At the bound the answer
 * is the snapshot's: the step goes on, and its late reply still lands in the snapshot.
 */
export function askCodec(timeoutMs: number): Promise<CodecAnswer> {
	return new Promise((resolve) => {
		const timer = setTimeout(() => resolve(answerNow()), timeoutMs);
		const settle = (answer: CodecAnswer) => {
			clearTimeout(timer);
			resolve(answer);
		};
		invokeIpc(ASK_CHANNEL).then(
			(reply) => {
				accept(reply);
				settle(answerNow());
			},
			// The refusal the backend worded (503: its service has stopped).
			(e) => settle({ ready: false, refusal: e instanceof Error ? e.message : codecRefusal() }),
		);
	});
}

// The three refusals mirror `NoDecoder::phrase()` in src/video/codec/supervisor.rs: the page and
// the 503 say the same.
export function codecRefusal(): string {
	const phase = current?.phase;
	switch (phase?.kind) {
		case "off":
			return "video decoding is turned off in settings";
		case "unsupported":
			return "no video codec is published for this platform";
		case "failed":
			return phase.reason ?? "the video codec service has stopped";
		case "ready":
			return "the video codec is ready";
		default:
			return "the video codec is not ready yet";
	}
}

export function noteCodecDemand(): void {
	sendIpc("settings.video_codec_demand");
}
