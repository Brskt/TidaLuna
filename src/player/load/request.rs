use super::sources::{start_stream_load, try_cache_hit, try_preload_hit};
use super::{LoadContext, LoadRequest, LoadSettleGuard, LoadStep, ResumePolicy, SegmentUrls};
use crate::player::buffer::RamBuffer;
use crate::player::resume;
use crate::player::track_identity::{
    is_same_active_track, print_track_banner, retained_product_id,
};
use crate::player::{
    Continuation, LoadOrigin, MediaErrorCode, Player, PlayerCommand, begin_load,
    canonical_track_id, continue_load, current_gen, format_bytes, next_event_seq,
    refresh_retained_credential, short_id,
};
use crate::state::{CURRENT_TRACK, HTTP_CLIENT_PLAYBACK, TrackInfo};
use futures_util::stream::{self, StreamExt};
use tokio_util::sync::CancellationToken;

impl Player {
    /// `load_gen` is minted by the caller, which is where it is decided whose load this is and
    /// whether it may start at all. Answers the event generation every event of this load
    /// carries. Starting a load cannot fail: its failures settle later, as `LoadFailed`.
    fn load_with_policy(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        resume_policy: ResumePolicy,
        auto_play: bool,
        load_gen: u32,
    ) -> u32 {
        let _ = self.cmd_tx.send(PlayerCommand::LoadStarted {
            generation: load_gen,
        });
        let event_seq = next_event_seq();
        crate::vprintln!("[LOAD #{load_gen}] start");
        print_track_banner(&format, product_id.as_deref());

        if let Some(prev) = self
            .load_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            prev.abort();
        }

        // Cancel the previous track's download; a skip must not leak it streaming a
        // full file into RAM (start_download races this token).
        let cancel_token = CancellationToken::new();
        if let Some(prev) = self
            .download_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(cancel_token.clone())
        {
            prev.cancel();
        }

        // Reset governor buffer progress: stale counters from the previous track
        // would otherwise throttle the new download.
        self.governor.reset_buffer_progress();

        {
            let mut lock = CURRENT_TRACK.lock().unwrap_or_else(|e| e.into_inner());
            // Stamped with the generation the caller minted, well before this write: a reader
            // sampling the generation apart could see it while the slot still held the previous
            // track.
            lock.publish_load(
                load_gen,
                Some(TrackInfo {
                    url: url.clone(),
                    format: format.clone(),
                    key: key.clone(),
                    product_id: product_id.clone(),
                }),
            );
        }

        let ctx = LoadContext {
            load_gen,
            event_seq,
            load_start: std::time::Instant::now(),
            resume_policy,
            auto_play,
            cmd_tx: self.cmd_tx.clone(),
            format: format.clone(),
            product_id,
            cancel_token,
            governor: self.governor.clone(),
        };

        let handle = self.rt_handle.spawn(async move {
            let _settle = LoadSettleGuard {
                cmd_tx: ctx.cmd_tx.clone(),
                generation: ctx.load_gen,
            };
            let track = TrackInfo {
                url: url.clone(),
                format: format.clone(),
                key: key.clone(),
                product_id: ctx.product_id.clone(),
            };
            let track_id = canonical_track_id(&url);

            // The track being loaded stops being the "next" one here, whichever of
            // the three branches below serves it. Only the preload-hit branch used
            // to clear the staged record. A cache hit (which returns first, and
            // is what a preload deliberately skips fetching for) left it naming the
            // track that is now playing.
            crate::audio::preload::clear_next_track_if_match(&track_id).await;

            if let LoadStep::Handled = try_cache_hit(&ctx, &track_id, &key).await {
                return;
            }
            if let LoadStep::Handled = try_preload_hit(&ctx, &track, &track_id).await {
                return;
            }
            start_stream_load(&ctx, &url, &key, &track_id).await;
        });

        *self.load_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

        event_seq
    }

    pub fn load(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        restart: bool,
        want_play: bool,
    ) {
        // Idempotent reconcile: when the SDK re-loads the track currently COMMITTED in
        // the pipeline, either restart it (a fresh play instance) or resume it in place
        // (a keep-position re-assert), without a full rebuild. The comparison is against
        // the committed track (set by the player thread in handle_load), never the
        // requested track: a duplicate load racing a still-in-flight load of a
        // different track cannot falsely match and touch a stale pipeline. stop() is
        // pause-retain, and the committed track survives. A genuine track change
        // (different canonical id/format) falls through to a full load.
        let canonical_url_id = canonical_track_id(&url);
        let same_committed = {
            let committed = self
                .committed_track
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            is_same_active_track(committed.as_ref(), &canonical_url_id, &format)
        };
        if same_committed {
            if restart {
                // Fresh play instance (TIDAL minted a new referenceId, threaded here as
                // `restart`): per the SDK's load contract (load = start at 0), restart from 0
                // via a fresh cached load (cache hit, no re-download), auto-playing only if
                // want_play. Reporting position 0 also stops Redux's position-reconcile seek
                // burst that a kept position would otherwise provoke.
                crate::vprintln!(
                    "[LOAD]   same track, fresh play instance -> restart at 0 (want_play={}): {}",
                    want_play,
                    short_id(&canonical_url_id, 60)
                );
                self.load_with_policy(
                    url,
                    format,
                    key,
                    product_id,
                    ResumePolicy::Restart,
                    want_play,
                    begin_load(LoadOrigin::Local),
                );
                return;
            }
            // Takes the new credential, keeps the play instance. The skip below correctly answers
            // "same track?" but must not also mean "still fetchable": every same-track load carries a
            // freshly signed url (HW-measured), and keeping the stale one meant the next reconnect
            // used a signature the CDN had dropped, 403ing the track dead.
            //
            // A refresh, not a rebuild: a quality-swap re-assert still keeps its position; only the
            // credential changes, and `refreshed_fetch_url` is what makes the download read it.
            {
                let mut retained = crate::state::CURRENT_TRACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                refresh_retained_credential(
                    retained.retained_mut(),
                    &canonical_url_id,
                    &url,
                    &key,
                    product_id.as_deref(),
                );
            }
            // Same-track re-assert keeping the play instance (a quality-swap re-load):
            // resumes if PLAYING pre-stop (resume_on_reassert), or if want_play is set --
            // click-to-play on a restored paused track is stop+load(same) with NO
            // player.play (SDK-verified): want_play is the only resume signal then.
            crate::vprintln!(
                "[LOAD]   idempotent reload skipped (same committed track, want_play={}): {}",
                want_play,
                short_id(&canonical_url_id, 60)
            );
            // The id travels with the re-assert: this branch never reaches `handle_load`, which
            // is the only other place the thread learns which track it is measuring. Dropping
            // it here left a gapless-advanced track nameless for the rest of its life.
            let _ = self.send_cmd(PlayerCommand::ReassertResume {
                want_play,
                product_id,
                track_id: canonical_url_id,
            });
            return;
        }
        // Different track (genuine select / queue advance): fresh load. `want_play` folds
        // the SELECT's play-intent into the load: no separate player.play arrives while
        // the OLD track is still committed (decide_play would Resume it -> audible bleed).
        self.load_with_policy(
            url,
            format,
            key,
            product_id,
            ResumePolicy::Disabled,
            want_play,
            begin_load(LoadOrigin::Local),
        );
    }

    /// A load over whatever plays, in `origin`'s name: the Connect receiver's, on a controller's
    /// behalf. The renderer's completion-time advance carries its own load on instead
    /// ([`Player::advance`]). Answers the event generation every event of the load carries.
    pub fn load_and_play(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        origin: LoadOrigin,
    ) -> u32 {
        self.load_with_policy(
            url,
            format,
            key,
            product_id,
            ResumePolicy::Disabled,
            true,
            begin_load(origin),
        )
    }

    /// The completion-time advance: play the track staged as next, if the load it was staged
    /// under is still the current one. Taking the record proved it (`next_is_current`), but this
    /// runs in a spawned task holding no lock, and a controller's load can take the player between
    /// the take and this mint.
    pub fn advance(&self, next: crate::state::RetainedTrack) -> Continuation {
        let Some(load_gen) = continue_load(next.load_gen) else {
            return Continuation::Superseded;
        };
        let track = next.track;
        self.load_with_policy(
            track.url,
            track.format,
            track.key,
            track.product_id,
            ResumePolicy::Disabled,
            true,
            load_gen,
        );
        Continuation::Started
    }

    /// Load a DASH stream by fetching init + media segments and concatenating them. Answers the
    /// event generation every event of the load carries.
    pub fn load_dash(
        &self,
        init_url: String,
        segment_urls: SegmentUrls,
        format: String,
        product_id: Option<String>,
        origin: LoadOrigin,
    ) -> u32 {
        // Both the IPC channel and the Connect receiver reach this function. Which of the two
        // called is why `origin` is a parameter and not something read here: this function
        // cannot tell them apart, and the staged-next guard has to.
        let load_gen = begin_load(origin);
        let _ = self.cmd_tx.send(PlayerCommand::LoadStarted {
            generation: load_gen,
        });
        let event_seq = next_event_seq();
        crate::vprintln!(
            "[DASH-LOAD #{load_gen}] start - {} segments",
            segment_urls.len()
        );
        print_track_banner(&format, product_id.as_deref());

        if let Some(prev) = self
            .load_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            prev.abort();
        }
        self.governor.reset_buffer_progress();

        // DASH isn't replayable via load_and_play (segmented); clear the
        // retained source: a post-DASH play re-arms nothing, not a stale track.
        CURRENT_TRACK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .publish_load(load_gen, None);

        let cmd_tx = self.cmd_tx.clone();
        let handle = self.rt_handle.spawn(async move {
            let _settle = LoadSettleGuard {
                cmd_tx: cmd_tx.clone(),
                generation: load_gen,
            };
            let load_start = std::time::Instant::now();
            let is_stale = || current_gen() != load_gen;

            crate::vprintln!("[DASH-LOAD #{load_gen}] fetching init segment...");
            let init_data = match HTTP_CLIENT_PLAYBACK.get(&init_url).send().await {
                Ok(r) if r.status().is_success() => match r.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        let text = crate::util::network_error_text(e);
                        crate::vprintln!("[ERROR]  DASH init segment read failed: {text}");
                        let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                            error: format!("DASH init segment: {text}"),
                            code: MediaErrorCode::NoSuchFile,
                            seq: event_seq,
                            load_gen,
                            product_id: product_id.clone(),
                        });
                        return;
                    }
                },
                Ok(r) => {
                    crate::vprintln!("[ERROR]  DASH init segment HTTP {}", r.status());
                    let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                        error: format!("DASH init HTTP {}", r.status()),
                        code: MediaErrorCode::NoSuchFile,
                        seq: event_seq,
                        load_gen,
                        product_id: product_id.clone(),
                    });
                    return;
                }
                Err(e) => {
                    let text = crate::util::network_error_text(e);
                    crate::vprintln!("[ERROR]  DASH init segment request failed: {text}");
                    let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                        error: format!("DASH init request: {text}"),
                        code: MediaErrorCode::NoSuchFile,
                        seq: event_seq,
                        load_gen,
                        product_id: product_id.clone(),
                    });
                    return;
                }
            };

            if is_stale() {
                crate::vprintln!("[DASH-LOAD #{load_gen}] stale after init, dropping");
                return;
            }

            crate::vprintln!(
                "[DASH-LOAD #{load_gen}] init: {} | {:.0}ms",
                format_bytes(init_data.len() as u64),
                load_start.elapsed().as_secs_f64() * 1000.0
            );

            let segment_count = segment_urls.len();

            // Fetch segments with bounded concurrency, appending each to the
            // output buffer as it arrives and dropping it immediately. This caps
            // both peak RAM (no transient second copy of the whole file) and the
            // number of simultaneous CDN connections, unlike a join_all that
            // fires every segment request at once. `buffered` preserves order.
            const DASH_MAX_CONCURRENT: usize = 6;

            let mut mp4_data = init_data;
            let mut segments = stream::iter(segment_urls.into_iter().enumerate())
                .map(|(i, url)| async move {
                    match HTTP_CLIENT_PLAYBACK.get(&url).send().await {
                        Ok(r) if r.status().is_success() => r.bytes().await.map_err(|e| {
                            format!(
                                "DASH segment {i} read: {}",
                                crate::util::network_error_text(e)
                            )
                        }),
                        Ok(r) => Err(format!("DASH segment {i} HTTP {}", r.status())),
                        Err(e) => Err(format!(
                            "DASH segment {i} request: {}",
                            crate::util::network_error_text(e)
                        )),
                    }
                })
                .buffered(DASH_MAX_CONCURRENT);

            while let Some(result) = segments.next().await {
                if is_stale() {
                    crate::vprintln!("[DASH-LOAD #{load_gen}] stale mid-fetch, dropping");
                    return;
                }
                match result {
                    Ok(data) => mp4_data.extend_from_slice(&data),
                    Err(msg) => {
                        crate::vprintln!("[ERROR]  {msg}");
                        let _ = cmd_tx.send(PlayerCommand::LoadFailed {
                            error: msg,
                            code: MediaErrorCode::NoSuchFile,
                            seq: event_seq,
                            load_gen,
                            product_id: product_id.clone(),
                        });
                        return;
                    }
                }
            }

            let total_ms = load_start.elapsed().as_secs_f64() * 1000.0;
            crate::vprintln!(
                "[DASH-LOAD #{load_gen}] done: {} segments, {} total in {:.0}ms",
                segment_count,
                format_bytes(mp4_data.len() as u64),
                total_ms
            );

            let buffer = RamBuffer::from_complete(mp4_data);
            let track_id = format!("dash-{load_gen}");
            let _ = cmd_tx.send(PlayerCommand::Load {
                request: LoadRequest {
                    buffer,
                    load_gen,
                    seq: event_seq,
                    track_id,
                    product_id,
                    resume_policy: ResumePolicy::Disabled,
                    load_start,
                    cached: None,
                    format,
                },
                auto_play: true,
            });
        });

        *self.load_handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        event_seq
    }

    /// An explicit target seeks there (when past the resume floor); otherwise
    /// fall back to the stored resume position for the track.
    fn resume_policy_for(target_time: Option<f64>) -> ResumePolicy {
        match target_time {
            Some(t) if t.is_finite() && t > resume::RESUME_MIN_SECONDS => ResumePolicy::Explicit(t),
            _ => ResumePolicy::Auto,
        }
    }

    pub fn recover(&self, url: String, format: String, key: String, target_time: Option<f64>) {
        // The SDK's `recover` delegate carries only a url and a key: the id comes from the
        // retained source instead. A recover re-fetches the track already playing, never
        // another; the retained id is that track's own.
        let product_id = retained_product_id(&canonical_track_id(&url));
        self.load_with_policy(
            url,
            format,
            key,
            product_id,
            Self::resume_policy_for(target_time),
            false,
            begin_load(LoadOrigin::Local),
        );
    }

    /// Re-arm the shared pipeline from a `ReplayRequest`: `Some(position)` seeks
    /// there (past the resume floor; below it starts from the beginning), `None`
    /// restarts; `play` auto-plays. Single entry point for the ReplayRequest
    /// consumer, keeping the dispatch out of flush.rs. `expected_gen` is the load being
    /// re-armed: the same track again, in its owner's name, and only while nothing has minted
    /// since. A re-arm over a newer load would abort it, and over a stop would undo it.
    pub fn rearm(
        &self,
        url: String,
        format: String,
        key: String,
        product_id: Option<String>,
        position: Option<f64>,
        play: bool,
        expected_gen: u32,
    ) -> Continuation {
        let Some(load_gen) = continue_load(expected_gen) else {
            return Continuation::Superseded;
        };
        // Honor the caller position directly (live re-arm target, not a
        // resume-store candidate): resume_policy_for would floor sub-1s to Auto
        // and seek a stale persisted offset. Explicit applies the floor itself.
        let policy = match position {
            Some(t) => ResumePolicy::Explicit(t),
            None => ResumePolicy::Disabled,
        };
        self.load_with_policy(url, format, key, product_id, policy, play, load_gen);
        Continuation::Started
    }
}
