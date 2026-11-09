/* Presentation state only. Ranking, source extraction and feedback stay in the existing API/renderer. */
let workspaceView = 'opportunities';
let selectedCard = null;
let selectedHome = null;
let selectedTrigger = null;
let preferenceBaseline = null;
let preferenceSaving = false;
let shortlistUndo = null;
let filterHome = null;
const preferenceIds = ['confirmed-activities', 'excluded-terms', 'preferred-regions', 'languages', 'minimum-value', 'maximum-value', 'recognition-class', 'recognition-categories'];
const workspaceQuery = selector => document.querySelector(selector);

function showWorkspaceView(view, focus = true) {
  if (!['opportunities', 'shortlist', 'profile'].includes(view)) return;
  if (focus) cancelWorkspaceRestore();
  closeTenderSelection(false);
  workspaceView = view;
  document.querySelectorAll('[id^="view-"]').forEach(section => { section.hidden = section.id !== `view-${view}`; });
  document.querySelectorAll('[data-view]').forEach(button => {
    if (button.dataset.view === view) button.setAttribute('aria-current', 'page');
    else button.removeAttribute('aria-current');
  });
  updateShortlistKeyboardAccess();
  if (view === 'shortlist') refreshShortlistFollowUps();
  if (focus) {
    const target = workspaceQuery(`#view-${view} h1, #view-${view} #page-title`);
    if (target) { target.tabIndex = -1; target.focus(); }
  }
  rememberWorkspace();
}

function workspaceCompanyLoaded(profile) {
  delete workspaceQuery('#sidebar-company-name').dataset.i18n;
  delete workspaceQuery('#sidebar-company-meta').dataset.i18n;
  workspaceQuery('#sidebar-company-name').textContent = profile.names[0] || profile.enterprise_number;
  workspaceQuery('#sidebar-company-meta').textContent = profile.enterprise_number;
  workspaceQuery('#mobile-company-name').textContent = profile.names[0] || profile.enterprise_number;
  workspaceQuery('#profile-empty').hidden = true;
  document.body.classList.add('company-loaded');
  shortlistUndo = null;
  workspaceQuery('#workspace-toast').hidden = true;
}

function resetWorkspaceForSearch() {
  activeEnterpriseNumber = '';
  shortlistEnterpriseNumber = '';
  shortlist = [];
  shortlistUndo = null;
  preferenceBaseline = null;
  preferenceSaving = false;
  renderShortlist();
  document.body.classList.remove('company-loaded');
  workspaceQuery('#sidebar-company-name').dataset.i18n = 'chooseCompany';
  workspaceQuery('#sidebar-company-meta').dataset.i18n = 'lookupHint';
  workspaceQuery('#sidebar-company-name').textContent = t('chooseCompany');
  workspaceQuery('#sidebar-company-meta').textContent = t('lookupHint');
  workspaceQuery('#mobile-company-name').textContent = '';
  workspaceQuery('#profile-empty').hidden = false;
  workspaceQuery('#workspace-toast').hidden = true;
  updatePreferenceState();
}

function updateShortlistNavigation() {
  workspaceQuery('#nav-shortlist-count').textContent = String(shortlist.length);
  workspaceQuery('#shortlist-empty').hidden = shortlist.length > 0;
  refreshShortlistChanges();
}

// A selected tender is the original card DOM, not a second renderer with duplicate controls/state.
function mountTenderWorkspace(items) {
  const cards = [...results.querySelectorAll(':scope > .card')];
  cards.forEach((card, index) => {
    const item = items[index];
    const row = document.createElement('div');
    row.className = 'result-item';
    const trigger = document.createElement('button');
    trigger.type = 'button';
    trigger.className = 'result-select';
    trigger.setAttribute('aria-expanded', 'false');
    trigger.setAttribute('aria-controls', 'selected-panel');
    trigger.setAttribute('aria-keyshortcuts', 'ArrowDown ArrowUp Enter S');
    const title = document.createElement('span');
    title.className = 'result-title';
    title.textContent = item.title;
    trigger.append(title, card.querySelector('.triage-facts').cloneNode(true));
    const reason = card.querySelector('.primary-reason');
    if (reason) trigger.append(reason.cloneNode(true));
    const score = document.createElement('span');
    score.className = 'result-score';
    score.innerHTML = `${card.querySelector('.score-fit strong').outerHTML} <span>/100</span> <small data-i18n="fitScore">${escapeHtml(t('fitScore'))}</small>`;
    trigger.append(score);
    trigger.addEventListener('click', () => selectTender(card, row, trigger));
    const meta = document.createElement('div');
    meta.className = 'result-meta';
    const badge = document.createElement('span');
    badge.className = 'review-badge';
    badge.innerHTML = `<span aria-hidden="true">△</span> <span data-i18n="requirementsUnverified">${escapeHtml(t('requirementsUnverified'))}</span>`;
    const source = document.createElement('span');
    source.className = 'source-label';
    source.textContent = `${item.source} · ${item.external_id}`;
    const originalSave = card.querySelector('[data-shortlist-toggle]');
    const save = originalSave.cloneNode(true);
    save.addEventListener('click', () => originalSave.click());
    meta.append(badge, source, save);
    card.before(row);
    row.append(trigger, meta, card);
  });
  applyUiLanguage(uiLanguage);
  // Opening automatically is useful on a roomy desktop, but never takes over a mobile screen.
  if (!workspaceRestore && workspaceView === 'opportunities' && cards.length && matchMedia('(min-width: 1251px)').matches) {
    const row = cards[0].parentElement;
    selectTender(cards[0], row, row.querySelector('.result-select'), false);
  }
}

function selectTender(card, home, trigger, focus = true) {
  closeTenderSelection(false);
  selectedCard = card;
  selectedHome = home;
  selectedTrigger = trigger;
  home.classList.add('selected');
  trigger.setAttribute('aria-expanded', 'true');
  workspaceQuery('#selected-content').append(card);
  workspaceQuery('#selected-panel').hidden = false;
  workspaceQuery('#tender-workspace').classList.add('has-selection');
  setDetailTab('overview');
  syncDetailModal();
  if (focus) workspaceQuery('#close-selected').focus();
  rememberWorkspace();
}

function closeTenderSelection(restoreFocus = true) {
  if (!selectedCard) return;
  const trigger = selectedTrigger;
  selectedHome.append(selectedCard);
  selectedHome.classList.remove('selected');
  trigger.setAttribute('aria-expanded', 'false');
  workspaceQuery('#selected-panel').hidden = true;
  workspaceQuery('#tender-workspace').classList.remove('has-selection');
  selectedCard = selectedHome = selectedTrigger = null;
  syncDetailModal();
  if (restoreFocus && trigger.isConnected) trigger.focus();
  rememberWorkspace();
}

function syncDetailModal() {
  const panel = workspaceQuery('#selected-panel');
  const modal = !!selectedCard && matchMedia('(max-width: 1250px)').matches;
  document.body.classList.toggle('detail-modal', modal);
  panel.setAttribute('role', modal ? 'dialog' : 'region');
  if (modal) panel.setAttribute('aria-modal', 'true');
  else panel.removeAttribute('aria-modal');
  // Native inert prevents pointer and keyboard interaction with the background modal content.
  workspaceQuery('.site-header').inert = modal;
  workspaceQuery('.workspace-sidebar').inert = modal;
  workspaceQuery('#results').inert = modal;
  workspaceQuery('.intro').inert = modal;
  workspaceQuery('.mobile-toolbar').inert = modal;
  workspaceQuery('.site-footer').inert = modal;
  const toast = workspaceQuery('#workspace-toast');
  if (modal) panel.append(toast);
  else if (toast.parentElement === panel) document.body.append(toast);
  if (modal && !panel.contains(document.activeElement)) workspaceQuery('#close-selected').focus();
}

function setDetailTab(tab) {
  if (!selectedCard || !['overview', 'evidence'].includes(tab)) return;
  workspaceQuery('#selected-panel').dataset.detailView = tab;
  document.querySelectorAll('[data-detail-tab]').forEach(button => {
    const active = button.dataset.detailTab === tab;
    button.setAttribute('aria-selected', String(active));
    button.tabIndex = active ? 0 : -1;
  });
  workspaceQuery('#selected-content').setAttribute('aria-labelledby', `tab-${tab}`);
  if (tab === 'evidence') {
    const button = selectedCard.querySelector('[data-detail]');
    if (!button.disabled) button.click();
  }
}

function openFilters() {
  const dialog = workspaceQuery('#filter-dialog');
  const panel = workspaceQuery('.search-panel');
  filterHome = panel.parentElement;
  workspaceQuery('#filter-dialog-content').append(panel);
  dialog.showModal();
}
function closeFilters() {
  const dialog = workspaceQuery('#filter-dialog');
  if (dialog.open) dialog.close();
  if (filterHome) {
    filterHome.insertBefore(workspaceQuery('.search-panel'), workspaceQuery('#health'));
    filterHome = null;
  }
}

function preferenceValues() {
  return Object.fromEntries(preferenceIds.map(id => [id, workspaceQuery(`#${id}`).value]));
}
function capturePreferenceBaseline() {
  preferenceBaseline = preferenceValues();
  updatePreferenceState();
}
function preferencesDirty() {
  return !!preferenceBaseline && preferenceIds.some(id => workspaceQuery(`#${id}`).value !== preferenceBaseline[id]);
}
function updatePreferenceState() {
  const dirty = preferencesDirty();
  const status = workspaceQuery('#preference-status');
  status.dataset.i18n = preferenceSaving ? 'savingFeedback' : dirty ? 'unsavedChanges' : 'preferencesUnchanged';
  status.textContent = t(status.dataset.i18n);
  workspaceQuery('#save-preferences').disabled = exampleWorkspace.active || !activeEnterpriseNumber || !dirty || preferenceSaving;
  workspaceQuery('#discard-preferences').disabled = exampleWorkspace.active || !dirty || preferenceSaving;
  preferenceIds.forEach(id => { workspaceQuery(`#${id}`).disabled = exampleWorkspace.active || preferenceSaving; });
}
function setPreferenceSaving(pending) { preferenceSaving = pending; updatePreferenceState(); }

function recordShortlistUndo() {
  shortlistUndo = { enterprise: shortlistEnterpriseNumber, items: shortlist.map(item => ({...item})) };
}
function notifyShortlistChange(persisted) {
  showWorkspaceToast(t(persisted ? 'shortlistUpdated' : 'shortlistNotPersisted'), true);
}
function showWorkspaceToast(text, undo = false) {
  workspaceQuery('#toast-text').textContent = text;
  workspaceQuery('#undo-shortlist').hidden = !undo;
  workspaceQuery('#workspace-toast').hidden = false;
}

// Local reviewer notes are deliberately separate from API evidence and server-side bid/outcome feedback.
function validAssessment(value) { return ['unreviewed', 'explore', 'pass'].includes(value) ? value : 'unreviewed'; }

// Local planning metadata is separate from the notice deadline and reviewer evidence.
function shortlistFollowUp(item) {
  const next_action = typeof item.next_action === 'string' ? item.next_action.trim().slice(0, 240) : '';
  const date = validNoticeDate(item.follow_up_date);
  const follow_up_date = date && date >= '0001-01-01' ? date : '';
  return {next_action, follow_up_date, action_done: item.action_done === true && !!(next_action || follow_up_date)};
}
function followUpStatus(item, today = brusselsToday()) {
  const action = shortlistFollowUp(item);
  if (action.action_done) return 'done';
  if (!action.follow_up_date) return 'unscheduled';
  return action.follow_up_date < today ? 'overdue' : action.follow_up_date === today ? 'today' : 'upcoming';
}
function renderFollowUpStatus(item, today) {
  const state = followUpStatus(item, today);
  const keys = {overdue:'followUpOverdue',today:'followUpToday',upcoming:'followUpUpcoming',unscheduled:'followUpUnscheduled',done:'followUpDone'};
  return `<span class="follow-up-status" data-state="${state}">${escapeHtml(t(keys[state]))}</span>`;
}
function renderShortlistFollowUps() {
  const today = brusselsToday();
  const actions = shortlist.map(item => {
    const action = shortlistFollowUp(item), key = escapeHtml(shortlistIdentity(item));
    return `<td><input class="shortlist-action" data-shortlist-next-action="${key}" maxlength="240" value="${escapeHtml(action.next_action)}" aria-label="${escapeHtml(t('nextAction') + ': ' + item.title)}"></td>`;
  }).join('');
  const dates = shortlist.map(item => {
    const action = shortlistFollowUp(item), key = escapeHtml(shortlistIdentity(item));
    const label = t(action.action_done ? 'reopenAction' : 'markActionDone');
    return `<td><div class="shortlist-follow-up"><input type="date" min="0001-01-01" max="9999-12-31" data-shortlist-follow-up="${key}" value="${action.follow_up_date}" aria-label="${escapeHtml(t('followUpDate') + ': ' + item.title)}"><div data-shortlist-follow-up-state="${key}" role="status" aria-live="polite">${renderFollowUpStatus(item, today)}</div><button class="secondary" type="button" data-shortlist-action-done="${key}" aria-label="${escapeHtml(label + ': ' + item.title)}"${action.next_action || action.follow_up_date ? '' : ' disabled'}>${escapeHtml(label)}</button></div></td>`;
  }).join('');
  return `<tr><th scope="row">${escapeHtml(t('nextAction'))}<br><small>${escapeHtml(t('browserOnly'))}</small></th>${actions}</tr><tr><th scope="row">${escapeHtml(t('followUpDate'))}</th>${dates}</tr>`;
}
function refreshShortlistFollowUps() {
  const content = workspaceQuery('#shortlist-content'), today = brusselsToday();
  content.querySelectorAll('[data-shortlist-follow-up-state]').forEach(element => {
    const item = shortlist.find(item => shortlistIdentity(item) === element.dataset.shortlistFollowUpState);
    if (item) {
      const markup = renderFollowUpStatus(item, today);
      if (element.innerHTML !== markup) element.innerHTML = markup;
    }
  });
  content.querySelectorAll('[data-shortlist-action-done]').forEach(button => {
    const item = shortlist.find(item => shortlistIdentity(item) === button.dataset.shortlistActionDone);
    if (!item) return;
    const action = shortlistFollowUp(item), label = t(action.action_done ? 'reopenAction' : 'markActionDone');
    button.textContent = label;
    button.setAttribute('aria-label', `${label}: ${item.title}`);
    button.disabled = !(action.next_action || action.follow_up_date);
  });
}
function updateShortlistFollowUp(key, field, value) {
  const item = shortlist.find(item => shortlistIdentity(item) === key);
  if (!item || !['next_action','follow_up_date','action_done'].includes(field)) return false;
  const before = shortlistFollowUp(item);
  if (field === 'follow_up_date' && value !== '' && (!validNoticeDate(value) || value < '0001-01-01')) return false;
  const after = shortlistFollowUp({...before, [field]:value});
  if (before[field] === after[field]) return false;
  // Changing the plan reopens a completed action. Notes and assessments do not.
  if (field !== 'action_done') after.action_done = false;
  recordShortlistUndo();
  Object.assign(item, after);
  refreshShortlistFollowUps();
  notifyShortlistChange(persistShortlist());
  return true;
}
function bindShortlistFollowUps() {
  const content = workspaceQuery('#shortlist-content'), company = shortlistEnterpriseNumber;
  content.querySelectorAll('[data-shortlist-next-action], [data-shortlist-follow-up]').forEach(control => {
    control.addEventListener('change', () => {
      if (company !== shortlistEnterpriseNumber) return;
      const isDate = control.hasAttribute('data-shortlist-follow-up');
      const key = isDate ? control.dataset.shortlistFollowUp : control.dataset.shortlistNextAction;
      const item = shortlist.find(item => shortlistIdentity(item) === key);
      if (!item) return;
      if (isDate) {
        control.setCustomValidity('');
        if (control.validity.badInput || (control.value && (!validNoticeDate(control.value) || control.value < '0001-01-01'))) {
          control.setCustomValidity(t('invalidFollowUpDate'));
          control.reportValidity();
          return;
        }
      }
      const field = isDate ? 'follow_up_date' : 'next_action';
      updateShortlistFollowUp(key, field, control.value);
      control.value = shortlistFollowUp(item)[field];
    });
  });
  content.querySelectorAll('[data-shortlist-action-done]').forEach(button => {
    button.addEventListener('click', () => {
      if (company !== shortlistEnterpriseNumber) return;
      const item = shortlist.find(item => shortlistIdentity(item) === button.dataset.shortlistActionDone);
      if (item) updateShortlistFollowUp(shortlistIdentity(item), 'action_done', !shortlistFollowUp(item).action_done);
    });
  });
}

function renderShortlistAssessments() {
  const options = [['unreviewed', 'notReviewed'], ['explore', 'worthExploring'], ['pass', 'notPursuing']];
  const assessments = shortlist.map(item => `<td><select data-shortlist-assessment="${escapeHtml(shortlistIdentity(item))}" aria-label="${escapeHtml(t('yourAssessment') + ': ' + item.title)}">${options.map(([value, label]) => `<option value="${value}"${validAssessment(item.assessment) === value ? ' selected' : ''}>${escapeHtml(t(label))}</option>`).join('')}</select></td>`).join('');
  const notes = shortlist.map(item => `<td><input class="shortlist-note" data-shortlist-note="${escapeHtml(shortlistIdentity(item))}" maxlength="1000" value="${escapeHtml(item.note || '')}" aria-label="${escapeHtml(t('reviewNote') + ': ' + item.title)}"></td>`).join('');
  return `<tr><th scope="row">${escapeHtml(t('yourAssessment'))}<br><small>${escapeHtml(t('browserOnly'))}</small></th>${assessments}</tr><tr><th scope="row">${escapeHtml(t('reviewNote'))}</th>${notes}</tr>`;
}
function bindShortlistAssessments() {
  shortlistContent.querySelectorAll('[data-shortlist-assessment], [data-shortlist-note]').forEach(control => {
    control.addEventListener('change', () => {
      const key = control.dataset.shortlistAssessment || control.dataset.shortlistNote;
      const item = shortlist.find(item => shortlistIdentity(item) === key);
      if (!item) return;
      recordShortlistUndo();
      if (control.hasAttribute('data-shortlist-assessment')) item.assessment = validAssessment(control.value);
      else item.note = control.value.slice(0, 1000);
      notifyShortlistChange(persistShortlist());
    });
  });
}

function undoShortlistChange() {
    if (!shortlistUndo || shortlistUndo.enterprise !== shortlistEnterpriseNumber) return;
    shortlist = shortlistUndo.items;
    shortlistUndo = null;
    const persisted = persistShortlist();
    renderShortlist();
    showWorkspaceToast(t(persisted ? 'changeUndone' : 'shortlistNotPersisted'));
}

window.addEventListener('DOMContentLoaded', () => {
  document.querySelectorAll('[data-view], [data-go-view]').forEach(button => {
    button.addEventListener('click', () => showWorkspaceView(button.dataset.view || button.dataset.goView));
  });
  workspaceQuery('#close-selected').addEventListener('click', () => closeTenderSelection());
  document.querySelectorAll('[data-detail-tab]').forEach(button => {
    button.addEventListener('click', () => setDetailTab(button.dataset.detailTab));
    button.addEventListener('keydown', event => {
      if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      const tab = event.key === 'Home' ? 'overview' : event.key === 'End' ? 'evidence'
        : button.dataset.detailTab === 'overview' ? 'evidence' : 'overview';
      setDetailTab(tab);
      workspaceQuery(`#tab-${tab}`).focus();
    });
  });
  document.addEventListener('keydown', event => {
    if (!selectedCard) return;
    if (event.defaultPrevented || event.isComposing || event.target?.closest('select')) return;
    if (event.key === 'Escape') { event.preventDefault(); closeTenderSelection(); }
    if (event.key !== 'Tab' || !document.body.classList.contains('detail-modal')) return;
    const focusable = [...workspaceQuery('#selected-panel').querySelectorAll('a[href], button:not(:disabled), select:not(:disabled), summary, [tabindex="0"]')].filter(el => el.tabIndex >= 0 && el.getClientRects().length);
    const first = focusable[0], last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
    else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
  });
  workspaceQuery('#open-filters').addEventListener('click', openFilters);
  workspaceQuery('#close-filters').addEventListener('click', closeFilters);
  workspaceQuery('#filter-dialog').addEventListener('close', () => {
    if (!workspaceQuery('#filter-dialog').open) closeFilters();
  });
  workspaceQuery('#reset-filters').addEventListener('click', () => {
    workspaceQuery('#sort-order').value = 'score';
    workspaceQuery('#deadline-window').value = '';
  });
  preferenceIds.forEach(id => workspaceQuery(`#${id}`).addEventListener('input', updatePreferenceState));
  workspaceQuery('#discard-preferences').addEventListener('click', () => {
    if (!preferenceBaseline || preferenceSaving) return;
    preferenceIds.forEach(id => { workspaceQuery(`#${id}`).value = preferenceBaseline[id]; });
    renderRecognitionGlossary();
    updatePreferenceState();
  });
  workspaceQuery('#undo-shortlist').addEventListener('click', undoShortlistChange);
  workspaceQuery('#dismiss-toast').addEventListener('click', () => { workspaceQuery('#workspace-toast').hidden = true; });
  window.addEventListener('resize', syncDetailModal);
  window.addEventListener('focus', refreshShortlistFollowUps);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) refreshShortlistFollowUps(); });
  window.addEventListener('beforeunload', event => {
    if (preferencesDirty()) { event.preventDefault(); event.returnValue = ''; }
  });
  updatePreferenceState();
});
