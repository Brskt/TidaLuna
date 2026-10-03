// TIDAL's `playbackControls` store as the volume tests fake it, its rules copied from the shipped
// bundle (store-DoF6ExYq.js, index-QBitWpxd.js):
//   reducer .addCase(SET_MUTE, (e,{payload:{mute:t}}) => { t && (e.volumeUnmute = e.volume) })
//           .addMatcher(INDICATE_MUTE|SET_MUTE, (e,{payload:{mute:t}}) => { e.muted = t })
//           .addMatcher(SET_VOLUME|INDICATE_VOLUME, (e,{payload:{volume:t}}) => { e.volume = t, t > 0 && (e.muted = false) })
//           .addMatcher(SET_VOLUME_UNMUTE, (e,{payload:{volume:t}}) => { e.volumeUnmute = t })
//   KDe (on SET_MUTE)    function*(e){ let t = 0; e.payload.mute || (t = yield select(volumeUnmute)); yield put(SET_VOLUME({volume:t})) }
//   qDe (on TOGGLE_MUTE) function*(){ yield put(SET_MUTE({mute: !(yield select(muted))})) ... }
//   Z8 (on SET_VOLUME)   function*(){ yield call(persist, {mute, volume, volumeUnmute}) }
//   Hwe (on SET_VOLUME)  function*(e){ yield call(setDesiredVolume, e.payload.volume/100) }
//   JDe (on BOOTSTRAP_DONE) function*(){ let e = yield call(readSaved); if(!e) return;
//       let t = yield select(muted), n = yield select(volume), r = yield select(volumeUnmute);
//       t !== e.mute && (yield put(TOGGLE_MUTE()));
//       n !== e.volume && (yield put(SET_VOLUME({volume: e.volume})));
//       r !== e.volumeUnmute && (yield put(SET_VOLUME_UNMUTE({volume: e.volumeUnmute}))) }
// Z8 and Hwe are the test's own `onSetVolume`: where a file saves the level, and whether TIDAL's
// player has a stream to apply it to, is what each one varies. Subscribers run in redux's order:
// after the reducer, before the sagas the action wakes.

export type Controls = { volume: number; volumeUnmute: number; muted: boolean };
export type Saved = { mute: boolean; volume: number; volumeUnmute: number };
type Action = { type: string; payload?: any };

export const INITIAL: Controls = { volume: 100, volumeUnmute: 100, muted: false };

export function playbackControls(onSetVolume: (volume: number) => void) {
	const controls: Controls = { ...INITIAL };
	const listeners = new Set<() => void>();

	function reduce({ type, payload }: Action) {
		if (type === "playbackControls/SET_MUTE" && payload.mute) controls.volumeUnmute = controls.volume;
		if (type === "playbackControls/SET_MUTE" || type === "playbackControlsActions/INDICATE_MUTE") {
			controls.muted = payload.mute;
		}
		if (type === "playbackControls/SET_VOLUME" || type === "playbackControlsActions/INDICATE_VOLUME") {
			controls.volume = payload.volume;
			if (payload.volume > 0) controls.muted = false;
		}
		if (type === "playbackControls/SET_VOLUME_UNMUTE") controls.volumeUnmute = payload.volume;
	}

	// TIDAL's own dispatch, sagas included; its sagas' puts come through here too.
	const store = {
		dispatch(action: Action) {
			reduce(action);
			for (const cb of [...listeners]) cb();
			if (action.type === "playbackControls/SET_MUTE") {
				// KDe
				store.dispatch({
					type: "playbackControls/SET_VOLUME",
					payload: { volume: action.payload.mute ? 0 : controls.volumeUnmute },
				});
			}
			if (action.type === "playbackControls/TOGGLE_MUTE") {
				// qDe
				store.dispatch({ type: "playbackControls/SET_MUTE", payload: { mute: !controls.muted } });
			}
			// Z8, then Hwe
			if (action.type === "playbackControls/SET_VOLUME") onSetVolume(action.payload.volume);
		},
		getState: () => ({ playbackControls: controls }),
		subscribe(cb: () => void) {
			listeners.add(cb);
			return () => listeners.delete(cb);
		},
	};

	// JDe's puts, once its read of the saved level resolved.
	function restore(e: Saved | null | undefined) {
		if (!e) return;
		const t = controls.muted;
		const n = controls.volume;
		const r = controls.volumeUnmute;
		if (t !== e.mute) store.dispatch({ type: "playbackControls/TOGGLE_MUTE" });
		if (n !== e.volume) store.dispatch({ type: "playbackControls/SET_VOLUME", payload: { volume: e.volume } });
		if (r !== e.volumeUnmute) {
			store.dispatch({ type: "playbackControls/SET_VOLUME_UNMUTE", payload: { volume: e.volumeUnmute } });
		}
	}

	// Puts the store at `state` without a notification, and drops every subscriber.
	function reset(state: Controls) {
		listeners.clear();
		Object.assign(controls, state);
	}

	return { controls, store, restore, reset };
}
