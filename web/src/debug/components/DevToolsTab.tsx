import { useState, useEffect } from 'preact/hooks';
import type { JSX } from 'preact';

const VERBOSE_KEY = 'websrt-debug';

export function DevToolsTab(): JSX.Element {
  const [verbose, setVerbose] = useState(false);

  useEffect(() => {
    setVerbose(localStorage.getItem(VERBOSE_KEY) === '1');
  }, []);

  const toggleVerbose = () => {
    const next = !verbose;
    setVerbose(next);
    if (next) localStorage.setItem(VERBOSE_KEY, '1');
    else localStorage.removeItem(VERBOSE_KEY);
  };

  const copyLink = (url: string) => (e: JSX.TargetedMouseEvent<HTMLAnchorElement>) => {
    e.preventDefault();
    navigator.clipboard?.writeText(url).catch(() => {});
  };

  const extSearch = (q: string) =>
    `https://chrome.google.com/webstore/search/${encodeURIComponent(q)}`;

  return (
    <>
      <div class="debug-section">
        <h3>Browser Internals</h3>
        <p>
          <a href="chrome://gpu/" onClick={copyLink('chrome://gpu/')}>chrome://gpu/</a>
          {' '}(copy &amp; paste in address bar) — GPU info + hardware acceleration status
        </p>
        <p>
          <a href="chrome://media-internals/" onClick={copyLink('chrome://media-internals/')}>chrome://media-internals/</a>
          {' '}(copy &amp; paste in address bar) — Media/decoder pipeline internals
        </p>
        <p>
          <a href="chrome://webrtc-internals/" onClick={copyLink('chrome://webrtc-internals/')}>chrome://webrtc-internals/</a>
          {' '}(copy &amp; paste in address bar) — WebRTC/media stats (useful patterns even for WT)
        </p>
        <p>
          <a href="edge://gpu/" onClick={copyLink('edge://gpu/')}>edge://gpu/</a>
          {' '}(copy &amp; paste in address bar) — Edge GPU info
        </p>
        <p>
          <a href="about:support">about:support</a> → Graphics section (Firefox)
        </p>
      </div>

      <div class="debug-section">
        <h3>Extensions</h3>
        <p>
          <a href={extSearch('WebGPU Inspector')} target="_blank" rel="noreferrer">WebGPU Inspector</a>
          {' '}— search Chrome Web Store
        </p>
        <p>
          <a href={extSearch('WebGPU DevTools')} target="_blank" rel="noreferrer">WebGPU DevTools</a>
          {' '}— search Chrome Web Store
        </p>
      </div>

      <div class="debug-section">
        <h3>Verbose Logging</h3>
        <label>
          <input type="checkbox" checked={verbose} onChange={toggleVerbose} />
          {' '}Enable verbose worker logging (reload required)
        </label>
      </div>

      <div class="debug-section">
        <h3>Quick Instructions</h3>
        <p>Chrome DevTools → More Tools → Media tab shows decoder pipelines</p>
        <p>Performance.measure() is used around key operations</p>
      </div>
    </>
  );
}
