//! Tests for the Windows half of `src/platform/app_lock.rs`, attached by `#[path]`.

use super::*;

const USER: &str = "S-1-5-21-1000000001-2000000002-3000000003-1000";
const OTHER: &str = "S-1-5-21-1000000001-2000000002-3000000003-1001";
const ADMINISTRATORS: &str = "S-1-5-32-544";

fn limited() -> OwnIdentity {
    OwnIdentity {
        user: USER.into(),
        default_owner: USER.into(),
    }
}

fn elevated() -> OwnIdentity {
    OwnIdentity {
        user: USER.into(),
        default_owner: ADMINISTRATORS.into(),
    }
}

/// Our elevated token stamps the Administrators owner, as another admin account's elevated
/// process does, and a limited token of ours never does. Another account's pipe, a SID that
/// merely starts like ours and an unreadable owner are not ours either.
#[test]
fn a_link_pipe_is_ours_only_when_our_own_token_created_it() {
    let lookalike = format!("{USER}0");
    for (what, owner, identity, expected) in [
        ("our account", Some(USER), limited(), ObjectOwner::Ours),
        (
            "our elevated token",
            Some(ADMINISTRATORS),
            elevated(),
            ObjectOwner::Ours,
        ),
        (
            "administrators beside a limited token",
            Some(ADMINISTRATORS),
            limited(),
            ObjectOwner::Foreign(ADMINISTRATORS.into()),
        ),
        (
            "another account",
            Some(OTHER),
            limited(),
            ObjectOwner::Foreign(OTHER.into()),
        ),
        (
            "a SID that starts like ours",
            Some(lookalike.as_str()),
            limited(),
            ObjectOwner::Foreign(lookalike.clone()),
        ),
        (
            "an owner that could not be read",
            None,
            limited(),
            ObjectOwner::Unattributed,
        ),
    ] {
        assert_eq!(classify_owner(owner, &identity), expected, "{what}");
    }
}

/// Real kernel objects, one account. Every name carries a `Test` infix and the pid, never
/// meeting a running app.
#[cfg(windows)]
mod kernel {
    use super::super::windows_impl::{
        LOCK_LEAF, Legacy, UserNamespace, connect_link_pipe, create_instance, in_namespace,
        legacy_lock, open_instance, own_identity, owner_sid, wide,
    };
    use super::*;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_CANT_OPEN_ANONYMOUS, FALSE,
        GetLastError, HANDLE, LocalFree, WIN32_ERROR,
    };
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSID, RevertToSelf, TOKEN_QUERY, TokenImpersonationLevel,
    };
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    };
    use windows_sys::Win32::System::Threading::{
        CreateMutexW, GetCurrentThread, MUTEX_ALL_ACCESS, OpenMutexW, OpenThreadToken,
    };

    fn test_namespace(tag: &str) -> String {
        format!("TidaLunarTest-{tag}-{}", std::process::id())
    }

    fn own_namespace(name: &str) -> Result<UserNamespace, WIN32_ERROR> {
        let (_, user) = own_identity().expect("our identity");
        UserNamespace::open(name, user.sid())
    }

    /// LocalSystem, a SID only SYSTEM's own token carries.
    const LOCAL_SYSTEM: &str = "S-1-5-18";

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

    /// The namespace the parent holds, set in the child it launches.
    const SECOND_LAUNCH: &str = "TIDALUNAR_TEST_SECOND_LAUNCH";

    /// Another process: the one holding the namespace cannot open it again.
    #[test]
    fn a_second_launch_reaches_the_first_ones_lock() {
        if let Some(name) = std::env::var_os(SECOND_LAUNCH) {
            let name = name.to_string_lossy().into_owned();
            let ns = own_namespace(&name).expect("the second launch opens the namespace");
            let lock = in_namespace(&name, LOCK_LEAF);
            // SAFETY: NUL-terminated name, attributes that outlive the call.
            let mutex = unsafe { CreateMutexW(&ns.attributes(), FALSE, lock.as_ptr()) };
            // SAFETY: reads this thread's last error, set by the call above.
            let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
            assert!(
                !mutex.is_null() && existed,
                "the second launch did not reach the first one's lock"
            );
            return;
        }

        let name = test_namespace("reach");
        let first = own_namespace(&name).expect("create the namespace");
        let lock = in_namespace(&name, LOCK_LEAF);
        // SAFETY: NUL-terminated name, attributes that outlive the call.
        let mutex = unsafe { CreateMutexW(&first.attributes(), FALSE, lock.as_ptr()) };
        assert!(!mutex.is_null(), "the lock could not be created");

        relaunch(
            "a_second_launch_reaches_the_first_ones_lock",
            SECOND_LAUNCH,
            &name,
        );
        // SAFETY: the handle opened above, closed once.
        unsafe { CloseHandle(mutex) };
    }

    /// The legacy name the parent holds, set in the child it launches.
    const EARLIER_BUILD: &str = "TIDALUNAR_TEST_EARLIER_BUILD";

    /// The transition from `Global\` names: a new build keeps the legacy lock
    /// when it is free (an earlier build launched later then finds it) and yields
    /// to an earlier build of ours holding it. This process stands in for that
    /// earlier build; the new build is a child.
    #[test]
    fn the_legacy_lock_is_kept_when_free_and_yielded_to_an_earlier_build() {
        let (own, _) = own_identity().expect("our identity");
        if let Some(name) = std::env::var_os(EARLIER_BUILD) {
            let found = legacy_lock(&wide(&name.to_string_lossy()), &own);
            assert!(
                matches!(found, Legacy::EarlierBuild),
                "the earlier build holding the legacy lock was not recognised: {found:?}"
            );
            return;
        }

        let name = format!("Global\\TidaLunarAppLockTest-{}", std::process::id());
        let held = match legacy_lock(&wide(&name), &own) {
            Legacy::Held(handle) => handle,
            other => panic!("a free legacy lock was not kept: {other:?}"),
        };
        relaunch(
            "the_legacy_lock_is_kept_when_free_and_yielded_to_an_earlier_build",
            EARLIER_BUILD,
            &name,
        );
        // SAFETY: the handle `legacy_lock` returned, closed once.
        unsafe { CloseHandle(held) };
    }

    /// The app's and the installer's namespaces, named by their boundaries, share no object.
    #[test]
    fn two_boundary_names_are_two_namespaces() {
        let (a, b) = (test_namespace("a"), test_namespace("b"));
        let in_a = own_namespace(&a).expect("namespace a");
        let _in_b = own_namespace(&b).expect("namespace b");
        let lock_a = in_namespace(&a, LOCK_LEAF);
        let lock_b = in_namespace(&b, LOCK_LEAF);
        // SAFETY: NUL-terminated names, attributes that outlive the call.
        let mutex = unsafe { CreateMutexW(&in_a.attributes(), FALSE, lock_a.as_ptr()) };
        assert!(!mutex.is_null());
        // SAFETY: NUL-terminated name.
        let across = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, FALSE, lock_b.as_ptr()) };
        assert!(
            across.is_null(),
            "a lock created in one namespace appeared in the other"
        );
        // SAFETY: the handle opened above, closed once.
        unsafe { CloseHandle(mutex) };
    }

    /// What keeps another account out: the kernel refuses a boundary naming a SID our token lacks.
    #[test]
    fn a_boundary_naming_a_sid_we_lack_is_refused() {
        let mut system: PSID = std::ptr::null_mut();
        // SAFETY: NUL-terminated SID string; the SID is freed below.
        assert_ne!(
            unsafe { ConvertStringSidToSidW(wide(LOCAL_SYSTEM).as_ptr(), &mut system) },
            0
        );
        let refused = UserNamespace::open(&test_namespace("foreign"), system).err();
        assert_eq!(
            refused,
            Some(ERROR_ACCESS_DENIED),
            "a namespace bounded by a SID we lack was not refused by the kernel"
        );
        // SAFETY: allocated by ConvertStringSidToSidW, freed once.
        unsafe { LocalFree(system.cast()) };
    }

    fn test_pipe(tag: &str) -> Vec<u16> {
        wide(&format!(
            "\\\\.\\pipe\\TidaLunarAppLinkTest-{tag}-{}",
            std::process::id()
        ))
    }

    struct SendHandle(HANDLE);
    // SAFETY: a Win32 handle is process-wide; the server end moves to one thread and is used
    // only there.
    unsafe impl Send for SendHandle {}
    impl SendHandle {
        fn get(&self) -> HANDLE {
            self.0
        }
    }

    /// What the server can do with the client's identity after one message: the impersonation
    /// level it got, or the error that refused it a token.
    fn server_view_of_client(name: &[u16], server: HANDLE) -> Result<i32, u32> {
        let server = SendHandle(server);
        let listener = std::thread::spawn(move || {
            let server = server.get();
            // SAFETY: our own server end, connected, read once, then disconnected and closed.
            unsafe {
                ConnectNamedPipe(server, std::ptr::null_mut());
                let mut buf = [0u8; 16];
                let mut read = 0u32;
                ReadFile(
                    server,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                );
                let seen = if ImpersonateNamedPipeClient(server) == 0 {
                    Err(GetLastError())
                } else {
                    let mut token: HANDLE = std::ptr::null_mut();
                    let seen =
                        if OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) == 0 {
                            Err(GetLastError())
                        } else {
                            let mut level = 0i32;
                            let mut len = 0u32;
                            GetTokenInformation(
                                token,
                                TokenImpersonationLevel,
                                (&raw mut level).cast(),
                                size_of::<i32>() as u32,
                                &mut len,
                            );
                            CloseHandle(token);
                            Ok(level)
                        };
                    RevertToSelf();
                    seen
                };
                DisconnectNamedPipe(server);
                CloseHandle(server);
                seen
            }
        });
        // Bounded: a server end that never becomes openable fails the test instead of hanging it.
        let client = (0..500)
            .find_map(|_| {
                open_instance(name).or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    None
                })
            })
            .expect("the server end never became openable");
        // SAFETY: our own client end, written once then closed.
        unsafe {
            let mut written = 0u32;
            WriteFile(client, b"x".as_ptr(), 1, &mut written, std::ptr::null_mut());
        }
        let seen = listener.join().unwrap();
        // SAFETY: as above.
        unsafe { CloseHandle(client) };
        seen
    }

    #[test]
    fn our_own_link_pipe_is_ours_by_its_kernel_owner() {
        let (own, _) = own_identity().expect("our own token");
        let name = test_pipe("owner");
        let server = create_instance(&name).expect("a server end");
        let client = open_instance(&name).expect("a client end");
        assert_eq!(
            classify_owner(owner_sid(client).as_deref(), &own),
            ObjectOwner::Ours
        );
        // SAFETY: both ends opened above, closed once.
        unsafe {
            CloseHandle(client);
            CloseHandle(server);
        }
    }

    /// The server is left an anonymous token it cannot even open.
    #[test]
    fn the_link_client_lends_the_server_no_identity() {
        let name = test_pipe("sqos");
        let server = create_instance(&name).expect("a server end");
        assert_eq!(
            server_view_of_client(&name, server),
            Err(ERROR_CANT_OPEN_ANONYMOUS)
        );
    }

    /// No second account exists here. The OWN side is forged: our real pipe, judged by an
    /// identity that is not its owner, is a pipe another account holds.
    #[test]
    fn a_link_pipe_another_account_holds_is_refused_before_a_handle_escapes() {
        let stranger = OwnIdentity {
            user: OTHER.into(),
            default_owner: OTHER.into(),
        };
        let name = test_pipe("refuse");
        let server = create_instance(&name).expect("a server end");
        assert_eq!(
            connect_link_pipe(&name, &stranger).err(),
            Some(HandOff::ForeignServer)
        );
        // SAFETY: the server end opened above, closed once.
        unsafe { CloseHandle(server) };
    }
}
