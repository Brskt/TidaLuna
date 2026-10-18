// IPC payload types for the connect.* channels.
//
// These match the wire shapes produced by `src/connect/types/*.rs` on the
// Rust side. They are intentionally narrow: only the fields the frontend
// actually consumes are named. Anything not modeled here is either
// forwarded verbatim to Redux (as an untyped payload on a typed action)
// or ignored.

import type { OutputLevelReport } from "../volume";

export type PlayerState = "IDLE" | "BUFFERING" | "PAUSED" | "PLAYING";

export type RepeatMode = "NONE" | "ONE" | "ALL";

export type DeviceType = "tidalConnect";

export interface MdnsDevice {
    addresses: string[];
    friendlyName: string;
    fullname: string;
    id: string;
    port: number;
    type: DeviceType;
}

export interface MediaMetadata {
    title?: string;
    albumTitle?: string;
    artists?: unknown;
    duration?: number;
    images?: unknown;
}

export interface MediaInfo {
    itemId: string;
    mediaId: string;
    srcUrl?: string;
    streamType?: string;
    metadata?: MediaMetadata;
    customData?: unknown;
    mediaType: number;
    policy: unknown;
}

export interface QueueInfo {
    queueId: string;
    repeatMode: RepeatMode;
    shuffled: boolean;
    maxAfterSize: number;
    maxBeforeSize: number;
}

export interface DevicesReceivedPayload {
    devices: MdnsDevice[];
}

export interface SessionStartedPayload {
    connectedDevice: MdnsDevice;
    sessionId: string;
    joined: boolean;
}

export interface SessionEndedPayload {
    suspended?: boolean;
}

export interface MediaChangedPayload {
    mediaInfo?: MediaInfo;
}

export interface PlayerStatusChangedPayload {
    playerState: PlayerState;
    progress: number;
}

export interface QueueChangedPayload {
    queueInfo?: QueueInfo;
}

export interface ServerErrorPayload {
    errorCode: number;
    details?: unknown;
}

// The format of the stream the receiver resolved, from TIDAL's own `playbackinfo` answer. A field
// the answer omitted is null; the whole object is null when no stream was resolved.
export interface ReceiverStream {
    audioQuality?: string | null;
    bitDepth?: number | null;
    sampleRate?: number | null;
}

export interface ReceiverMediaChangedPayload {
    mediaId?: string;
    metadata?: {
        duration?: number;
    };
    stream?: ReceiverStream | null;
}

export interface ConnectStateSnapshot {
    devices?: MdnsDevice[];
    session?: {
        sessionId: string;
        device: MdnsDevice;
        joined?: boolean;
    };
    media?: MediaInfo;
    playerStatus?: PlayerStatusChangedPayload;
    isConnected?: boolean;
    receiverActive?: boolean;
    /** The level Rust plays, which `connect.receiver.volume_changed`/`mute_changed` also carry. */
    outputLevel?: OutputLevelReport;
}

export interface QueueLoadRequestData {
    audioquality?: string;
    autoplay?: boolean;
    position?: number;
    currentMediaInfo?: MediaInfo;
    queueId?: string;
    repeatMode?: RepeatMode;
    shuffled?: boolean;
    maxAfterSize?: number;
    maxBeforeSize?: number;
}
