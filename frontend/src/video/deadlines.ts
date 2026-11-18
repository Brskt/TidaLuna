// Deadlines Rust publishes before the page runs, and stops its own decode at: one value on both
// sides. Absent, the caller does not start the work, rather than guess a second value of its own.

type DeadlineName = "FRAGMENT_DEADLINE_MS" | "COVER_PASS_DEADLINE_MS";

/** The deadline in milliseconds, or `null` when Rust published none. */
export function rustDeadlineMs(name: DeadlineName): number | null {
	const ms: unknown = window[`__TIDALUNAR_${name}__`];
	return typeof ms === "number" && Number.isFinite(ms) && ms > 0 ? ms : null;
}
