import { useEffect, useRef, useState } from 'preact/hooks';
import type { DebugStore } from '../store';

interface Props { store: DebugStore }

export const AUDIO_CHANNEL_EVENT = 'websrt:audio-select-channel';

const DB_FLOOR = -60;
const PEAK_HOLD_DB_PER_S = 12;

function toDb(linear: number): number {
  if (linear < 1e-12) return DB_FLOOR;
  return Math.max(DB_FLOOR, 20 * Math.log10(linear));
}

function lufsColor(l: number): string {
  if (l >= -23 && l <= -18) return 'stat-good';
  if (l >= -30 && l <= -10) return '';
  return 'stat-warn';
}

function dbColor(db: number): string {
  if (db > -3) return '#f66';
  if (db > -12) return '#fc6';
  return '#6f6';
}

export function AudioTab({ store }: Props) {
  const [, forceRender] = useState(0);
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), 50);
    return () => clearInterval(id);
  }, []);

  const scopeRef = useRef<HTMLCanvasElement>(null);
  const spectrumRef = useRef<HTMLCanvasElement>(null);
  const peakHoldRef = useRef<Float32Array>(new Float32Array(0));
  const peakTimeRef = useRef<number>(0);

  const meter = store.audioMeter.value;

  const totalChannels = meter ? meter.peaks.length : 0;

  const now = performance.now();
  const ph = peakHoldRef.current;
  if (ph.length < totalChannels) {
    peakHoldRef.current = new Float32Array(totalChannels).fill(DB_FLOOR);
  }
  const lastT = peakTimeRef.current || now;
  const dt = Math.min((now - lastT) / 1000, 0.25);
  peakTimeRef.current = now;
  const decay = PEAK_HOLD_DB_PER_S * dt;

  if (meter) {
    const holdArr = peakHoldRef.current;
    for (let i = 0; i < totalChannels && i < holdArr.length; i++) {
      const db = toDb(meter.peaks[i]);
      holdArr[i] = Math.max(db, holdArr[i] - decay);
    }
  }

  useEffect(() => {
    if (!meter) return;
    drawScope(scopeRef.current, meter.scopeL, meter.scopeR);
  });

  useEffect(() => {
    if (!meter) return;
    drawSpectrum(spectrumRef.current, meter.spectrum);
  });

  if (!meter || meter.pids.length === 0) {
    return (
      <div class="debug-section">
        <p style={{ color: '#999' }}>
          No PCM audio data. Connect to a SMPTE 302M stream to see audio meters.
        </p>
      </div>
    );
  }

  const pids = meter.pids;
  const channelCounts = meter.channelCounts;
  const chOffset: number[] = [0];
  for (let i = 0; i < channelCounts.length; i++) {
    chOffset.push(chOffset[i] + channelCounts[i]);
  }
  const totalCh = chOffset[chOffset.length - 1];
  const pairs = Math.floor(totalCh / 2);

  const onSelectChannel = (v: number) => {
    window.dispatchEvent(new CustomEvent(AUDIO_CHANNEL_EVENT, { detail: v }));
  };

  return (
    <>
      <div class="debug-section">
        <h3>Stream Overview</h3>
        <table class="debug-table">
          <tbody>
            <tr><td>PIDs</td><td>{pids.length}</td></tr>
            <tr><td>Total Channels</td><td>{totalCh}</td></tr>
            <tr><td>Sample Rate</td><td>48000 Hz</td></tr>
          </tbody>
        </table>
      </div>

      <div class="debug-section">
        <h3>Per-Channel Peak Meters ({totalCh}ch)</h3>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, 12px)', gap: '2px', maxHeight: '200px', overflowY: 'auto' }}>
          {Array.from({ length: totalCh }, (_, i) => {
            const peak = meter.peaks[i] || 0;
            const db = toDb(peak);
            const frac = Math.max(0, (db - DB_FLOOR) / (-DB_FLOOR));
            const holdDb = peakHoldRef.current[i] || DB_FLOOR;
            const holdFrac = Math.max(0, (holdDb - DB_FLOOR) / (-DB_FLOOR));
            const clip = meter.clips[i] > 0;
            return (
              <div style={{ position: 'relative', width: '8px', height: '120px', background: '#222', borderRadius: '1px' }}>
                <div style={{ position: 'absolute', bottom: 0, left: 0, right: 0, height: `${frac * 100}%`, background: dbColor(db), borderRadius: '1px' }} />
                <div style={{ position: 'absolute', bottom: `${holdFrac * 100}%`, left: 0, right: 0, height: '2px', background: '#fff' }} />
                {clip && <div style={{ position: 'absolute', top: 0, left: 0, right: 0, height: '4px', background: '#f00' }} />}
              </div>
            );
          })}
        </div>
      </div>

      <div class="debug-section">
        <h3>Loudness (LUFS) + RMS</h3>
        <table class="debug-table" style={{ fontSize: '11px' }}>
          <thead><tr><th>Ch</th><th>Peak</th><th>RMS</th><th>LUFS</th><th>Clips</th></tr></thead>
          <tbody>
            {Array.from({ length: Math.min(totalCh, 32) }, (_, i) => (
              <tr>
                <td>{i}</td>
                <td>{toDb(meter.peaks[i]).toFixed(1)}</td>
                <td>{toDb(meter.rms[i]).toFixed(1)}</td>
                <td class={lufsColor(meter.lufs[i])}>{meter.lufs[i].toFixed(1)}</td>
                <td class={meter.clips[i] > 0 ? 'stat-bad' : ''}>{meter.clips[i]}</td>
              </tr>
            ))}
          </tbody>
        </table>
        {totalCh > 32 && <p style={{ color: '#888', fontSize: '11px' }}>Showing first 32 of {totalCh} channels</p>}
      </div>

      <div class="debug-section">
        <h3>Phase Correlation ({pairs} pairs)</h3>
        {Array.from({ length: Math.min(pairs, 16) }, (_, i) => {
          const c = meter.phase[i] || 0;
          const pct = ((c + 1) / 2) * 100;
          const color = c > 0.5 ? '#6f6' : c > -0.5 ? '#fc6' : '#f66';
          return (
            <div style={{ display: 'flex', alignItems: 'center', gap: '8px', marginBottom: '3px' }}>
              <span style={{ width: '60px', fontSize: '11px', color: '#999' }}>Pair {i}</span>
              <div style={{ flex: 1, height: '12px', background: '#222', borderRadius: '2px', position: 'relative' }}>
                <div style={{ position: 'absolute', left: `${pct}%`, top: '-2px', width: '3px', height: '16px', background: color }} />
              </div>
              <span style={{ width: '40px', fontSize: '11px', textAlign: 'right', color }}>{c.toFixed(2)}</span>
            </div>
          );
        })}
      </div>

      <div class="debug-section">
        <h3>Vectorscope (PID {meter.selectedPid} Ch {meter.selectedChannel})</h3>
        <canvas ref={scopeRef} style={{ width: '200px', height: '200px', background: '#000', borderRadius: '3px' }} />
      </div>

      <div class="debug-section">
        <h3>Spectrum (PID {meter.selectedPid} Ch {meter.selectedChannel})</h3>
        <canvas ref={spectrumRef} style={{ width: '100%', height: '100px', background: '#000', borderRadius: '3px' }} />
      </div>

      <div class="debug-section">
        <h3>Channel Selector</h3>
        <select
          value={meter.selectedChannel}
          onChange={(e) => onSelectChannel(+((e.target as HTMLSelectElement).value))}
          style={{ background: '#222', color: '#ddd', border: '1px solid #444', padding: '4px' }}
        >
          {Array.from({ length: totalCh }, (_, i) => (
            <option value={i}>Channel {i}</option>
          ))}
        </select>
        <p style={{ color: '#888', fontSize: '11px', marginTop: '4px' }}>
          Selected: PID {meter.selectedPid}, Channel {meter.selectedChannel}
        </p>
      </div>
    </>
  );
}

function drawScope(canvas: HTMLCanvasElement | null, scopeL: number[], scopeR: number[]) {
  if (!canvas || !scopeL.length) return;
  const dpr = window.devicePixelRatio || 1;
  const w = 200;
  const h = 200;
  if (canvas.width !== w * dpr) {
    canvas.width = w * dpr;
    canvas.height = h * dpr;
  }
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

  ctx.fillStyle = 'rgba(0,0,0,0.15)';
  ctx.fillRect(0, 0, w, h);

  const cx = w / 2;
  const cy = h / 2;
  const scale = (w / 2) * 0.9;

  ctx.strokeStyle = '#6f6';
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = 0; i < scopeL.length; i++) {
    const l = scopeL[i] || 0;
    const r = (scopeR[i] || 0);
    const x = cx + (r - l) * scale;
    const y = cy - (l + r) * scale;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

function drawSpectrum(canvas: HTMLCanvasElement | null, spectrum: number[]) {
  if (!canvas || !spectrum.length) return;
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 300;
  const h = 100;
  if (canvas.width !== w * dpr) {
    canvas.width = w * dpr;
    canvas.height = h * dpr;
  }
  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, w, h);

  const bins = spectrum.length;
  const barW = w / bins;
  for (let i = 0; i < bins; i++) {
    const db = spectrum[i];
    const frac = Math.max(0, Math.min(1, (db + 80) / 80));
    const barH = frac * h;
    const hue = (i / bins) * 240;
    ctx.fillStyle = `hsl(${hue}, 80%, 50%)`;
    ctx.fillRect(i * barW, h - barH, barW - 1, barH);
  }
}
