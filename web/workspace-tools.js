/* Local navigation memory and bounded, read-only checks against retained notices. */
const workspaceStorageKey = `${exampleWorkspace.storagePrefix}workspace.v1`;
let workspaceRestore = null;
let workspaceRestoreSubmit = false;
let workspaceRestoring = false;
let workspaceScrollTimer = null;
let shortlistCheckGeneration = 0;
let shortlistChecking = false;
const shortlistChecks = new Map();
const knownNoticeRevisions = new Map();

function validWorkspaceState(value) {
  if (!value || value.version !== 1 || typeof value.company !== 'string'
      || !(exampleWorkspace.active ? value.company === exampleWorkspace.companyNumber : /^(?:BE)?\d{4}\.?\d{3}\.?\d{3}$/i.test(value.company))
      || !['score', 'deadline'].includes(value.sort) || !['', '7', '30', '90'].includes(value.dueWithin)
      || !['opportunities', 'shortlist', 'profile'].includes(value.view)) return null;
  const selection = value.selection && typeof value.selection.source === 'string' && value.selection.source.length > 0 && value.selection.source.length <= 32
    && typeof value.selection.external_id === 'string' && value.selection.external_id.length > 0 && value.selection.external_id.length <= 512
    ? {source: value.selection.source, external_id: value.selection.external_id} : null;
  const scroll = n => typeof n === 'number' && Number.isFinite(n) && n >= 0 && n <= 1000000 ? n : 0;
  // Whitelist fields: tokens, note drafts, raw notices and analysis never enter navigation memory.
  return {version: 1, company: value.company, sort: value.sort, dueWithin: value.dueWithin, view: value.view,
    selection, scrollY: scroll(value.scrollY), detailScroll: scroll(value.detailScroll)};
}

function rememberWorkspace() {
  if (workspaceRestoring || workspaceRestore || !activeEnterpriseNumber || dashboardFlow.status !== 'ready') return;
  const selection = selectedCard ? {source: selectedCard.dataset.source, external_id: selectedCard.dataset.id} : null;
  const state = validWorkspaceState({version: 1, company: activeEnterpriseNumber, sort: activeSort,
    dueWithin: activeDueWithin, view: workspaceView, selection, scrollY: window.scrollY || 0,
    detailScroll: selectedCard ? workspaceQuery('#selected-panel').scrollTop : 0});
  if (!state) return;
  try { localStorage.setItem(workspaceStorageKey, JSON.stringify(state)); } catch { /* Navigation remains usable without storage. */ }
}

function resumeWorkspace() {
  let saved;
  try {
    const text = localStorage.getItem(workspaceStorageKey);
    if (!text || text.length > 4096) return;
    saved = validWorkspaceState(JSON.parse(text));
  } catch { return; }
  if (!saved) return;
  workspaceRestore = saved;
  workspaceQuery('#enterprise-number').value = saved.company;
  workspaceQuery('#sort-order').value = saved.sort;
  workspaceQuery('#deadline-window').value = saved.dueWithin;
  workspaceRestoreSubmit = true;
  try { form.requestSubmit(); } finally { workspaceRestoreSubmit = false; }
}

function workspaceSearchStarted(sequence) {
  if (workspaceRestoreSubmit && workspaceRestore) workspaceRestore.sequence = sequence;
  else workspaceRestore = null;
  shortlistCheckGeneration++;
  shortlistChecking = false;
  shortlistChecks.clear();
  knownNoticeRevisions.clear();
}

function workspaceSearchReady() {
  const saved = workspaceRestore;
  workspaceRestore = null;
  if (saved && saved.sequence === searchSequence && saved.company.replace(/\D/g, '') === activeEnterpriseNumber.replace(/\D/g, '')) {
    workspaceRestoring = true;
    showWorkspaceView(saved.view, false);
    if (saved.view === 'opportunities' && saved.selection) {
      const card = [...document.querySelectorAll('article.card')].find(card =>
        card.dataset.source === saved.selection.source && card.dataset.id === saved.selection.external_id);
      // A filtered-out or superseded record is not replaced by a guessed match.
      if (card) {
        const row = card.parentElement;
        selectTender(card, row, row.querySelector('.result-select'), false);
      }
    }
    const sequence = searchSequence;
    const view = workspaceView;
    requestAnimationFrame(() => {
      if (sequence === searchSequence && view === workspaceView) {
        window.scrollTo(0, saved.scrollY);
        if (selectedCard) workspaceQuery('#selected-panel').scrollTop = saved.detailScroll;
      }
      workspaceRestoring = false;
      rememberWorkspace();
    });
  } else rememberWorkspace();
}

function cancelWorkspaceRestore() { workspaceRestore = null; workspaceRestoring = false; }

function shortcutContext(event) {
  if (event.defaultPrevented || event.isComposing || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return false;
  if (event.target?.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"]), [role="textbox"]')) return false;
  return !workspaceQuery('#filter-dialog').open && workspaceView === 'opportunities';
}

function handleWorkspaceShortcut(event) {
  if (!shortcutContext(event)) return;
  const rows = [...document.querySelectorAll('.result-select')];
  const trigger = event.target?.closest('.result-select');
  if (trigger && ['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) {
    const current = rows.indexOf(trigger);
    if (current < 0 || !rows.length) return;
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? rows.length - 1
      : Math.max(0, Math.min(rows.length - 1, current + (event.key === 'ArrowDown' ? 1 : -1)));
    event.preventDefault();
    rows[next].focus();
    rows[next].scrollIntoView({block: 'nearest'});
    return;
  }
  if (event.key.toLowerCase() !== 's' || event.repeat) return;
  // Single-letter shortcuts work only inside the focused result/detail component.
  const card = trigger ? trigger.closest('.result-item')?.querySelector('.card')
    || (trigger === selectedTrigger ? selectedCard : null)
    : selectedCard && workspaceQuery('#selected-panel').contains(event.target) ? selectedCard : null;
  if (!card) return;
  event.preventDefault();
  card.querySelector('[data-shortlist-toggle]').click();
}

// Compare money as canonical decimal strings, never with binary-float subtraction.
function comparableAmount(value) {
  if (value === null) return null;
  if (!['string', 'number'].includes(typeof value) || String(value).length > 128) return undefined;
  const match = /^([+-]?)(\d+)(?:\.(\d*))?(?:[eE]([+-]?\d{1,3}))?$/.exec(String(value));
  if (!match) return undefined;
  const exponent = Number(match[4] || 0);
  if (Math.abs(exponent) > 100) return undefined;
  let digits = (match[2] + (match[3] || '')).replace(/^0+/, '');
  if (!digits) return '0';
  let scale = (match[3] || '').length - exponent;
  while (digits.endsWith('0')) { digits = digits.slice(0, -1); scale--; }
  return `${match[1] === '-' ? '-' : ''}${digits}e${-scale}`;
}

function validNoticeDate(value) {
  if (typeof value !== 'string' || !/^\d{4}-\d{2}-\d{2}$/.test(value)) return null;
  const date = new Date(`${value}T12:00:00Z`);
  return Number.isFinite(date.getTime()) && date.toISOString().slice(0, 10) === value ? value : null;
}
function brusselsToday(now = new Date()) {
  const parts = new Intl.DateTimeFormat('en-GB', {timeZone: 'Europe/Brussels', year: 'numeric', month: '2-digit', day: '2-digit'}).formatToParts(now);
  return ['year', 'month', 'day'].map(type => parts.find(part => part.type === type).value).join('-');
}
function noticeRevision(detail) {
  const result = {};
  for (const key of ['notice_version', 'bosa_notice_id']) {
    const value = detail?.raw?.[key];
    if (typeof value === 'string' && value.trim() && value.length <= 128) result[key] = value;
  }
  return Object.keys(result).length ? result : null;
}
function validSavedRevision(value) {
  return value && typeof value === 'object' ? noticeRevision({raw: value}) : null;
}
function snapshotCheckKey(item) {
  return JSON.stringify([shortlistEnterpriseNumber, item.source, item.external_id, item.title, item.buyer,
    item.deadline, comparableAmount(item.estimated_value), validSavedRevision(item.notice_revision)]);
}
function compareSavedNotice(saved, current) {
  if (!current || current.source !== saved.source || current.external_id !== saved.external_id
      || typeof current.title !== 'string' || !current.title
      || !(current.buyer === null || typeof current.buyer === 'string')
      || !(current.deadline === null || validNoticeDate(current.deadline))
      || comparableAmount(current.estimated_value) === undefined) return null;
  const changes = [];
  for (const field of ['title', 'buyer', 'deadline', 'estimated_value']) {
    const before = field === 'estimated_value' ? comparableAmount(saved[field]) : saved[field] || null;
    const after = field === 'estimated_value' ? comparableAmount(current[field]) : current[field] || null;
    if (before !== undefined && before !== after) changes.push({field, before: saved[field], after: current[field]});
  }
  const beforeRevision = validSavedRevision(saved.notice_revision), afterRevision = noticeRevision(current);
  let revisionCompared = false;
  if (beforeRevision && afterRevision) {
    for (const field of ['notice_version', 'bosa_notice_id']) {
      if (beforeRevision[field] && afterRevision[field]) {
        revisionCompared = true;
        if (beforeRevision[field] !== afterRevision[field]) changes.push({field: 'revision', before: beforeRevision[field], after: afterRevision[field]});
      }
    }
  }
  return {changes, deadline: current.deadline, revisionCompared};
}
function rememberNoticeRevision(detail) {
  if (!detail || typeof detail.source !== 'string' || typeof detail.external_id !== 'string') return;
  knownNoticeRevisions.set(shortlistIdentity(detail), {source: detail.source, external_id: detail.external_id, title: detail.title, buyer: detail.buyer, deadline: detail.deadline, estimated_value: detail.estimated_value, raw: noticeRevision(detail) || {}});
}
function savedNoticeRevision(item) {
  const detail = knownNoticeRevisions.get(shortlistIdentity(item));
  const comparison = detail && compareSavedNotice(item, detail);
  return comparison && !comparison.changes.length ? noticeRevision(detail) : null;
}

function renderShortlistChanges() {
  if (!shortlist.length) return '';
  const today = brusselsToday();
  const cells = shortlist.map(item => {
    const state = shortlistChecks.get(snapshotCheckKey(item));
    const label = !state ? 'changesNotChecked' : state.status === 'checking' ? 'checkingChanges'
      : state.status !== 'checked' ? 'changesUnavailable' : state.changes.length ? 'noticeChanged' : 'noRecordedChanges';
    const deadline = state?.status === 'checked' ? state.deadline : item.deadline;
    const expired = validNoticeDate(deadline) && deadline < today;
    const fieldLabels = {title: 'tender', buyer: 'buyer', deadline: 'closingLabel', estimated_value: 'estimatedValue', revision: 'noticeRevision'};
    const fieldValue = (field, value) => value == null || value === '' ? t('notStated')
      : field === 'deadline' ? formatDate(value) : String(value);
    const changes = state?.status === 'checked' ? state.changes.map(change =>
      `<li>${escapeHtml(t(fieldLabels[change.field]))}: ${escapeHtml(fieldValue(change.field, change.before))} → ${escapeHtml(fieldValue(change.field, change.after))}</li>`).join('') : '';
    return `<td><span class="review-badge">${escapeHtml(t(label))}</span>${expired ? `<p class="deadline-passed">${escapeHtml(t(state?.status === 'checked' ? 'retainedDeadlinePassed' : 'savedDeadlinePassed'))}</p>` : ''}
      ${changes ? `<ul class="notice-changes">${changes}</ul>` : ''}
      ${state?.checkedAt ? `<p class="field-help">${escapeHtml(t('checkedAt'))}: <time datetime="${escapeHtml(state.checkedAt)}">${escapeHtml(new Intl.DateTimeFormat(numberLocale(), {dateStyle: 'short', timeStyle: 'short', timeZone: 'Europe/Brussels'}).format(new Date(state.checkedAt)))}</time></p>` : ''}
      ${state?.status === 'checked' && !state.revisionCompared ? `<p class="field-help">${escapeHtml(t('revisionNotCompared'))}</p>` : ''}</td>`;
  }).join('');
  return `<tr id="shortlist-changes-row"><th scope="row">${escapeHtml(t('changesSinceSaving'))}</th>${cells}</tr>`;
}
function refreshShortlistChanges() {
  const row = workspaceQuery('#shortlist-changes-row');
  if (row) row.outerHTML = renderShortlistChanges();
  const button = workspaceQuery('#check-shortlist-changes');
  button.disabled = shortlistChecking || !shortlist.length;
  button.dataset.i18n = shortlistChecking ? 'checkingChanges' : 'checkSavedNotices';
  button.textContent = t(button.dataset.i18n);
}
async function checkShortlistChanges() {
  if (shortlistChecking || !shortlistEnterpriseNumber || !shortlist.length) return;
  const generation = ++shortlistCheckGeneration, companyNumber = shortlistEnterpriseNumber;
  const items = shortlist.slice(0, 5).map(item => ({...item}));
  const current = () => generation === shortlistCheckGeneration && companyNumber === shortlistEnterpriseNumber;
  const wanted = key => shortlist.some(item => snapshotCheckKey(item) === key);
  shortlistChecking = true;
  items.forEach(item => shortlistChecks.set(snapshotCheckKey(item), {status: 'checking'}));
  refreshShortlistChanges();
  try {
    for (const item of items) {
      if (!current()) return;
      const key = snapshotCheckKey(item);
      if (!wanted(key)) continue;
      let state = {status: 'unavailable'};
      let stop = false;
      try {
        const response = await apiFetch(`/tenders/${encodeURIComponent(item.source)}/${encodeURIComponent(item.external_id)}`);
        if (!current()) return;
        stop = [401, 403, 429].includes(response.status);
        if (response.ok) {
          const detail = await response.json();
          if (!current()) return;
          const comparison = compareSavedNotice(item, detail);
          if (comparison) {
            state = {...comparison, status: 'checked', checkedAt: new Date().toISOString()};
            rememberNoticeRevision(detail);
          }
        }
      } catch { /* Unavailable evidence remains unknown, including malformed API payloads. */ }
      if (!current()) return;
      if (wanted(key)) shortlistChecks.set(key, state);
      refreshShortlistChanges();
      if (stop) break;
    }
  } finally {
    if (current()) {
      for (const item of items) {
        const key = snapshotCheckKey(item);
        if (shortlistChecks.get(key)?.status === 'checking') shortlistChecks.set(key, {status: 'unavailable'});
      }
      shortlistChecking = false;
      refreshShortlistChanges();
    }
  }
}

window.addEventListener('DOMContentLoaded', () => {
  document.addEventListener('keydown', handleWorkspaceShortcut);
  workspaceQuery('#check-shortlist-changes').addEventListener('click', checkShortlistChanges);
  const scheduleRemember = () => {
    clearTimeout(workspaceScrollTimer);
    workspaceScrollTimer = setTimeout(rememberWorkspace, 150);
  };
  window.addEventListener('scroll', scheduleRemember, {passive: true});
  workspaceQuery('#selected-panel').addEventListener('scroll', scheduleRemember, {passive: true});
  window.addEventListener('pagehide', rememberWorkspace);
  resumeWorkspace();
});
