// Chromium's own MediaSource, kept before bootstrap deletes it: the transcoding shim extends it.
// Bootstrap's first import, since imports evaluate ahead of its body and the `delete` in it.
export const RealMediaSource: typeof MediaSource | undefined = (globalThis as any).MediaSource;
