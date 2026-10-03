//! Tests for `Volume` in `src/player/mod.rs`, attached to it by `#[path]`.

use super::fixture::silent_player;
use super::*;

#[test]
fn ordinary_levels_pass_through() {
    assert_eq!(Volume::from_percent(0.0).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(42.5).as_percent(), 42.5);
    assert_eq!(Volume::from_percent(100.0).as_percent(), 100.0);
}

#[test]
fn out_of_range_levels_are_clamped() {
    assert_eq!(Volume::from_percent(500.0).as_percent(), 100.0);
    assert_eq!(Volume::from_percent(-1.0).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(-500.0).as_percent(), 0.0);
}

#[test]
fn a_large_finite_level_cannot_reach_infinity_at_the_cast() {
    // The actually-reachable path: an ordinary JSON number, well inside f64, whose
    // `/ 100.0` quotient still exceeds f32::MAX (~3.4e38) and casts to infinity.
    // The threshold is the quotient, not the input: 1e40 divides down to 1e38 and stays
    // finite, which is why this test pins 1e41 rather than the first value that looks big.
    let raw = 1e41_f64;
    assert!(raw.is_finite(), "the input itself is an ordinary f64");
    assert!(
        ((raw / 100.0) as f32).is_infinite(),
        "unsanitized, the cast saturates"
    );

    assert_eq!(Volume::from_percent(raw).as_gain(), 1.0);
}

#[test]
fn non_finite_levels_sanitize_to_silence_not_full_scale() {
    // `.clamp()` alone would let NaN through unchanged; the finite check is not
    // redundant. Silence is the safe wrong answer here; full scale is not.
    assert_eq!(Volume::from_percent(f64::NAN).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(f64::NEG_INFINITY).as_percent(), 0.0);
    assert_eq!(Volume::from_percent(f64::INFINITY).as_percent(), 0.0);
    assert!(
        f64::NAN.clamp(0.0, 100.0).is_nan(),
        "the reason from_percent cannot be a bare clamp"
    );
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

#[test]
fn a_mute_repeated_by_the_controller_keeps_the_level_to_restore() {
    assert_eq!(
        level_after(&[&set(55.0), &mute, &mute, &unmute]),
        (55.0, 55.0)
    );
}

#[test]
fn an_unmute_while_audible_leaves_the_level_alone() {
    assert_eq!(level_after(&[&set(40.0), &unmute]), (40.0, 40.0));
}

#[test]
fn a_level_set_while_muted_ends_the_mute_at_that_level() {
    // The later unmute must not jump back to the level the mute silenced.
    assert_eq!(level_after(&[&set(80.0), &mute, &set(25.0)]), (25.0, 25.0));
    assert_eq!(
        level_after(&[&set(80.0), &mute, &set(25.0), &unmute]),
        (25.0, 25.0)
    );
}

#[test]
fn a_mute_taken_locally_is_one_a_remote_unmute_restores() {
    // TIDAL's own mute button reaches the player as a level of zero.
    assert_eq!(level_after(&[&set(64.0), &set(0.0), &unmute]), (64.0, 64.0));
}

#[test]
fn a_level_of_zero_set_while_muted_keeps_the_level_to_restore() {
    // TIDAL's mute puts a level of zero behind itself; it lands on a mute already taken.
    assert_eq!(
        level_after(&[&set(70.0), &mute, &set(0.0), &unmute]),
        (70.0, 70.0)
    );
}

#[test]
fn before_any_level_an_unmute_restores_what_the_thread_starts_at() {
    assert_eq!(level_after(&[&mute, &unmute]), (100.0, 100.0));
}

#[tokio::test]
async fn an_unmute_restores_the_level_the_player_was_given_not_full_scale() {
    let player = silent_player();
    player.set_volume(40.0, None).expect("thread alive");

    let muted = player.set_mute(true).expect("thread alive");
    assert_eq!(muted.volume().as_percent(), 0.0);
    assert_eq!(muted.report().unmute_volume, 40.0);

    let unmuted = player.set_mute(false).expect("thread alive");
    assert_eq!(unmuted.volume().as_percent(), 40.0);
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

fn remote(v: f64) -> impl Fn(&mut OutputLevel) {
    move |level| level.set(Volume::from_percent(v), LevelWriter::Remote)
}

fn report_after(steps: &[&dyn Fn(&mut OutputLevel)]) -> LevelReport {
    let mut level = OutputLevel::START;
    for step in steps {
        step(&mut level);
    }
    level.report()
}

#[test]
fn the_start_level_is_nobodys_change() {
    let report = OutputLevel::START.report();
    assert!(!report.chosen);
    assert_eq!(report.seq, 0);
}

#[test]
fn a_controllers_level_is_newer_than_any_saved_one() {
    let report = report_after(&[&set(70.0), &remote(30.0)]);
    assert!(report.chosen);
    assert_eq!(report.volume, 30.0);
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

#[test]
fn a_page_level_after_a_remote_change_is_the_newer_one() {
    // A page's level is a choice too: a page of another account loading next takes it.
    let report = report_after(&[&remote(30.0), &set(50.0)]);
    assert!(report.chosen);
    assert_eq!(report.volume, 50.0);
}

#[test]
fn every_change_is_counted_whoever_made_it() {
    let report = report_after(&[&set(70.0), &remote(30.0), &mute, &unmute, &set(40.0)]);
    assert_eq!(report.seq, 5);
}

#[test]
fn a_page_level_decided_before_a_remote_change_is_refused() {
    let mut level = OutputLevel::START;
    level.set(Volume::from_percent(70.0), LevelWriter::Page);
    level.mute();
    let muted_at = level.report().seq;
    assert!(!level.admits(Some(muted_at - 1)));
    assert!(level.admits(Some(muted_at)));
}

#[test]
fn a_page_level_is_admitted_while_no_remote_change_happened() {
    // A fresh boot with no controller: the page's first level is TIDAL's saved one, and it must
    // reach the player even though the page has taken in no report yet.
    let mut level = OutputLevel::START;
    assert!(level.admits(Some(0)));
    level.set(Volume::from_percent(70.0), LevelWriter::Page);
    assert!(level.admits(Some(0)));
}

#[test]
fn a_sender_that_keeps_no_count_is_taken_at_its_word() {
    let mut level = OutputLevel::START;
    level.set(Volume::from_percent(30.0), LevelWriter::Remote);
    assert!(level.admits(None));
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

// One level per install. Every page is a fresh document whose count starts at 0.

#[tokio::test]
async fn a_page_loading_after_another_pages_level_cannot_undo_it() {
    // Alice sets 30 from her page, logs out, and Bob's page builds its player at the store's
    // initial full scale before it caught up: no remote change separates the two.
    let player = silent_player();
    let alice = player.catch_up_page().report().seq;
    player.set_volume(30.0, Some(alice)).expect("thread alive");

    player.set_volume(100.0, Some(0)).expect("thread alive");
    assert_eq!(player.output_level().volume().as_percent(), 30.0);

    let bob = player.catch_up_page().report();
    assert!(bob.chosen);
    assert_eq!(bob.volume, 30.0);
    player
        .set_volume(40.0, Some(bob.seq))
        .expect("thread alive");
    assert_eq!(player.output_level().volume().as_percent(), 40.0);
}

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
async fn a_page_level_sent_before_its_page_caught_up_is_refused() {
    // Sent before the catch-up's reply reached the page: it still carries the older count.
    let player = silent_player();
    player.set_volume(30.0, None).expect("thread alive");
    let seen = player.catch_up_page().report().seq;
    player
        .set_volume(70.0, Some(seen - 1))
        .expect("thread alive");
    assert_eq!(player.output_level().volume().as_percent(), 30.0);
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

#[test]
fn a_catch_up_changes_no_level_and_chooses_none() {
    let mut level = OutputLevel::START;
    level.catch_up();
    let report = level.report();
    assert!(!report.chosen);
    assert_eq!((report.volume, report.unmute_volume), (100.0, 100.0));
    assert!(!level.admits(Some(0)));
    assert!(level.admits(Some(report.seq)));
}
