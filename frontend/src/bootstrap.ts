// Bootstrap: sets up globals that @luna/core and @luna/lib depend on.
// This module MUST be imported before any @luna/* module.
// ESM evaluates imports in order, so this runs first.

import { setupIpcBridge } from "./plugins/ipc-bridge";

// Set __ipcRenderer and __platform globals (used by luna-lib/ipc.ts)
setupIpcBridge();

// Initialize window.luna object (window.core.ts adds getters to it)
(window as any).luna = {};
