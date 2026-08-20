import { useEffect, useRef, useState } from 'preact/hooks';
import type { JSX } from 'preact';
import type { DebugStore } from '../store';

interface Props {
  store: DebugStore;
}

const RENDER_TICK_MS = 50;
const SCOPE_SIZE = 200;
const SPEC_HEIGHT = 100;
const SPEC_BINS = 64;
const DB_FLOOR = -60;
const SPEC_FLOOR = -80;
const PEAK_HOLD_DB_PER_S = 12;
const SPEC_PEAK_HOLD_DB_PER_FRAME = 0.5;

export const AUDIO_CHANNEL_EVENT = 'websrt:audio-select-channel';

const toDb = (lin: number): number => 20 * Math.log10((lin ?? 0) + 1e-12);
const clamp01 = (v: number): number => (v < 0 ? 0 : v > 1 ? 1 : v);

const meterColor = (db: number): string =>
  db > -3 ? '#f33' : db >= -12 ? '#fc3' : '#3f3';

const lufsClass = (l: number): string => {
  if (l >= -23 && l <= -18) return 'stat-good';
  if ((l >= -30 && l < -23) || (l > -18 && l <= -10)) return 'stat-warn';
  return 'stat-bad';
};

type SubTab = 'overview' | 'peak' | 'lufs' | 'phase' | 'scope' | 'spectrum' | 'pacing';

export function AudioTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0);
  const [selectedCh, setSelectedCh] = useState(0);
  const [subTab, setSubTab] = useState<SubTab>('overview');

  const scopeRef = useRef<HTMLCanvasElement | null>(null);
  const specRef = useRef<HTMLCanvasElement | null>(null);

  const peakHoldRef = useRef<Float32Array | null>(null);
  const peakTimeRef = useRef(0);
  const specPeakRef = useRef<Float32Array | null>(null);
  const scopeSizeRef = useRef<{ w: number; h: number } | null>(null);
  // Smoothed vectorscope auto-gain: program audio sits well below 0 dBFS, so a
  // fixed ±1.0 scale renders normal levels as a tiny dot. Track recent ring
  // amplitude and normalize it to ~85% of the canvas radius (clamped 1..60x,
  // EMA-smoothed so transients don't pump the display).
  const scopeGainRef = useRef(1);

  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), RENDER_TICK_MS);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    if (subTab !== 'scope') return;
    const canvas = scopeRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    const m = store.audioMeter.value;
    if (!m) return;

    const dpr = window.devicePixelRatio || 1;
    const w = SCOPE_SIZE;
    const h = SCOPE_SIZE;
    // Validate the canvas itself, not just the remembered logical size: the
    // <canvas> is conditionally rendered, so switching subtabs away and back
    // mounts a FRESH element (default 300x150 store, identity transform) while
    // this ref survives. Skipping the resize would draw unscaled and leave a
    // never-faded strip beyond x=200 where traces accumulate forever.
    const prev = scopeSizeRef.current;
    if (
      !prev || prev.w !== w || prev.h !== h ||
      canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)
    ) {
      canvas.width = w * dpr;
      canvas.height = h * dpr;
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.scale(dpr, dpr);
      ctx.fillStyle = '#000';
      ctx.fillRect(0, 0, w, h);
      scopeSizeRef.current = { w, h };
    } else {
      ctx.fillStyle = 'rgba(0,0,0,0.15)';
      ctx.fillRect(0, 0, w, h);
    }

    const n = m.scopeL.length;
    if (n === 0) return;
    const cx = w / 2;
    const cy = h / 2;
    let peak = 0;
    for (let i = 0; i < n; i++) {
      const a = Math.abs(m.scopeL[i]);
      const b = Math.abs(m.scopeR[i]);
      if (a > peak) peak = a;
      if (b > peak) peak = b;
    }
    const target = Math.min(60, Math.max(1, 0.85 / (peak + 1e-6)));
    scopeGainRef.current = scopeGainRef.current * 0.85 + target * 0.15;
    const scale = ((Math.min(w, h) / 2) / 2) * scopeGainRef.current;
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

  useEffect(() => {
    if (subTab !== 'spectrum') return;
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
      const v = i < n ? m.spectrum[i] : SPEC_FLOOR;
      const frac = clamp01((v - SPEC_FLOOR) / (0 - SPEC_FLOOR));
      const barH = frac * h;
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
  if (!meter || !meter.pids?.length) {
    return (
      <div class="debug-section">
        <p style={{ color: '#999' }}>
          No PCM audio data. Connect to a SMPTE 302M stream to see audio meters.
        </p>
      </div>
    );
  }

  const ch = meter.peaks.length;
  const pairCount = Math.floor(ch / 2);

  const now = performance.now();
  let ph = peakHoldRef.current;
  if (!ph || ph.length < ch) {
    ph = new Float32Array(ch).fill(DB_FLOOR);
    peakHoldRef.current = ph;
  }
  const lastT = peakTimeRef.current || now;
  const dt = Math.min((now - lastT) / 1000, 0.25);
  peakTimeRef.current = now;
  const decay = PEAK_HOLD_DB_PER_S * dt;
  for (let i = 0; i < ch; i++) {
    const db = toDb(meter.peaks[i]);
    ph[i] = Math.max(db, ph[i] - decay);
  }
  const peakHold: Float32Array = ph;

  const onSelectChannel = (v: number) => {
    setSelectedCh(v);
    window.dispatchEvent(new CustomEvent(AUDIO_CHANNEL_EVENT, { detail: v }));
  };

  const SUB_TABS: Array<{ id: SubTab; label: string }> = [
    { id: 'overview', label: 'Overview' },
    { id: 'peak', label: 'Peak' },
    { id: 'lufs', label: 'LUFS' },
    { id: 'phase', label: 'Phase' },
    { id: 'scope', label: 'Scope' },
    { id: 'spectrum', label: 'Spectrum' },
    { id: 'pacing', label: 'Pacing' },
  ];

  const channelSelector = (
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
  );

  return (
    <>
      <div
        class="debug-tabs"
        style={{ position: 'sticky', top: '-8px', background: '#1a1a1a', zIndex: 2, margin: '-8px -8px 8px -8px' }}
      >
        {SUB_TABS.map((t) => (
          <button
            class={`debug-tab ${subTab === t.id ? 'active' : ''}`}
            onClick={() => setSubTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>

      {subTab === 'overview' && (
        <div class="debug-section">
          <h3>Stream Overview</h3>
          <table class="debug-table">
            <tbody>
              <tr><td>PIDs</td><td>{meter.pids.length}</td></tr>
              <tr><td>Total Channels</td><td>{ch}</td></tr>
              <tr><td>Sample Rate</td><td>48000 Hz</td></tr>
              <tr><td>Selected PID</td><td>{meter.selectedPid}</td></tr>
              <tr><td>Selected Channel</td><td>{meter.selectedChannel}</td></tr>
            </tbody>
          </table>
        </div>
      )}

      {subTab === 'peak' && (
        <div class="debug-section">
          <h3>Peak Meters (dBFS)</h3>
          <div
            style={{
              display: 'grid',
              gridTemplateColumns: 'repeat(auto-fill, 12px)',
              gap: '2px',
              overflowY: 'auto',
              maxHeight: '400px',
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
          <div style={{ fontSize: '10px', color: '#888', marginTop: '4px' }}>
            <span style={{ color: '#3f3' }}>■</span> &lt; -12
            &nbsp; <span style={{ color: '#fc3' }}>■</span> -12..-3
            &nbsp; <span style={{ color: '#f33' }}>■</span> &gt; -3 dBFS
            &nbsp; • peak hold {PEAK_HOLD_DB_PER_S} dB/s
            &nbsp; • <span style={{ color: '#f00' }}>●</span> clip
          </div>
        </div>
      )}

      {subTab === 'lufs' && (
        <div class="debug-section">
          <h3>RMS / Loudness (LUFS)</h3>
          <div style={{ maxHeight: '500px', overflowY: 'auto' }}>
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
      )}

      {subTab === 'phase' && (
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
          <div style={{ fontSize: '10px', color: '#888', marginTop: '4px' }}>
            -1 anti-phase (red) · 0 mono (yellow) · +1 in-phase (green)
          </div>
        </div>
      )}

      {subTab === 'scope' && (
        <div class="debug-section">
          <h3>
            Vectorscope
            {channelSelector}
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
                Showing PID {meter.selectedPid} channel {meter.selectedChannel}.
                Auto-gain ×{scopeGainRef.current.toFixed(1)} (85% radius, smoothed).
              </div>
            </div>
          </div>
        </div>
      )}

      {subTab === 'spectrum' && (
        <div class="debug-section">
          <h3>
            Spectrum
            {channelSelector}
          </h3>
          <canvas
            ref={specRef}
            style={{ width: '100%', height: `${SPEC_HEIGHT}px`, display: 'block' }}
          />
          <div style={{ fontSize: '10px', color: '#888', marginTop: '2px' }}>
            {SPEC_BINS} log-spaced bins · range {SPEC_FLOOR}..0 dB · blue=low / red=high freq · white = peak hold
          </div>
        </div>
      )}
      {subTab === 'pacing' && (
        <div class="debug-section">
          <h3>PCM Release Pacing (per stats window)</h3>
          {(() => {
            const pcm = store.srtStats.value?.pcmRelease;
            if (!pcm || pcm.length === 0) {
              return <div style={{ color: '#999' }}>No PCM release samples in this window yet.</div>;
            }
            const errCls = (us: number) => (us <= 1000 ? 'stat-good' : us <= 2000 ? 'stat-warn' : 'stat-bad');
            const gapCls = (us: number) => (us <= 13800 ? 'stat-good' : us <= 20000 ? 'stat-warn' : 'stat-bad');
            return (
              <table class="debug-table">
                <thead>
                  <tr>
                    <th style={{ textAlign: 'left', padding: '2px 6px' }}>PID</th>
                    <th style={{ textAlign: 'right', padding: '2px 6px' }}>Count</th>
                    <th style={{ textAlign: 'right', padding: '2px 6px' }}>Mean |rel−sched|</th>
                    <th style={{ textAlign: 'right', padding: '2px 6px' }}>Max |rel−sched|</th>
                    <th style={{ textAlign: 'right', padding: '2px 6px' }}>Max gap</th>
                  </tr>
                </thead>
                <tbody>
                  {pcm.map((p) => (
                    <tr key={p.pid}>
                      <td style={{ textAlign: 'left', padding: '2px 6px' }}>{p.pid}</td>
                      <td style={{ textAlign: 'right', padding: '2px 6px' }}>{p.count}</td>
                      <td class={errCls(p.meanErrUs)} style={{ textAlign: 'right', padding: '2px 6px' }}>
                        {(p.meanErrUs / 1000).toFixed(2)} ms
                      </td>
                      <td class={errCls(p.maxErrUs)} style={{ textAlign: 'right', padding: '2px 6px' }}>
                        {(p.maxErrUs / 1000).toFixed(2)} ms
                      </td>
                      <td class={gapCls(p.maxGapUs)} style={{ textAlign: 'right', padding: '2px 6px' }}>
                        {(p.maxGapUs / 1000).toFixed(2)} ms
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            );
          })()}
          <div style={{ fontSize: '10px', color: '#888', marginTop: '4px' }}>
            Release error vs TSBPD deadline (schedUs) and inter-pcm release gaps, aggregated per PID over the
            stats window by the worker. Gates (local): mean &lt; 1 ms · gap max ≤ 20 ms · p99 ≤ 2× nominal
            interval (percentiles via the worker's VERBOSE logger). Message fields remain the source of truth.
          </div>
        </div>
      )}
    </>
  );
}
