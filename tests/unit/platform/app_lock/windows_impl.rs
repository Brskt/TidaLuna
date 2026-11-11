//! Tests for `src/platform/app_lock/windows_impl.rs`, attached to it by `#[path]`.

/// Real kernel objects, one account. Every name carries a `Test` infix and the pid, never
/// meeting a running app.
#[cfg(windows)]
mod kernel {
    use super::super::{Names, acquire_with, wide};

    /// Runs this test binary again as a second process, running `test` of this module alone
    /// with `var` set to `value`, and fails unless that test ran and passed: a filter that
    /// matches nothing exits 0 having run none.
    fn relaunch(test: &str, var: &str, value: &str) {
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, rest)| rest);
        let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([&format!("{module}::{test}"), "--exact", "--test-threads=1"])
            .env(var, value)
            .output()
            .expect("launch the second process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "the second process did not run `{test}` to a pass ({}):\n{stdout}",
            output.status
        );
    }

    /// The tag the second launch runs under, set in the child it launches.
    const CLOSING_HOLDER: &str = "TIDALUNAR_TEST_CLOSING_HOLDER";

    /// Names of one test's own, none of them a running app's.
    fn names_for(tag: &str) -> Names {
        Names {
            namespace: format!("TidaLunarTest-{tag}"),
            pipe: wide(&format!("\\\\.\\pipe\\TidaLunarAppLinkTest-{tag}")),
            legacy_lock: wide(&format!("Global\\TidaLunarAppLockTest-{tag}")),
            legacy_focus: wide(&format!("Global\\TidaLunarAppFocusTest-{tag}")),
        }
    }

    /// A launch that came in while the holder closed used to write its link into a pipe nobody
    /// read and leave; it now waits on the lock and holds it once the old holder is gone. This
    /// process is the holder, on its own thread (the thread that ends releases the mutex).
    #[test]
    fn a_launch_that_finds_the_holder_closing_outwaits_it_and_takes_over() {
        if let Some(tag) = std::env::var_os(CLOSING_HOLDER) {
            let tag = tag.to_string_lossy().into_owned();
            let launched = acquire_with(Some("tidal://album/1"), |_| names_for(&tag));
            assert!(
                launched.is_some(),
                "the second launch left instead of taking over"
            );
            return;
        }

        let tag = format!("closing-{}", std::process::id());
        let (held, holding) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn({
            let tag = tag.clone();
            move || {
                let lock = acquire_with(None, |_| names_for(&tag))
                    .expect("the first launch holds the lock");
                lock.closer.as_ref().expect("a closer").close();
                held.send(()).expect("the test is waiting");
                released.recv().expect("the test releases the holder");
                drop(lock);
            }
        });
        holding.recv().expect("the holder never closed");
        // Long enough for the child to start and find the lock taken.
        let releasing = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            release.send(()).expect("the holder is waiting");
        });

        relaunch(
            "a_launch_that_finds_the_holder_closing_outwaits_it_and_takes_over",
            CLOSING_HOLDER,
            &tag,
        );
        releasing.join().expect("the releasing thread");
        holder.join().expect("the holder thread");
    }
}
