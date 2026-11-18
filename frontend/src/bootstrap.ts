// Bootstrap: sets up globals that @luna/core and @luna/lib depend on.
// This module MUST be imported before any @luna/* module.
// ESM evaluates imports in order; this runs first.

// First: it keeps the real MediaSource, and imports evaluate ahead of the `delete` below.
import "./video/real-media-source";
import { setupIpcBridge } from "./plugins/ipc-bridge";
import { installDateTimeFormatMemo } from "./intl-memo";
import { armOnShakaBlind } from "./video/shaka-blind";
import { installTranscodingMediaSource } from "./video/transcode-shim";

// Set __ipcRenderer and __platform globals (used by plugins/lib/src/ipc.ts)
setupIpcBridge();

// Cache Intl.DateTimeFormat: TIDAL builds one per render tick, churning ICU
// handles and RAM. Installed here, before any @luna module and any plugin.
installDateTimeFormatMemo();

// Initialize window.luna object (window.core.ts adds getters to it)
(window as any).luna = {};

// No MSE for audio: Rust rewrites Shaka's MediaSource test to say no (`blind_shaka`). Should that
// rewrite go inert, Shaka still finds no MediaSource here.
delete (window as any).MediaSource;
delete (window as any).ManagedMediaSource;

// H.264 clips get a transcoding MediaSource, but only once Shaka can no longer see one.
armOnShakaBlind(installTranscodingMediaSource);

import "./audio-proxy";
