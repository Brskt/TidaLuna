/**
 * One run per key at a time: callers arriving while a key is in flight join it instead of
 * starting a second run.
 *
 * The gap it closes is the window between asking and answering. A cache filled on RESOLUTION
 * says nothing while the work is still running: every caller arriving in that window starts
 * its own, and a row leaving and re-entering the DOM asks again, which a scrolled list does
 * constantly. Measured on a 397-track playlist: about four loads dispatched per track, three of
 * them for an answer their caller would discard, and the server served every one.
 *
 * Holding the PROMISE rather than a flag is what lets a joiner have the answer instead of being
 * turned away with nothing.
 */
export class SingleFlight<T> {
	private readonly running = new Map<string, Promise<T>>();

	/** How many keys are being worked on right now. */
	public get inFlight(): number {
		return this.running.size;
	}

	public async run(key: string, work: () => Promise<T>): Promise<T> {
		const joined = this.running.get(key);
		if (joined !== undefined) return joined;

		const started = work();
		this.running.set(key, started);
		try {
			return await started;
		} finally {
			// In `finally`, which clears the key on a rejection too. Left behind, a single
			// failure would be handed to every later caller for the life of the page: the one
			// outcome worse than repeating the work.
			this.running.delete(key);
		}
	}
}
