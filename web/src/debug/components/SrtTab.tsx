import { useState, useEffect } from 'preact/hooks';
import type { JSX } from 'preact';
import type { DebugStore } from '../store';
import { TimeSeriesChart } from './charts/TimeSeriesChart';
import { FrameTimeline } from './charts/FrameTimeline';
import { LossHeatmap } from './charts/LossHeatmap';
import { LossCorrelationChart } from './charts/LossCorrelationChart';

interface Props {
  store: DebugStore;
}

export function SrtTab({ store }: Props): JSX.Element {
  const [, forceRender] = useState(0);
  useEffect(() => {
    const id = setInterval(() => forceRender((n) => n + 1), 250);
    return () => clearInterval(id);
  }, []);

  const srt = store.srtStats.value;

  if (!srt) return <div>Not connected</div>;

  const total = srt.rxData + srt.rxLoss;
  const lossRate = total > 0 ? (srt.rxLoss / total) * 100 : 0;
  const lossClass =
    lossRate <= 1 ? 'stat-good' : lossRate <= 5 ? 'stat-warn' : 'stat-bad';

  const lastBucket = store.history.value[store.history.value.length - 1];
  const lastCcErrors = lastBucket?.ccErrors ?? 0;
  const lastSrtLoss = lastBucket?.srtLoss ?? 0;
  const lastSrtDropped = lastBucket?.srtDropped ?? 0;
  const orphanCc = lastCcErrors > 0 && lastSrtLoss === 0;

  const row = (label: string, value: string, cls?: string) => (
    <tr>
      <td>{label}</td>
      <td class={cls}>{value}</td>
    </tr>
  );

  return (
    <>
      <div class="debug-section">
        <h3>Connection Stats</h3>
        <table class="debug-table">
          <tbody>
            {row('Uptime', `${(srt.elapsedMs / 1000).toFixed(1)} s`)}
            {row('RTT', `${srt.rttMs.toFixed(1)} ms`)}
            {row('Bandwidth', `${(srt.bandwidthBps / 1_000_000).toFixed(1)} Mbps`)}
            {row('RX Data Packets', `${srt.rxData}`)}
            {row('RX Bytes', `${(srt.rxBytes / 1024 / 1024).toFixed(2)} MB`)}
            {row('Loss', `${srt.rxLoss} (${lossRate.toFixed(2)}%)`, lossClass)}
            {row('Retransmits', `${srt.rxRetransmit}`)}
            {row('Dropped', `${srt.rxDropped}`)}
            {row('Belated', `${srt.rxBelated}`)}
            {row('Buffered', `${srt.rxBuffered}`)}
            {row('ACK count', `${srt.rxAck}`)}
            {row('NAK count', `${srt.rxNak}`)}
            {row('Poll Max', `${srt.pollMaxMs.toFixed(1)} ms`)}
          </tbody>
        </table>
      </div>

      <div class="debug-section">
        <h3>Charts</h3>
        {store.history.value.length > 0 ? (
          <>
            <TimeSeriesChart
              store={store}
              field="rttMs"
              label="RTT (ms)"
              color="#6cf"
              height={100}
            />
            <TimeSeriesChart
              store={store}
              field="bandwidthMbps"
              label="Bandwidth (Mbps)"
              color="#6f6"
              height={100}
            />
            <TimeSeriesChart
              store={store}
              field="lossRate"
              label="Loss Rate (%)"
              color="#f66"
              height={100}
              transform={(v) => v * 100}
              yFormat={(v) => `${v.toFixed(2)}%`}
            />
            <TimeSeriesChart
              store={store}
              field="videoQueueDepth"
              label="Decode Queue Depth"
              color="#fc6"
              height={100}
            />
          </>
        ) : (
          <div id="srt-charts-area">No chart data yet</div>
        )}
      </div>

      <div class="debug-section">
        <h3>Visualizers</h3>
        <div style={{ marginBottom: '8px' }}>
          <div style={{ color: '#999', fontSize: '11px', marginBottom: '2px' }}>Render Health</div>
          <FrameTimeline store={store} height={80} />
        </div>
        <div>
          <div style={{ color: '#999', fontSize: '11px', marginBottom: '2px' }}>Packet Loss Heatmap</div>
          <LossHeatmap store={store} height={40} />
        </div>
        <div>
          <div style={{ color: '#999', fontSize: '11px', marginBottom: '2px' }}>Loss Correlation</div>
          <LossCorrelationChart store={store} height={80} />
          <div style={{ fontSize: '11px', marginTop: '4px' }}>
            CC errors (this period): {lastCcErrors} | SRT loss: {lastSrtLoss} | SRT dropped: {lastSrtDropped}
          </div>
          {orphanCc && (
            <div style={{ color: '#f66', fontSize: '11px', marginTop: '2px' }}>
              CC errors without SRT loss — investigate TS byte dropping
            </div>
          )}
        </div>
      </div>
    </>
  );
}
