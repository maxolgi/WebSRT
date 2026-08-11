import { useEffect, useRef, useState } from 'preact/hooks';
import type { JSX } from 'preact';
import type { DebugStore } from '../store';

interface Props {
  store: DebugStore;
}

// 20fps refresh for meters — same pattern as CodecTab/DemuxTab but faster
// because meters need to feel live.
const RENDER_TICK_MS = 50;
const SCOPE_SIZE = 200; // vectorscope canvas (logical px)
const SPEC_HEIGHT = 100; // spectrum canvas height (logical px)
const SPEC_BINS = 64; // spectrum bar count
const DB_FLOOR = -60; // peak-meter dBFS floor (bottom of bar)
const SPEC_FLOOR = -80; // spectrum dB floor (bottom of bar)
const PEAK_HOLD_DB_PER_S = 12; // peak-hold marker drop rate
const SPEC_PEAK_HOLD_DB_PER_FRAME = 0.5; // ~10 dB/s at 20fps

// Viewer-side wiring (owned by another agent) listens for this to drive the
// worklet's channel selection. The store has no setter for selectedChannel,
// so we emit a window event as the decoupled notification channel.
export const AUDIO_CHANNEL_EVENT = 'websrt:audio-select-channel';

const toDb = (lin: number): number => 20 * Math.log10((lin ?? 0) + 1e-12);
const clamp01 = (v: number): number => (v < 0 ? 0 : v > 1 ? 1 : v);

// green < -12, yellow [-12,-3], red > -3  (matches dBFS color spec)
const meterColor = (db: number): string =>
  db > -3 ? '#f33' : db >= -12 ? '#fc3' : '#3f3';

// green [-23,-18], yellow [-30,-23) or (-18,-10], red otherwise
const lufsClass = (l: number): string => {
  if (l >= -23 && l <= -18) return 'stat-good';
  if ((l >= -30 && l < -23) || (l > -18 && l <= -10)) return 'stat-warn';
  return 'stat-bad';
};

export function AudioTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0);
  const [selectedCh, setSelectedCh] = useState(0);

  const scopeRef = useRef<HTMLCanvasElement | null>(null);
  const specRef = useRef<HTMLCanvasElement | null>(null);

  // Peak-hold state (per-channel dB), persisted + decayed across renders.
  const peakHoldRef = useRef<Float32Array | null>(null);
  const peakTimeRef = useRef(0);
  // Spectrum peak-hold (per-bin dB).
  const specPeakRef = useRef<Float32Array | null>(null);
  // Tracks last vectorscope backing-store size so we only resize (which clears)
  // when the dimensions change — preserving the persistence/fade effect.
  const scopeSizeRef = useRef<{ w: number; h: number } | null>(null);

  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), RENDER_TICK_MS);
    return () => clearInterval(id);
  }, []);

  // --- Vectorscope draw (runs every render; no deps array, like LossHeatmap) ---
  useEffect(() => {
    const canvas = scopeRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const m = store.audioMeter.value;
    if (!m) return;

    const dpr = window.devicePixelRatio || 1;
    const w = SCOPE_SIZE;
    const h = SCOPE_SIZE;
    const prev = scopeSizeRef.current;
    if (!prev || prev.w !== w || prev.h !== h) {
      // Resize: resets context state, so re-apply scale + full clear.
      canvas.width = w * dpr;
      canvas.height = h * dpr;
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.scale(dpr, dpr);
      ctx.fillStyle = '#000';
      ctx.fillRect(0, 0, w, h);
      scopeSizeRef.current = { w, h };
    } else {
      // Persistence: fade old traces instead of fully clearing.
      ctx.fillStyle = 'rgba(0,0,0,0.15)';
      ctx.fillRect(0, 0, w, h);
    }

    const n = m.scopeL.length;
    if (n === 0) return;
    const cx = w / 2;
    const cy = h / 2;
    // (R-L) and (L+R) each span [-2,2]; scale to fit half-canvas.
    const scale = (Math.min(w, h) / 2) / 2;
    // Broadcast orientation: L → top-left, R → top-right, mono → vertical line.
    ctx.strokeStyle = '#3f3';
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let i = 0; i < n; i++) {
      const L = m.scopeL[i];
      const R = m.scopeR[i];
      const x = cx + (R - L) * scale;
      const y = cy - (L + R) * scale;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    }
    ctx.stroke();
  });

  // --- Spectrum draw (runs every render) ---
  useEffect(() => {
    const canvas = specRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const m = store.audioMeter.value;
    if (!m) return;

    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 400;
    const h = SPEC_HEIGHT;
    canvas.width = w * dpr;
    canvas.height = h * dpr;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.scale(dpr, dpr);
    ctx.fillStyle = '#111';
    ctx.fillRect(0, 0, w, h);

    let pk = specPeakRef.current;
    if (!pk || pk.length < SPEC_BINS) {
      pk = new Float32Array(SPEC_BINS).fill(SPEC_FLOOR);
      specPeakRef.current = pk;
    }
    const n = m.spectrum.length;
    const barW = w / SPEC_BINS;
    const drawW = Math.max(barW - 1, 1);
    for (let i = 0; i < SPEC_BINS; i++) {
      // spectrum values are already in dB per the worklet contract.
      const v = i < n ? m.spectrum[i] : SPEC_FLOOR;
      const frac = clamp01((v - SPEC_FLOOR) / (0 - SPEC_FLOOR));
      const barH = frac * h;
      // Blue (low freq) → red (high freq) via HSL hue 240..0.
      const hue = 240 * (1 - i / (SPEC_BINS - 1));
      ctx.fillStyle = `hsl(${hue}, 80%, 55%)`;
      ctx.fillRect(i * barW, h - barH, drawW, barH);

      if (v > pk[i]) pk[i] = v;
      else pk[i] -= SPEC_PEAK_HOLD_DB_PER_FRAME;
      const pkFrac = clamp01((pk[i] - SPEC_FLOOR) / (0 - SPEC_FLOOR));
      ctx.fillStyle = '#eee';
      ctx.fillRect(i * barW, h - pkFrac * h, drawW, 1);
    }
  });

  const meter = store.audioMeter.value;
  if (!meter) {
    return (
      <div class="debug-section">
        <p style={{ color: '#999' }}>
          No PCM audio data. Connect to a SMPTE 302M stream to see audio meters.
        </p>
      </div>
    );
  }

  const ch = meter.channelCount;
  const pairCount = Math.floor(ch / 2);

  // --- Update peak hold (decay + latch) before reading it for the meters. ---
  const now = performance.now();
  let ph = peakHoldRef.current;
  if (!ph || ph.length < ch) {
    ph = new Float32Array(ch).fill(DB_FLOOR);
    peakHoldRef.current = ph;
  }
  const lastT = peakTimeRef.current || now;
  const dt = Math.min((now - lastT) / 1000, 0.25); // clamp after tab inactivity
  peakTimeRef.current = now;
  const decay = PEAK_HOLD_DB_PER_S * dt;
  for (let i = 0; i < ch; i++) {
    const db = toDb(meter.peaks[i]);
    ph[i] = Math.max(db, ph[i] - decay);
  }
  // Capture narrowed non-null array as a const so closures keep the type.
  const peakHold: Float32Array = ph;

  const onSelectChannel = (v: number) => {
    setSelectedCh(v);
    // Decoupled notification for the viewer/worklet (wired by another agent).
    window.dispatchEvent(new CustomEvent(AUDIO_CHANNEL_EVENT, { detail: v }));
  };

  const bufFillPct = Math.round(clamp01(meter.bufferFill) * 100);

  return (
    <>
      {/* Section 1: Stream Overview */}
      <div class="debug-section">
        <h3>Stream Overview</h3>
        <table class="debug-table">
          <tbody>
            <tr><td>Channels</td><td>{ch}</td></tr>
            <tr><td>Sample Rate</td><td>{meter.sampleRate ? `${meter.sampleRate} Hz` : '—'}</td></tr>
            <tr><td>Buffer Fill</td><td class={bufFillPct > 90 ? 'stat-warn' : ''}>{bufFillPct}%</td></tr>
            <tr><td>Underruns</td><td class={meter.underruns > 0 ? 'stat-bad' : 'stat-good'}>{meter.underruns}</td></tr>
          </tbody>
        </table>
      </div>

      {/* Section 2: Per-Channel Peak Meters */}
      <div class="debug-section">
        <h3>Peak Meters (dBFS)</h3>
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: 'repeat(auto-fill, 12px)',
            gap: '2px',
            overflowY: 'auto',
            maxHeight: '200px',
            justifyItems: 'center',
          }}
        >
          {Array.from({ length: ch }, (_, i) => {
            const db = toDb(meter.peaks[i]);
            const frac = clamp01((db - DB_FLOOR) / (0 - DB_FLOOR));
            const phFrac = clamp01((peakHold[i] - DB_FLOOR) / (0 - DB_FLOOR));
            const clipped = (meter.clips[i] ?? 0) > 0;
            return (
              <div
                key={i}
                title={`ch ${i}: ${db.toFixed(1)} dBFS`}
                style={{ width: '8px', height: '120px', background: '#222', position: 'relative' }}
              >
                <div
                  style={{
                    position: 'absolute',
                    bottom: 0,
                    left: 0,
                    right: 0,
                    height: `${frac * 100}%`,
                    background: meterColor(db),
                  }}
                />
                <div
                  style={{
                    position: 'absolute',
                    left: 0,
                    right: 0,
                    bottom: `${phFrac * 100}%`,
                    height: '2px',
                    background: '#fff',
                  }}
                />
                {clipped && (
                  <div
                    style={{
                      position: 'absolute',
                      top: '1px',
                      left: '1px',
                      width: '4px',
                      height: '4px',
                      borderRadius: '50%',
                      background: '#f00',
                    }}
                  />
                )}
              </div>
            );
          })}
        </div>
        <div style={{ fontSize: '10px', color: '#888', marginTop: '2px' }}>
          <span style={{ color: '#3f3' }}>■</span> &lt; -12
          &nbsp; <span style={{ color: '#fc3' }}>■</span> -12..-3
          &nbsp; <span style={{ color: '#f33' }}>■</span> &gt; -3 dBFS
          &nbsp; • peak hold {PEAK_HOLD_DB_PER_S} dB/s
          &nbsp; • <span style={{ color: '#f00' }}>●</span> clip
        </div>
      </div>

      {/* Section 3: RMS / Loudness (LUFS) */}
      <div class="debug-section">
        <h3>RMS / Loudness (LUFS)</h3>
        <div style={{ maxHeight: '260px', overflowY: 'auto' }}>
          <table class="debug-table">
            <thead>
              <tr>
                <th style={{ textAlign: 'left', padding: '2px 6px' }}>Ch</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>Peak</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>RMS</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>LUFS</th>
                <th style={{ textAlign: 'right', padding: '2px 6px' }}>Clips</th>
              </tr>
            </thead>
            <tbody>
              {Array.from({ length: ch }, (_, i) => (
                <tr key={i}>
                  <td style={{ textAlign: 'left', padding: '2px 6px' }}>{i}</td>
                  <td style={{ textAlign: 'right', padding: '2px 6px' }}>{toDb(meter.peaks[i]).toFixed(1)}</td>
                  <td style={{ textAlign: 'right', padding: '2px 6px' }}>{toDb(meter.rms[i]).toFixed(1)}</td>
                  <td class={lufsClass(meter.lufs[i] ?? -Infinity)} style={{ textAlign: 'right', padding: '2px 6px' }}>
                    {(meter.lufs[i] ?? 0).toFixed(1)}
                  </td>
                  <td class={(meter.clips[i] ?? 0) > 0 ? 'stat-bad' : ''} style={{ textAlign: 'right', padding: '2px 6px' }}>
                    {meter.clips[i] ?? 0}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>

      {/* Section 4: Phase Correlation */}
      <div class="debug-section">
        <h3>Phase Correlation (stereo pairs)</h3>
        {pairCount === 0 ? (
          <div style={{ color: '#999' }}>No stereo pairs (need ≥ 2 channels).</div>
        ) : (
          Array.from({ length: pairCount }, (_, p) => {
            const c = meter.phase[p] ?? 0;
            return (
              <div key={p} style={{ display: 'flex', alignItems: 'center', gap: '6px', margin: '2px 0' }}>
                <span style={{ width: '120px', color: '#999', fontSize: '11px' }}>
                  Pair {p + 1} (ch {2 * p}-{2 * p + 1})
                </span>
                <div
                  style={{
                    position: 'relative',
                    flex: 1,
                    height: '12px',
                    background: 'linear-gradient(to right, #f66, #fc6 50%, #6f6)',
                    borderRadius: '2px',
                  }}
                >
                  <div
                    style={{
                      position: 'absolute',
                      left: `${clamp01((c + 1) / 2) * 100}%`,
                      top: '-2px',
                      bottom: '-2px',
                      width: '2px',
                      background: '#fff',
                      transform: 'translateX(-1px)',
                    }}
                  />
                </div>
                <span style={{ width: '40px', textAlign: 'right', fontSize: '11px', color: '#ccc' }}>
                  {c.toFixed(2)}
                </span>
              </div>
            );
          })
        )}
        <div style={{ fontSize: '10px', color: '#888', marginTop: '2px' }}>
          -1 anti-phase (red) · 0 mono (yellow) · +1 in-phase (green)
        </div>
      </div>

      {/* Sections 5 + 6 + 7: Vectorscope, Spectrum, and Channel Selector */}
      <div class="debug-section">
        <h3>
          Vectorscope &amp; Spectrum
          <span style={{ marginLeft: '10px', fontWeight: 'normal', textTransform: 'none', letterSpacing: 0 }}>
            Channel:
            <select
              value={selectedCh}
              onChange={(e) => onSelectChannel(parseInt((e.currentTarget as HTMLSelectElement).value, 10) || 0)}
              style={{ font: 'inherit', fontSize: '11px', marginLeft: '4px' }}
            >
              {Array.from({ length: ch }, (_, i) => (
                <option value={i}>{i}</option>
              ))}
            </select>
          </span>
        </h3>
        <div style={{ display: 'flex', gap: '10px', alignItems: 'flex-start', flexWrap: 'wrap' }}>
          <canvas
            ref={scopeRef}
            style={{ width: `${SCOPE_SIZE}px`, height: `${SCOPE_SIZE}px`, display: 'block', background: '#000' }}
          />
          <div style={{ fontSize: '10px', color: '#888', maxWidth: '160px' }}>
            <div>L → top-left · R → top-right</div>
            <div>Mono (in-phase) draws a <span style={{ color: '#3f3' }}>vertical</span> line; out-of-phase spreads horizontally.</div>
            <div style={{ marginTop: '6px', color: '#666' }}>
              Showing worklet channel {meter.selectedChannel}. Dropdown emits{' '}
              <code style={{ color: '#888' }}>{AUDIO_CHANNEL_EVENT}</code> for viewer-side wiring.
            </div>
          </div>
        </div>
        <div style={{ marginTop: '8px' }}>
          <canvas
            ref={specRef}
            style={{ width: '100%', height: `${SPEC_HEIGHT}px`, display: 'block' }}
          />
          <div style={{ fontSize: '10px', color: '#888', marginTop: '2px' }}>
            {SPEC_BINS} log-spaced bins · range {SPEC_FLOOR}..0 dB · blue=low / red=high freq · white = peak hold
          </div>
        </div>
      </div>
    </>
  );
}
