// IPC payload types for the connect.* channels.
//
// These match the wire shapes produced by `src/connect/types/*.rs` on the
// Rust side. They are intentionally narrow: only the fields the frontend
// actually consumes are named. Anything not modeled here is either
// forwarded verbatim to Redux (as an untyped payload on a typed action)
// or ignored.

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

// ── IPC event payloads (Rust → frontend) ─────────────────────────────

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

export interface ReceiverMediaChangedPayload {
    mediaId?: string;
    metadata?: {
        duration?: number;
    };
}

export interface VolumeChangedPayload {
    volume: number;
}

export interface MuteChangedPayload {
    mute: boolean;
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
}

// ── Controller request payloads (frontend → Rust) ────────────────────

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
