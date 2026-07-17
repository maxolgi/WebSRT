import { useState, useEffect } from 'preact/hooks';
import type { JSX } from 'preact';
import type { DebugStore } from '../store';

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
  const demux = store.demuxStats.value;

  if (!srt) return <div>Not connected</div>;

  const total = srt.rxData + srt.rxLoss;
  const lossRate = total > 0 ? (srt.rxLoss / total) * 100 : 0;
  const lossClass =
    lossRate <= 1 ? 'stat-good' : lossRate <= 5 ? 'stat-warn' : 'stat-bad';

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
          </tbody>
        </table>
      </div>

      <div class="debug-section">
        <h3>Demuxer Stats</h3>
        <table class="debug-table">
          <tbody>
            {demux ? (
              <>
                {row('PAT', `${demux.pat}`)}
                {row('PMT', `${demux.pmt}`)}
                {row('PES', `${demux.pes}`)}
                {row('Random Access', `${demux.ra}`)}
                {row('Errors', `${demux.err}`, demux.err > 0 ? 'stat-bad' : 'stat-good')}
                {row('Raw events', `${demux.raw}`)}
              </>
            ) : (
              row('Status', 'No demux stats yet')
            )}
          </tbody>
        </table>
      </div>

      <div class="debug-section">
        <h3>Charts</h3>
        <div id="srt-charts-area">Charts will load here</div>
      </div>
    </>
  );
}
