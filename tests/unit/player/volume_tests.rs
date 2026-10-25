//! Tests for `Volume` in `src/player/mod.rs`, attached to it by `#[path]`.

use super::fixture::silent_player;
use super::*;

/// A level is clamped to 0..=100, and a non-finite one sanitizes to silence, never full scale:
/// `.clamp()` alone would let NaN through. 1e41 is an ordinary JSON number whose `/ 100.0`
/// quotient still exceeds f32::MAX (1e40 does not), and it must not reach the cast as infinity.
#[test]
fn levels_are_clamped_and_never_reach_full_scale_by_overflow_or_nan() {
    for (input, percent, gain) in [
        (500.0, 100.0, 1.0),
        (-1.0, 0.0, 0.0),
        (-500.0, 0.0, 0.0),
        (1e41, 100.0, 1.0),
        (f64::NAN, 0.0, 0.0),
        (f64::NEG_INFINITY, 0.0, 0.0),
        (f64::INFINITY, 0.0, 0.0),
    ] {
        let volume = Volume::from_percent(input);
        assert_eq!(volume.as_percent(), percent, "{input}");
        assert_eq!(volume.as_gain(), gain, "{input}");
    }
}

// `OutputLevel`: what a Connect receiver's mute silences and its unmute restores.

fn level_after(steps: &[&dyn Fn(&mut OutputLevel)]) -> (f64, f64) {
    let mut level = OutputLevel::START;
    for step in steps {
        step(&mut level);
    }
    (level.volume().as_percent(), level.report().unmute_volume)
}

fn set(v: f64) -> impl Fn(&mut OutputLevel) {
    move |level| level.set(Volume::from_percent(v), LevelWriter::Page)
}

fn mute(level: &mut OutputLevel) {
    level.mute();
}

fn unmute(level: &mut OutputLevel) {
    level.unmute();
}

/// A mute, however it was taken (repeated by the controller, a level set behind it, or TIDAL's own
/// button reaching the player as a level of zero), leaves the level an unmute restores: the later
/// unmute must not jump back to the level the mute silenced.
#[test]
fn the_level_an_unmute_restores_follows_every_way_of_muting() {
    type Steps<'a> = &'a [&'a dyn Fn(&mut OutputLevel)];
    let rows: [(Steps, (f64, f64)); 5] = [
        (&[&set(55.0), &mute, &mute, &unmute], (55.0, 55.0)),
        (&[&set(80.0), &mute, &set(25.0)], (25.0, 25.0)),
        (&[&set(80.0), &mute, &set(25.0), &unmute], (25.0, 25.0)),
        (&[&set(64.0), &set(0.0), &unmute], (64.0, 64.0)),
        (&[&set(70.0), &mute, &set(0.0), &unmute], (70.0, 70.0)),
    ];
    for (at, (steps, expected)) in rows.into_iter().enumerate() {
        assert_eq!(level_after(steps), expected, "row {at}");
    }
}

#[tokio::test]
async fn a_level_the_os_mixer_reported_is_the_one_a_mute_restores() {
    let player = silent_player();
    player.set_volume(70.0, None).expect("thread alive");
    player.note_volume(30.0);

    player.set_mute(true).expect("thread alive");
    let unmuted = player.set_mute(false).expect("thread alive");
    assert_eq!(unmuted.volume().as_percent(), 30.0);
}

// Which of the two memories of this level is the newer one when a page loads: TIDAL's saved
// level, or the one a Connect controller or the OS mixer left in Rust while no page could see it.

fn report_after(steps: &[&dyn Fn(&mut OutputLevel)]) -> LevelReport {
    let mut level = OutputLevel::START;
    for step in steps {
        step(&mut level);
    }
    level.report()
}

#[test]
fn a_controllers_mute_and_unmute_are_newer_than_any_saved_level() {
    let muted = report_after(&[&set(70.0), &mute]);
    assert!(muted.chosen);
    assert!(muted.mute);
    assert_eq!((muted.volume, muted.unmute_volume), (0.0, 70.0));

    let unmuted = report_after(&[&set(70.0), &mute, &unmute]);
    assert!(unmuted.chosen);
    assert!(!unmuted.mute);
}

#[tokio::test]
async fn a_page_level_sent_before_the_page_knew_of_a_remote_mute_leaves_the_mute_alone() {
    // The page loaded after a phone muted this receiver, and TIDAL applied its saved level to a
    // new player before the page mirrored Rust's: that level must not undo the mute.
    let player = silent_player();
    player.set_volume(70.0, Some(0)).expect("thread alive");
    let muted = player.set_mute(true).expect("thread alive").report();

    player.set_volume(70.0, Some(0)).expect("thread alive");
    assert_eq!(player.output_level().report(), muted);

    player
        .set_volume(55.0, Some(muted.seq))
        .expect("thread alive");
    let report = player.output_level().report();
    assert_eq!(report.volume, 55.0);
    assert!(report.chosen);
}

#[tokio::test]
async fn a_controllers_level_is_reported_as_the_newer_one() {
    let player = silent_player();
    player.set_volume(70.0, None).expect("thread alive");
    let level = player.set_remote_volume(30.0).expect("thread alive");
    assert_eq!(level.report(), player.output_level().report());
    assert!(level.report().chosen);
    assert_eq!(level.volume().as_percent(), 30.0);
}

#[tokio::test]
async fn an_os_mixer_move_is_reported_as_the_newer_level() {
    let player = silent_player();
    player.set_volume(70.0, None).expect("thread alive");
    let level = player.note_volume(25.0);
    assert!(level.report().chosen);
    assert!(!player.output_level().admits(Some(0)));
}

// The level across a restart: every launch is a new player on the same settings database.

struct Launch {
    player: Player,
    settings: crate::db::DbActor,
}

fn launch(dir: &std::path::Path) -> Launch {
    let settings = crate::db::DbActor::open(dir).expect("settings database");
    let player = Player::with_settings(
        |_| {},
        tokio::runtime::Handle::current(),
        Some(settings.clone()),
    );
    Launch { player, settings }
}

impl Launch {
    /// The quit path: `main` drains the settings queue once the message loop ends.
    fn quit(self) {
        let Launch { player, settings } = self;
        drop(player);
        settings.flush();
    }

    fn saved(&self) -> Option<(f64, f64)> {
        self.settings
            .call_settings(crate::settings::load_output_level)
    }
}

#[tokio::test]
async fn the_next_launch_plays_the_level_the_page_set() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = launch(dir.path());
    first
        .player
        .set_volume(40.0, Some(0))
        .expect("thread alive");
    first.quit();

    let report = launch(dir.path()).player.output_level().report();
    assert_eq!((report.volume, report.unmute_volume), (40.0, 40.0));
    assert!(report.chosen);
}

#[tokio::test]
async fn a_controllers_level_survives_a_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = launch(dir.path());
    first.player.set_volume(70.0, None).expect("thread alive");
    first.player.set_remote_volume(30.0).expect("thread alive");
    first.quit();

    assert_eq!(
        launch(dir.path())
            .player
            .output_level()
            .volume()
            .as_percent(),
        30.0
    );
}

#[tokio::test]
async fn a_controllers_mute_survives_a_restart_with_the_level_to_restore() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = launch(dir.path());
    first.player.set_volume(70.0, None).expect("thread alive");
    first.player.set_mute(true).expect("thread alive");
    first.quit();

    let second = launch(dir.path());
    let report = second.player.output_level().report();
    assert!(report.mute);
    assert_eq!((report.volume, report.unmute_volume), (0.0, 70.0));
    let unmuted = second.player.set_mute(false).expect("thread alive");
    assert_eq!(unmuted.volume().as_percent(), 70.0);
}

#[tokio::test]
async fn an_os_mixer_level_survives_a_restart() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = launch(dir.path());
    first.player.set_volume(70.0, None).expect("thread alive");
    first.player.note_volume(25.0);
    first.quit();

    assert_eq!(
        launch(dir.path())
            .player
            .output_level()
            .volume()
            .as_percent(),
        25.0
    );
}

#[tokio::test]
async fn a_page_level_decided_before_the_restored_one_is_refused() {
    // TIDAL builds its player with the store's initial level before the page mirrored Rust's.
    let dir = tempfile::tempdir().expect("temp dir");
    let first = launch(dir.path());
    first
        .player
        .set_volume(40.0, Some(0))
        .expect("thread alive");
    first.quit();

    let second = launch(dir.path());
    let restored = second.player.output_level().report();
    second
        .player
        .set_volume(100.0, Some(0))
        .expect("thread alive");
    assert_eq!(second.player.output_level().report(), restored);
    second.quit();

    let third = launch(dir.path());
    assert_eq!(third.player.output_level().volume().as_percent(), 40.0);
    third
        .player
        .set_volume(55.0, Some(third.player.output_level().report().seq))
        .expect("thread alive");
    assert_eq!(third.player.output_level().volume().as_percent(), 55.0);
}

#[tokio::test]
async fn every_level_is_written_while_the_app_runs_not_only_at_quit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = launch(dir.path());
    running.player.set_volume(30.0, None).expect("thread alive");
    running
        .player
        .set_remote_volume(45.0)
        .expect("thread alive");
    // No flush: a blocking read is queued behind the writes and observes them.
    assert_eq!(running.saved(), Some((45.0, 45.0)));
}

#[tokio::test]
async fn a_drag_queues_one_write_and_the_newest_level_lands() {
    let dir = tempfile::tempdir().expect("temp dir");
    let running = launch(dir.path());
    // Hold the actor, so every step of the drag finds the write before it still queued.
    let (release, held) = std::sync::mpsc::channel::<()>();
    running.settings.post(move |_, _| {
        let _ = held.recv();
    });
    // More steps than the actor's queue holds: one write per step would block the caller.
    let steps = 3 * crate::db::QUEUE_CAPACITY as u32;
    let player = std::sync::Arc::new(running.player);
    let dragging = std::sync::Arc::clone(&player);
    let (done_tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for step in 0..=steps {
            dragging
                .set_remote_volume(f64::from(step) * 100.0 / f64::from(steps))
                .expect("thread alive");
        }
        let _ = done_tx.send(());
    });
    let finished = done.recv_timeout(std::time::Duration::from_secs(10));
    release.send(()).expect("actor waiting");
    finished.expect("a drag must not wait on the settings actor");
    assert_eq!(
        running
            .settings
            .call_settings(crate::settings::load_output_level),
        Some((100.0, 100.0))
    );
}

#[test]
fn a_restored_level_keeps_the_invariant_of_the_level_to_restore() {
    // Audible, the level to restore is that level itself; a saved one below it is stale.
    let audible = OutputLevel::restored(Volume::from_percent(50.0), Volume::from_percent(20.0));
    assert_eq!(audible.report().unmute_volume, 50.0);
    // Muted with nothing audible to restore, as a hand-edited row could leave it.
    let mut lost = OutputLevel::restored(Volume::from_percent(0.0), Volume::from_percent(0.0));
    lost.unmute();
    assert_eq!(lost.volume().as_percent(), 100.0);
}

// One level per install. Every page is a fresh document whose count starts at 0.

#[tokio::test]
async fn a_page_keeps_setting_levels_after_it_caught_up() {
    let player = silent_player();
    let seen = player.catch_up_page().report().seq;
    for volume in [30.0, 45.0, 0.0, 60.0] {
        player.set_volume(volume, Some(seen)).expect("thread alive");
        assert_eq!(player.output_level().volume().as_percent(), volume);
    }
}

#[tokio::test]
async fn a_page_that_caught_up_earlier_cannot_undo_the_level_a_later_one_took_in() {
    let player = silent_player();
    let alice = player.catch_up_page().report().seq;
    player.set_volume(30.0, Some(alice)).expect("thread alive");
    let bob = player.catch_up_page().report().seq;
    player.set_volume(55.0, Some(alice)).expect("thread alive");
    assert_eq!(player.output_level().volume().as_percent(), 30.0);
    player.set_volume(55.0, Some(bob)).expect("thread alive");
    assert_eq!(player.output_level().volume().as_percent(), 55.0);
}

#[tokio::test]
async fn a_first_launch_page_applying_full_scale_is_nobodys_choice() {
    // TIDAL applies the store's initial level before the page caught up and after.
    let player = silent_player();
    player.set_volume(100.0, Some(0)).expect("thread alive");
    let caught_up = player.catch_up_page().report();
    assert!(!caught_up.chosen);
    player
        .set_volume(100.0, Some(caught_up.seq))
        .expect("thread alive");
    assert!(!player.output_level().report().chosen);

    // TIDAL's saved level, restored after, seeds it.
    player
        .set_volume(70.0, Some(caught_up.seq))
        .expect("thread alive");
    let seeded = player.output_level().report();
    assert!(seeded.chosen);
    assert_eq!(seeded.volume, 70.0);
}

#[tokio::test]
async fn a_first_launch_seeded_before_the_page_caught_up_is_the_level_it_takes() {
    // TIDAL restored its saved level before our bundle ran: the native player applies it first.
    let player = silent_player();
    player.set_volume(70.0, Some(0)).expect("thread alive");
    let caught_up = player.catch_up_page().report();
    assert!(caught_up.chosen);
    assert_eq!(caught_up.volume, 70.0);
}

#[tokio::test]
async fn a_plugins_level_is_chosen_and_leaves_the_page_in_line() {
    let player = silent_player();
    let seen = player.catch_up_page().report().seq;
    player.set_volume(40.0, None).expect("thread alive");
    assert!(player.output_level().report().chosen);
    player.set_volume(35.0, Some(seen)).expect("thread alive");
    assert_eq!(player.output_level().volume().as_percent(), 35.0);
}
