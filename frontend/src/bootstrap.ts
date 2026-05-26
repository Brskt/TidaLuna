// Bootstrap: sets up globals that @luna/core and @luna/lib depend on.
// This module MUST be imported before any @luna/* module.
// ESM evaluates imports in order, so this runs first.

import { setupIpcBridge } from "./plugins/ipc-bridge";
import { installDateTimeFormatMemo } from "./intl-memo";

// Set __ipcRenderer and __platform globals (used by plugins/lib/src/ipc.ts)
setupIpcBridge();

// Cache Intl.DateTimeFormat: TIDAL builds one per render tick, churning ICU
// handles and RAM. Installed here, before any TIDAL/@luna code runs.
installDateTimeFormatMemo();

// Initialize window.luna object (window.core.ts adds getters to it)
(window as any).luna = {};

// Force all audio through Rust player - prevents SDK boombox/shakaPlayer for DASH/AAC.
delete (window as any).MediaSource;
delete (window as any).ManagedMediaSource;

import "./audio-proxy";
