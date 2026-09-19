import type { JSX } from 'preact';
import type { DebugStore } from '../store';
import { writeHash } from '../hash';
import { computeIssues, type Issue } from '../issues';

interface Props {
  store: DebugStore;
}

export function IssuesStrip({ store }: Props): JSX.Element | null {
  const issues = computeIssues(store);

  if (issues.length === 0) {
    return (
      <div class="debug-section" style={{ display: 'flex', alignItems: 'center', gap: '6px' }}>
        <span class="stat-good" style={{ fontSize: '11px' }}>All clear</span>
      </div>
    );
  }

  const navigate = (issue: Issue) => {
    store.activeTab.value = issue.tab;
    if (issue.demuxSub) store.demuxSubTab.value = issue.demuxSub;
    writeHash(issue.tab, issue.demuxSub ?? undefined);
  };

  return (
    <div class="debug-section" style={{ display: 'flex', flexWrap: 'wrap', gap: '4px' }}>
      {issues.map((issue) => {
        const error = issue.severity === 'error';
        return (
          <button
            key={issue.id}
            class={error ? 'stat-bad' : 'stat-warn'}
            onClick={() => navigate(issue)}
            style={{
              fontSize: '11px',
              lineHeight: '16px',
              padding: '0 8px',
              borderRadius: '9px',
              border: 'none',
              background: error ? 'rgba(255,102,102,0.15)' : 'rgba(255,204,102,0.12)',
              cursor: 'pointer',
              whiteSpace: 'nowrap',
            }}
          >
            {issue.label}
          </button>
        );
      })}
    </div>
  );
}
