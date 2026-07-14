// Type declarations for browser APIs that the TS DOM lib lags on.

// MediaStreamTrackGenerator (Insertable Streams / WebCodecs) — Chrome ships
// it; TS lib types don't yet.
interface MediaStreamTrackGenerator extends MediaStreamTrack {
  writable?: WritableStream<VideoFrame | AudioData>;
}
declare var MediaStreamTrackGenerator: {
  prototype: MediaStreamTrackGenerator;
  new (init: { kind: 'audio' | 'video' }): MediaStreamTrackGenerator;
};
