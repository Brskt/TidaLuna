/**
 * A ceiling on how fast we ask TIDAL for anything, in starts per second.
 *
 * Newest first, and that is the whole design. A list view queues one request per row as the row
 * enters the DOM; scrolling past a few hundred rows in seconds books minutes of work, and
 * served in arrival order, the rows on screen wait behind every row already scrolled past.
 * Measured on a real playlist: 46 seconds of queue, which reads as "it stopped loading" though
 * nothing had stopped. The most recently asked-for row is the one the listener is looking at:
 * it goes first.
 *
 * Nothing is ever dropped. A row scrolled past keeps its place at the back and is served once
 * the queue drains; a row cannot end up permanently blank for having been unlucky. Being
 * held back lasts only as long as new rows keep arriving, and they stop the moment scrolling
 * does.
 *
 * An idle gate serves at once rather than banking what it did not spend: a quiet minute must
 * not become a minute's worth of burst, which is what a token bucket would do here and the
 * opposite of what politeness wants.
 */
export class RateGate {
	private readonly spacingMs: number;
	private readonly waiting: Array<() => void> = [];
	private lastRelease = 0;
	private pumping = false;

	constructor(perSecond: number) {
		this.spacingMs = 1000 / perSecond;
	}

	/** How many callers are still waiting their turn - what "it stopped loading" actually means. */
	public get queued(): number {
		return this.waiting.length;
	}

	public pass(): Promise<void> {
		const { promise, resolve } = Promise.withResolvers<void>();
		this.waiting.push(resolve);
		this.pump();
		return promise;
	}

	private pump(): void {
		// One pump however many callers arrive: a second would double the rate rather than share
		// it, which is the one thing this class exists to prevent.
		if (this.pumping) return;
		this.pumping = true;

		const step = () => {
			const next = this.waiting.pop();
			if (next === undefined) {
				this.pumping = false;
				return;
			}
			this.lastRelease = Date.now();
			next();
			setTimeout(step, this.spacingMs);
		};
		// Counted from the last release rather than from now. A caller arriving just after one
		// went out waits the remainder of that interval instead of a whole fresh one.
		setTimeout(step, Math.max(0, this.lastRelease + this.spacingMs - Date.now()));
	}
}
