// Rust's level reports as the tests fake them (`LevelReport`, src/player/mod.rs). One count for
// every test of a file: the page's count of what it has taken in (`VolumeForwarder`) is module
// state shared the same way. Each report has to be newer than any an earlier test handed it.

import type { OutputLevelReport } from "../../src/volume";
import type { SentIpc } from "../setup";

let seq = 0;

export function rustReport(
	volume: number,
	unmuteVolume: number,
	{ chosen = true }: { chosen?: boolean } = {},
): OutputLevelReport {
	return { volume, unmuteVolume, mute: volume === 0, chosen, seq: ++seq };
}

// Rust's level as a page meets it, restating `OutputLevel`'s rules (proved on the Rust side in
// tests/unit/player/volume_tests.rs) to follow a page load end to end: a page's level applies
// once the page took in the newest catch-up or remote change, and one the output already plays
// changes nothing.
export class FakeRust {
	volume = 100;
	unmuteVolume = 100;
	chosen = false;
	floor = 0;
	seq = 0;

	report(): OutputLevelReport {
		return { volume: this.volume, unmuteVolume: this.unmuteVolume, mute: this.volume === 0, chosen: this.chosen, seq: this.seq };
	}

	// A Connect controller, the OS mixer, or the level an earlier launch left.
	remote(volume: number, unmuteVolume: number): OutputLevelReport {
		this.seq = ++seq;
		this.floor = this.seq;
		this.chosen = true;
		this.volume = volume;
		this.unmuteVolume = unmuteVolume;
		return this.report();
	}

	// `connect.catch_up_level`.
	catchUp(): OutputLevelReport {
		this.seq = ++seq;
		this.floor = this.seq;
		return this.report();
	}

	// `player.volume`: a page's level with the count it carried, or a plugin's with none.
	page(volume: number, seen?: number): void {
		if (seen !== undefined && seen < this.floor) return;
		if (volume === this.volume) return;
		this.seq = ++seq;
		this.chosen = true;
		this.volume = volume;
		if (volume > 0) this.unmuteVolume = volume;
	}

	applies(sent: SentIpc[]): void {
		for (const { channel, args } of sent) {
			if (channel === "player.volume") this.page(args[0] as number, args[1] as number | undefined);
		}
		sent.length = 0;
	}
}
