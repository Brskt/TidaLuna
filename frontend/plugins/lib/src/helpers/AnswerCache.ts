/**
 * Answers kept by key for the life of the page; a failed one is forgotten and the next caller asks
 * again. `memoize` from @inrixia/helpers forgets it by rethrowing from a promise nobody holds, which
 * adds an unhandled rejection to every failure its caller already sees.
 */
export class AnswerCache<T> {
	private readonly answers = new Map<string, Promise<T>>();

	public get(key: string, work: () => Promise<T>): Promise<T> {
		const kept = this.answers.get(key);
		if (kept !== undefined) return kept;

		const answer = work();
		this.answers.set(key, answer);
		// Settles without a rejection of its own: the caller holds `answer` and sees the failure.
		answer.catch(() => {
			if (this.answers.get(key) === answer) this.answers.delete(key);
		});
		return answer;
	}

	/** Forgets one key's answer, or every answer. */
	public clear(key?: string): void {
		if (key === undefined) this.answers.clear();
		else this.answers.delete(key);
	}
}
