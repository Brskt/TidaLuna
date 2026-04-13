// Fragment 5/5 - Minimal nativeInterface stub + session delegate
// Depends on: sendIpc, _cfg (from ipc.js)

if (!window.nativeInterface) {
    var creds = window.__TIDALUNAR_CREDENTIALS__ || {
        credentialsStorageKey: 'tidal',
        codeChallenge: '',
        redirectUri: _cfg.redirectUri || 'tidal://login/auth',
        codeVerifier: ''
    };
    var noop = function() {};
    var platform = window.__TIDALUNAR_PLATFORM__ || 'win32';
    self.__LUNAR_SESSION_DELEGATE__ = self.__LUNAR_SESSION_DELEGATE__ || null;
    window.nativeInterface = {
        application: {
            applyUpdate: noop,
            checkForUpdatesSilently: noop,
            getDesktopReleaseNotes: function() { return '{}'; },
            getPlatform: function() { return platform; },
            getPlatformTarget: function() { return 'standalone'; },
            getProcessUptime: function() { return 100; },
            getVersion: function() { return '2.38.6.6'; },
            getWindowsVersionNumber: function() { return Promise.resolve('10.0.0'); },
            ready: noop,
            reenableAutoUpdater: noop,
            registerDelegate: noop,
            reload: function() { window.location.reload(); },
            setWebVersion: noop,
        },
        audioHack: { registerDelegate: noop },
        chromecast: undefined,
        credentials: creds,
        features: { chromecast: false, tidalConnect: true, remoteDesktop: true },
        navigation: { registerDelegate: noop },
        playback: {
            registerDelegate: noop,
            setCurrentMediaItem: noop,
            pause: noop,
            play: noop,
            seek: noop,
            setVolume: noop,
        },
        remoteDesktop: {
            initialize: noop,
            startBroadcasting: noop,
            stopBroadcasting: noop,
            statusUpdated: noop,
            progressUpdated: noop,
            playbackCompleted: noop,
            requestNextMedia: noop,
            disconnect: noop,
            setRepeatMode: noop,
            setShuffle: noop,
        },
        tidalConnect: {
            initialize: noop,
            discoverDevices: noop,
            refreshDevices: noop,
            connectToDevice: noop,
            disconnect: noop,
            setAuthentication: noop,
            loadMedia: noop,
            loadQueue: noop,
            playNext: noop,
            playPrevious: noop,
            playOrPause: noop,
            seek: noop,
            selectQueueItem: noop,
            refreshQueue: noop,
            setVolume: noop,
            setMute: noop,
            setRepeatMode: noop,
            setShuffle: noop,
            updateQuality: noop,
        },
        userSession: {
            clear: function() {
                if (window.location.pathname === (_cfg.loginCallbackPath || '/login/auth')) return;
                sendIpc('jsrt.session_clear');
            },
            registerDelegate: function(d) { self.__LUNAR_SESSION_DELEGATE__ = d; },
            update: function(s) {
                var d = self.__LUNAR_SESSION_DELEGATE__;
                if (d && typeof d.onSessionChanged === 'function') {
                    d.onSessionChanged(s);
                }
            },
        },
        userSettings: { registerDelegate: noop },
        window: {
            registerDelegate: noop,
            minimize: noop,
            maximize: noop,
            unmaximize: noop,
            close: noop,
            setFullscreen: noop,
        },
    };

    self.__LUNAR_SESSION_CLEAR_DONE__ = false;
    self.__LUNAR_IPC_ON__('jsrt.session_cleared', function() {
        self.__LUNAR_SESSION_CLEAR_DONE__ = true;
        var d = self.__LUNAR_SESSION_DELEGATE__;
        if (d && typeof d.onSessionChanged === 'function') {
            d.onSessionChanged(null);
        }
    });
}
