const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

// Exercise the real inline bootstrap code without running the dashboard's
// network/rendering handlers. No browser packages or external requests.
const html = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');
const testTranslations = {
  openProcurement: 'open procurement', openProcurements: 'open procurements', unknownOpen: 'open procurements unknown',
  freshData: 'Fresh data', staleData: 'Data is stale', freshnessUnknown: 'Freshness unverified',
  tenderLoaded: 'tender loaded', tendersLoaded: 'tenders loaded', dataDetails: 'Data details',
  dataUnavailable: 'Data status could not be loaded. Check the API server connection.',
  noResults: 'No open tenders matched this company and these filters.',
  noResultsHelp: 'Try a longer closing-date window or adjust your company profile, then search again.',
  resultsShown: 'Recommendations shown: {count} (max 20 per search)', anyDeadlineSummary: 'any deadline',
  deadlineWithinSummary: 'deadline within {days} days', sortedBestMatchSummary: 'best match', sortedSoonestSummary: 'soonest deadline',
  scoreGuideTitle: 'How scores work',
  scoreGuideCopy: 'Fit score ranks how closely a notice matches the company profile. Evidence signal reflects positive support across five areas. Neither is a probability of winning or an eligibility decision.',
  fitScore: 'Fit score', evidenceSignal: 'Evidence signal', buyer: 'Buyer', closingLabel: 'Closing date',
  estimatedValue: 'Estimated value', moreTenderDetails: 'Location, CPV and language', location: 'Location', cpv: 'CPV', language: 'Language',
  whyMayFit: 'Why it may fit', otherSignals: 'Other match signals', checkManually: 'Check manually',
  requirementSignals: 'Requirement signals', showRequirements: 'Show extracted requirements', sourceRecord: 'Source record',
  reviewOfficialNotice: 'Review official notice', moreActions: 'More actions', tender: 'Tender', matchFeedback: 'Match feedback',
  viewDetails: 'View details', structuredAnalysis: 'Structured analysis', relevant: 'Relevant', notRelevant: 'Not relevant',
  unknownBuyer: 'Unknown buyer', notStated: 'Not stated', status: 'Status',
  reasonActivityMatch: 'Tender activity matches registered company activities',
  reasonLanguageMatch: 'Tender language matches preferred languages', reasonRegionMatch: 'Tender location matches a company municipality',
  reasonValueForReview: 'Contract value is available for capacity review', reasonValueWithinAward: 'Contract value is within the previous award range',
  reasonPreviousAward: 'CPV category matches a previous awarded contract', registeredActivities: 'Registered activities',
  officialKbo: 'Official KBO data', noneFound: 'None found', companyProfile: 'Company profile',
};
const testTranslate = key => testTranslations[key] || key;
const noOpApplyLanguage = () => {};
const start = html.indexOf("    const tokenField = document.querySelector('#api-token');");
const end = html.indexOf("    try { tokenField.value =", start);
assert.ok(start >= 0 && end > start, 'dashboard bootstrap code must be present');
const bootstrap = html.slice(start, end);

function run(href, saved = 'existing', example = false) {
  let stored = saved;
  let cleaned;
  vm.runInNewContext(bootstrap, {
    URL, URLSearchParams,
    exampleWorkspace: {active:example},
    location: { href },
    document: { querySelector: () => ({}) },
    localStorage: { setItem: (key, value) => { assert.equal(key, 'aanbestedingen_token'); stored = value; } },
    history: { replaceState: (_state, _title, value) => { cleaned = value; } },
  });
  return { stored, cleaned };
}

test('fragment token is remembered and stripped, unrelated parameters survive', () => {
  assert.deepEqual(run('https://app.test/?filter=open#token=new%2Bsecret&section=profile'), {
    stored: 'new+secret', cleaned: '/?filter=open#section=profile',
  });
});

test('query tokens are stripped but never accepted, retaining a saved token', () => {
  assert.deepEqual(run('https://app.test/?token=legacy&filter=open#section'), {
    stored: 'existing', cleaned: '/?filter=open#section',
  });
});

test('fragment wins over legacy query and duplicate secrets are all removed', () => {
  assert.deepEqual(run('https://app.test/?token=legacy&token=other#token=new&token=duplicate'), {
    stored: 'new', cleaned: '/',
  });
});

test('empty token does not overwrite saved credentials', () => {
  assert.deepEqual(run('https://app.test/#token=%20'), { stored: 'existing', cleaned: '/' });
});

test('ordinary URLs are unchanged', () => {
  assert.deepEqual(run('https://app.test/?filter=open#section'), { stored: 'existing', cleaned: undefined });
});

test('example mode strips fragment credentials without replacing the saved real token', () => {
  assert.deepEqual(run('https://app.test/?example=1#token=example-secret', 'existing', true), {
    stored:'existing', cleaned:'/?example=1',
  });
});

test('authentication hint advertises only fragment token bootstrap', () => {
  assert.ok(html.includes('paste it above (or add #token=... to the URL).'));
  assert.ok(!html.includes('paste it above (or add ?token=... to the URL).'));
});

// The dashboard's health line is coverage, not only freshness: it must show the
// publication range the store holds and admit when rows predate that column.
// Exercise the real inline block against payloads shaped like /health answers.
const healthStart = html.indexOf('    const escapeHtml =');
const healthEnd = html.indexOf('    function renderCompany', healthStart);
assert.ok(healthStart >= 0 && healthEnd > healthStart, 'health block must be present');
const healthBlock = html.slice(healthStart, healthEnd);

// A minimal promise shim: `.then` chains, and a callback that returns another
// thenable is followed, which is what `apiFetch(...).then(r => r.json())` needs.
function resolved(value) {
  return {
    then(onFulfilled) {
      const result = onFulfilled(value);
      return result && typeof result.then === 'function' ? result : resolved(result);
    },
    catch() { return resolved(value); },
  };
}

function renderHealth(payload, raw = false) {
  let rendered = '';
  vm.runInNewContext(healthBlock, {
    apiFetch: () => resolved({ json: () => resolved(payload) }),
    t: testTranslate,
    uiLanguage: 'en',
    applyUiLanguage: noOpApplyLanguage,
    health: {
      set textContent(value) { rendered = value; },
      set innerHTML(value) { rendered = value; },
      get textContent() { return rendered; },
    },
  });
  return raw ? rendered : rendered.replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').trim();
}

const copyStart = html.indexOf('    const uiCopy = {');
const copyEnd = html.indexOf('    const languageSelect', copyStart);
assert.ok(copyStart >= 0 && copyEnd > copyStart, 'locale dictionaries must be present');
const uiCopy = vm.runInNewContext(`${html.slice(copyStart, copyEnd)}; uiCopy`);

const apiErrorStart = html.indexOf('    async function apiErrorText(response, resource) {');
const apiErrorEnd = html.indexOf("    apiFetch('/health')", apiErrorStart);
assert.ok(apiErrorStart >= 0 && apiErrorEnd > apiErrorStart, 'API error formatter must be present');
const apiErrorBlock = html.slice(apiErrorStart, apiErrorEnd);

async function explainApiError(status, contentType, body, resource = 'company', language = 'en') {
  const response = { status, headers: { get: () => contentType }, text: async () => body };
  const t = key => uiCopy[language]?.[key] || uiCopy.en[key] || key;
  return vm.runInNewContext(`${apiErrorBlock}; apiErrorText(response, resource)`, { response, resource, t });
}

test('API errors replace server HTML and internal failures with actionable copy', async () => {
  const htmlError = await explainApiError(404, 'text/html', '<!DOCTYPE html><h1>Error response</h1><p>File not found</p>');
  assert.match(htmlError, /HTML page instead of API data/);
  assert.doesNotMatch(htmlError, /File not found|<!DOCTYPE/i);
  const missingCompany = await explainApiError(404, 'application/json', '{"detail":"company not found"}');
  assert.match(missingCompany, /not in the configured KBO register/);
  const internalError = await explainApiError(500, 'application/json', '{"detail":"private database path"}', 'recommendations');
  assert.match(internalError, /temporarily unavailable/);
  assert.doesNotMatch(internalError, /private database path/);
});

test('API errors use the selected Dutch and French copy', async () => {
  const dutch = await explainApiError(404, 'text/html', '<!doctype html>', 'company', 'nl');
  assert.match(dutch, /HTML-pagina terug in plaats van API-gegevens/);
  const french = await explainApiError(500, 'application/json', '{}', 'recommendations', 'fr');
  assert.match(french, /temporairement indisponible/);
});

test('empty recommendation results explain how to broaden the search', () => {
  const start = html.indexOf('    function renderResults(items, enterpriseNumber) {');
  const end = html.indexOf('    form.addEventListener(\'submit\'', start);
  let rendered = '';
  vm.runInNewContext(`${html.slice(start, end)}; renderResults([], '0404.484.060');`, {
    t: testTranslate, uiLanguage: 'en', applyUiLanguage: noOpApplyLanguage,
    heading: { classList: { remove() {} } },
    resultsSummary: { dataset: {}, classList: { remove() {}, add() {} } },
    activeSort: 'score', activeDueWithin: '',
    results: { set innerHTML(value) { rendered = value; } },
    closeTenderSelection() {},
  });
  assert.match(rendered, /No open tenders matched this company and these filters/);
  assert.match(rendered, /Try a longer closing-date window or adjust your company profile/);
});

test('freshness stays prominent while detailed provenance is collapsed and escaped', () => {
  const markup = renderHealth({
    tenders: 1,
    tenders_open_procurements: 1,
    tenders_freshness: 'stale',
    tenders_last_fetched: '<img src=x>',
    tenders_oldest_publication_date: '2026-01-05',
    tenders_newest_publication_date: '2026-01-05',
    award_history_companies: 0,
    recognition_register: null,
  }, true);
  assert.match(markup, /1 open procurement · 1 tender loaded/);
  assert.match(markup, /class="health-freshness" data-state="stale" data-i18n="staleData">Data is stale/);
  assert.match(markup, /<details class="health-details"><summary data-i18n="dataDetails">Data details<\/summary>/);
  assert.match(markup, /publication dates|2026-01-05/);
  assert.match(markup, /&lt;img src=x&gt;/);
  assert.doesNotMatch(markup, /<img src=x>/);
  assert.doesNotMatch(markup, /<details[^>]* open/);
});

test('company activities show their code and a readable description without duplicating code-only labels', () => {
  const start = html.indexOf('    function renderCompany(profile) {');
  const end = html.indexOf("    // A company's own awarded contracts", start);
  assert.ok(start >= 0 && end > start, 'the company renderer must be present');
  const block = html.slice(start, end);
  let rendered = '';
  const classList = { remove() {} };
  const context = {
    company: { classList, set innerHTML(value) { rendered = value; } },
    preferences: { classList },
    t: testTranslate, uiLanguage: 'en', applyUiLanguage: noOpApplyLanguage,
    searchSequence: 1,
    escapeHtml: value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c])),
    loadAwardHistory() {},
    loadTenderAwardHistory() {},
    loadShortlist() {},
    workspaceCompanyLoaded() {},
    document: { querySelector: () => ({ value: '' }) },
  };
  const profile = {
    names: ['Voorbeeld Farma Groothandel'], enterprise_number: '0404.484.060', status: 'AC',
    activities: [{ code: '46460', dutch: 'Groothandel in farmaceutische producten', french: null }],
  };
  vm.runInNewContext(`${block}; renderCompany(profile);`, { ...context, profile });
  assert.match(rendered, /<code>46460<\/code><span>Groothandel in farmaceutische producten<\/span>/);
  assert.match(rendered, /Registered activities/);

  profile.activities[0] = { code: '46460', dutch: '46460', french: '46460' };
  vm.runInNewContext(`${block}; renderCompany(profile);`, { ...context, profile });
  assert.match(rendered, /NACE-BEL activity 46460/);
  assert.doesNotMatch(rendered, /<span>46460<\/span>/);
});

test('recommended opportunities appear before profile corrections in the page order', () => {
  const recommendations = html.indexOf('<h2 id="results-heading"');
  const profileCorrections = html.indexOf('<section id="preferences"');
  assert.ok(recommendations >= 0 && profileCorrections > recommendations);
});

test('official KBO activities are clearly distinct from editable matching preferences', () => {
  assert.match(html, /data-i18n="registeredActivities"[^>]*>\$\{t\('registeredActivities'\)\}<\/span> <span class="source-badge" data-i18n="officialKbo">/);
  assert.match(html, /<h2 data-i18n="matchingProfile">Your matching profile<\/h2>/);
  assert.match(html, /preferences shape recommendations only; they never change your official KBO registration above/);
  assert.match(html, /<label><span data-i18n="activitiesToMatch">Activities to match<\/span>\s*<span class="field-help" data-i18n="activitiesHelp">Describe the work you want tender recommendations for\./);
});

/// The award-history panel was added after the endpoints existed; nothing else
/// exercises it, and a scoping mistake would fail silently in the browser (the
/// panel simply never appears).
test('recommendation cards render recognition and framework-ceiling evidence safely', () => {
  const start = html.indexOf('    function renderResults(items, enterpriseNumber) {');
  const end = html.indexOf('    form.addEventListener(\'submit\'', start);
  assert.ok(start >= 0 && end > start, 'the recommendation renderer must be present');
  const block = html.slice(start, end);
  let rendered = '';
  const heading = { classList: { remove() {} } };
  const results = {
    set innerHTML(value) { rendered = value; },
    querySelectorAll: () => [],
  };
  const escapeHtml = (value) => String(value ?? '').replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#039;',
  }[c]));
  const context = { exampleWorkspace:{active:false}, heading, results, resultsSummary: {dataset: {}, classList: {remove() {}, add() {}}}, activeSort: 'score', activeDueWithin: '',
    comparableAmount: value => value === undefined ? undefined : value, closeTenderSelection() {}, mountTenderWorkspace() {}, safeNoticeUrl: value => /^https:\/\//.test(value || '') ? value : '',
    shortlist: [], shortlistIdentity: item => JSON.stringify([item.source, item.external_id]),
    escapeHtml, t: testTranslate, uiLanguage: 'en', applyUiLanguage: noOpApplyLanguage,
    feedbackStateLabel: (...values) => feedbackFns().feedbackStateLabel(...values),
    renderLotCpvNaceAlignment: () => '',
    formatNumber: (value, options) => new Intl.NumberFormat('en-BE', options).format(value),
    formatDate: value => new Intl.DateTimeFormat('en-BE', {day: 'numeric', month: 'short', year: 'numeric', timeZone: 'UTC'}).format(new Date(`${value}T12:00:00Z`)),
    formatValue: value => value == null ? 'Value not stated' : '€48,000' };
  const item = {
    source: 'test', external_id: 't1', title: 'Example', score: 50, confidence: 0.8,
    framework_maximum_amounts: ['350000.00', '<img src=x>', '50000.00', '1000.00', '750.00', '200.00'],
    buyer: 'City of Leuven', deadline: '2026-12-18', estimated_value: 48000,
    reasons: ['Tender activity matches registered company activities', 'Deadline leaves 12 days to prepare'],
    recognition_evidence: {
      register_as_of: '2026-10-31', kbo_snapshot_as_of: '2026-06-24', statement_source: 'requirements',
      lot_aggregation_needs_review: false,
      comparison: {
        status: 'current_dated_approval_found', required_category: 'C', required_class: 7,
        evidence: [{ category: 'C1', category_description_nl: '<img src=x>', class: 8,
          validity_as_of: 'current_as_of', source_row: 12,
          kbo_match: { status: 'exact_local_snapshot' } }],
      },
    },
  };
  vm.runInNewContext(`${block}; renderResults([item], '0404.484.060');`, { ...context, item });
  assert.match(rendered, /<span class="score score-fit"><strong data-format-number="50">50<\/strong><span class="score-scale">\/100<\/span><small class="score-label" data-i18n="fitScore">Fit score<\/small>/);
  assert.match(rendered, /<span class="score score-evidence"[^>]*><strong>80<\/strong><span class="score-scale">\/100<\/span><small class="score-label" data-i18n="evidenceSignal">Evidence signal<\/small>/);
  assert.match(rendered, /How scores work/);
  assert.match(rendered, /Neither is a probability of winning or an eligibility decision/);
  assert.match(rendered, /Buyer<\/span><strong>City of Leuven<\/strong>/);
  assert.match(rendered, /Closing date<\/span><strong data-format-date="2026-12-18">18 Dec 2026<\/strong>/);
  assert.match(rendered, /Estimated value<\/span><strong data-format-currency="48000">€48,000<\/strong>/);
  assert.match(rendered, /Why it may fit/);
  assert.match(rendered, /<span data-i18n="otherSignals">Other match signals<\/span> \(1\)/);
  assert.doesNotMatch(rendered, /<details class="more-tender-details" open|<details open/);
  assert.doesNotMatch(rendered, /% confidence/);
  assert.match(rendered, /Recognition register evidence \(not an eligibility decision\)/);
  assert.match(rendered, /Register as of 2026-10-31; local KBO snapshot dated 2026-06-24/);
  assert.match(rendered, /Recognition statement in structured selection criteria: category C, class 7/);
  assert.match(rendered, /Framework maximum amounts \(ceiling, not estimated spend; currency and lot scope shown in details when retained; do not sum without checking the notice\)/);
  assert.match(rendered, /and 1 more in the notice/);
  assert.match(rendered, /&lt;img src=x&gt;/);
  assert.match(rendered, /Confirm current status and tender-specific legal requirements/);
  assert.doesNotMatch(rendered, /<img src=x>/);

  item.framework_maximum_amounts = ['2600000.00'];
  vm.runInNewContext(`${block}; renderResults([item], '0404.484.060');`, { ...context, item });
  assert.match(rendered, /Framework maximum amount \(ceiling, not estimated spend; currency and lot scope shown in details when retained\): 2600000\.00/);

  item.recognition_evidence.statement_source = 'description';
  vm.runInNewContext(`${block}; renderResults([item], '0404.484.060');`, { ...context, item });
  assert.match(rendered, /Recognition statement in notice description \(unstructured text\): category C, class 7/);
  item.recognition_evidence.lot_aggregation_needs_review = true;
  vm.runInNewContext(`${block}; renderResults([item], '0404.484.060');`, { ...context, item });
  assert.match(rendered, /does not calculate that class/);

  item.recognition_evidence.comparison.evidence[0].kbo_match = {
    status: 'exact_live_api', verified_at_utc: '2026-10-31T10:00:00Z', current_status: 'AC',
  };
  vm.runInNewContext(`${block}; renderResults([item], '0404.484.060');`, { ...context, item });
  assert.match(rendered, /live KBO check at 2026-10-31T10:00:00Z \(status AC\)/);
  item.saved_feedback = {relevant: false, pursuable: true, outcome: 'won', reason: null};
  vm.runInNewContext(`${block}; renderResults([item], '0404.484.060');`, { ...context, item });
  assert.match(rendered, /data-topic="false" data-pursuable="true"/);
  assert.match(rendered, /<option value="won" selected/);
  assert.match(rendered, /<div class="pursue-row">/);
  assert.match(rendered, /not relevant · pursuable · outcome: won/);

});

test('award history panel renders the engagements the endpoint returns', async () => {
  const start = html.indexOf('    // A company\'s own awarded contracts as recorded in the Brussels inventories.');
  const end = html.indexOf('    async function loadPreferences(', start);
  assert.ok(start >= 0 && end > start, 'the award history panel must be present');
  const block = html.slice(start, end);

  let appended;
  const panel = {
    className: '',
    set innerHTML(value) { this.html = value; },
    get innerHTML() { return this.html; },
  };
  const company = { appendChild: (node) => { appended = node; } };
  const payload = {
    engagements: [
      { engagement_id: '2024002611', buyer_name: 'SCR', engagement_date: '2024-12-31',
        engagement_amount: '129.83', object: 'Justice fees', accounting_rows: 8 },
    ],
    accounting_rows: 8,
    note: 'awarded contracts as recorded in the imported Brussels inventories',
  };
  const context = {
    company,
    searchSequence: 3,
    escapeHtml: (value) => String(value ?? ''),
    document: { createElement: () => panel, querySelector: () => company },
    apiFetch: () => Promise.resolve({ ok: true, json: () => Promise.resolve(payload) }),
    Promise,
  };
  context.loadAwardHistory = undefined;
  vm.runInNewContext(`${block}; loadAwardHistory('0214596464', 3);`, context);
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(appended, 'the panel should be appended to the company section');
  assert.match(appended.innerHTML, /Recorded award history/);
  assert.match(appended.innerHTML, /SCR/);
  assert.match(appended.innerHTML, /8 accounting rows/);
  assert.match(appended.innerHTML, /awarded contracts as recorded/);
});

test('TED and BOSA award panel labels lot evidence and does not imply omitted amounts are zero', async () => {
  const start = html.indexOf('    // TED/BOSA award records are distinct from the Brussels accounting inventory.');
  const end = html.indexOf('    async function loadPreferences(', start);
  assert.ok(start >= 0 && end > start, 'the TED/BOSA award panel must be present');
  const block = html.slice(start, end);
  let appended;
  const panel = { className: '', set innerHTML(value) { this.html = value; }, get innerHTML() { return this.html; } };
  const context = {
    company: { appendChild: (node) => { appended = node; } },
    searchSequence: 3,
    escapeHtml: (value) => String(value ?? ''),
    document: { createElement: () => panel, querySelector: () => ({ appendChild: (node) => { appended = node; } }) },
    apiFetch: () => Promise.resolve({ ok: true, json: () => Promise.resolve({
      awards: [
        { source: 'BOSA', publication_number: 'bosa:workspace:notice', title: 'Road works',
          buyer: 'Buyer', winner_names: ['Example NV'], cpv_codes: ['45233100'], values: [],
          publication_references: ['BDA-123_pub01'],
          amount_note: 'BOSA amounts are omitted because supplier-level attribution is not reliable',
          lot_evidence: [{ lot_id: 'LOT-0001', cpv_codes: ['45233100'], participation: 'contract-linked-winner' }] },
        { source: 'TED', publication_number: '642950-2026', title: 'IT services', buyer: 'TED buyer',
          winner_names: ['Example NV'], cpv_codes: ['72000000'], values: ['584550'],
          amount_note: 'source-reported TED award value; currency is not included in this record',
          links: ['https://ted.europa.eu/en/notice/642950-2026/xml'] },
      ],
      note: 'BOSA records are listed first, then TED',
    }) }),
    Promise,
  };
  vm.runInNewContext(`${block}; loadTenderAwardHistory('0404484060', 3);`, context);
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(appended, 'the award panel should be appended');
  assert.match(appended.innerHTML, /TED and BOSA award history/);
  assert.match(appended.innerHTML, /LOT-0001/);
  assert.match(appended.innerHTML, /contract-linked-winner/);
  assert.match(appended.innerHTML, /amounts are omitted/);
  assert.match(appended.innerHTML, /584550/);
  assert.match(appended.innerHTML, /currency is not included/);
  assert.match(appended.innerHTML, /BDA-123_pub01/);
});

test('award history panel stays away when the company has no records', async () => {
  const start = html.indexOf('    // A company\'s own awarded contracts as recorded in the Brussels inventories.');
  const end = html.indexOf('    async function loadPreferences(', start);
  const block = html.slice(start, end);
  let appended = false;
  const context = {
    company: { appendChild: () => { appended = true; } },
    searchSequence: 3,
    escapeHtml: (value) => String(value ?? ''),
    document: {
      createElement: () => ({ set innerHTML(v) { this.html = v; } }),
      querySelector: () => ({ appendChild: () => { appended = true; } }),
    },
    apiFetch: () => Promise.resolve({ ok: true, json: () => Promise.resolve({ engagements: [], accounting_rows: 0, note: 'none' }) }),
    Promise,
  };
  vm.runInNewContext(`${block}; loadAwardHistory('0404484060', 3);`, context);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(appended, false, 'an empty history must not add a panel');
});

test('health line reports recognition snapshot provenance when available', () => {
  const configured = renderHealth({
    tenders: 2,
    tenders_open_procurements: 1,
    tenders_last_fetched: '2026-10-31',
    recognition_register: {
      approval_rows: 11870,
      register_as_of: '2026-10-31',
      kbo_snapshot_as_of: '2026-06-24',
    },
  });
  assert.match(configured, /recognition register 11870 approvals as of 2026-10-31/);
  assert.match(configured, /KBO join snapshot 2026-06-24/);
  const absent = renderHealth({ tenders: 0, recognition_register: null });
  assert.match(absent, /no recognition register snapshot loaded/);
});

test('health line reports the publication range the store holds', () => {
  const text = renderHealth({
    tenders: 3117,
    tenders_oldest_publication_date: '2024-08-01',
    tenders_newest_publication_date: '2026-10-16',
    tenders_publication_unknown_rows: 0,
    tenders_last_fetched: '2026-10-18 21:45:56+00',
  });
  assert.match(text, /3117 tenders loaded/);
  assert.match(text, /publications 2024-08-01 to 2026-10-16/);
  assert.match(text, /last refresh 2026-10-18/);
  assert.doesNotMatch(text, /older rows/);
});

test('health line counts rows that predate the publication-date column', () => {
  const text = renderHealth({
    tenders: 3117,
    tenders_oldest_publication_date: '2024-08-01',
    tenders_newest_publication_date: '2026-10-16',
    tenders_publication_unknown_rows: 12,
    tenders_last_fetched: '2026-10-18 21:45:56+00',
  });
  assert.match(text, /\(12 older rows without a date\)/);
});

test('health line leads with open procurements and flags drift', () => {
  const fresh = renderHealth({
    tenders: 6035,
    tenders_open_notices: 1340,
    tenders_open_procurements: 1015,
    tenders_freshness: 'fresh',
    tenders_oldest_publication_date: '2026-04-22',
    tenders_newest_publication_date: '2026-10-18',
    tenders_publication_unknown_rows: 0,
    tenders_last_fetched: '2026-10-18 06:00:00+00',
    award_history_companies: 0,
  });
  assert.match(fresh, /^1015 open procurements/);
  assert.doesNotMatch(fresh, /stale|unverified/);
  // 15% of the score is the history term, which scores neutrally when absent.
  assert.match(fresh, /no award history/);
  const stale = renderHealth({
    tenders: 2889,
    tenders_open_notices: 23,
    tenders_open_procurements: 14,
    tenders_freshness: 'stale',
    tenders_oldest_publication_date: null,
    tenders_newest_publication_date: null,
    tenders_publication_unknown_rows: 2889,
    tenders_last_fetched: '2026-10-18 06:00:00+00',
    award_history_companies: 3,
  });
  assert.match(stale, /^14 open procurements/);
  assert.match(stale, /Data is stale/);
  assert.match(stale, /award history for 3 companies/);
  const unknown = renderHealth({
    tenders: 6035,
    tenders_open_notices: 1340,
    tenders_open_procurements: 1015,
    tenders_freshness: 'unknown',
    tenders_oldest_publication_date: null,
    tenders_newest_publication_date: null,
    tenders_publication_unknown_rows: 6035,
    tenders_last_fetched: '2026-10-18 06:00:00+00',
  });
  assert.match(unknown, /Freshness unverified/);
});

test('health line names the population the last run drew from', () => {
  const text = renderHealth({
    tenders: 3117,
    tenders_oldest_publication_date: '2024-08-01',
    tenders_newest_publication_date: '2026-10-16',
    tenders_publication_unknown_rows: 0,
    tenders_last_fetched: '2026-10-18 21:45:56+00',
    last_refresh: {
      scope: 'ACTIVE', window: 'days=30', fetched: 1877, total_notice_count: 19093,
    },
  });
  assert.match(text, /last run ACTIVE days=30 \(1877 fetched of 19093\)/);
  assert.doesNotMatch(text, /evidence as of/);
  const backfill = renderHealth({
    tenders: 2889,
    tenders_last_fetched: '2024-09-19 06:00:00+00',
    last_refresh: {scope: 'ALL', window: '2024-08-01..2024-08-31', fetched: 1240},
  });
  assert.match(backfill, /last run ALL 2024-08-01\.\.2024-08-31 \(1240 fetched\)/);
  assert.doesNotMatch(backfill, /evidence as of/);
});

test('a run that stopped short reads as partial, not as a small complete day', () => {
  // `refresh-partial` means the watermark stayed put and the window was not
  // covered. Without the word, `(5 fetched)` looks like a quiet day of TED news.
  const partial = renderHealth({
    tenders: 3117,
    tenders_last_fetched: '2026-10-18 21:45:56+00',
    last_refresh: {kind: 'refresh-partial', scope: 'ACTIVE', window: 'days=1', fetched: 5},
  });
  assert.match(partial, /last run ACTIVE days=1 \(5 fetched, partial\)/);
  const complete = renderHealth({
    tenders: 3117,
    tenders_last_fetched: '2026-10-18 21:45:56+00',
    last_refresh: {kind: 'refresh', scope: 'ACTIVE', window: 'days=30', fetched: 1877, total_notice_count: 1877},
  });
  assert.match(complete, /last run ACTIVE days=30 \(1877 fetched of 1877\)/);
  assert.doesNotMatch(complete, /partial/);
});

test('health line shows the stored evidence mix as of the last run', () => {
  const text = renderHealth({
    tenders: 3117,
    tenders_oldest_publication_date: '2024-08-01',
    tenders_newest_publication_date: '2026-10-16',
    tenders_publication_unknown_rows: 0,
    tenders_last_fetched: '2026-10-18 21:45:56+00',
    last_refresh: {
      scope: 'ACTIVE', window: 'days=30', fetched: 1877, total_notice_count: 19093,
      evidence: {opportunities: 1877, stated: 300, hinted: 20, unknown: 1557},
    },
  });
  assert.match(text, /evidence as of last run: 300 stated, 20 hinted, 1557 unknown of 1877 opportunities/);
});

// The judging helpers must stay pure: the payload omits what the judge left
// unset (server falls back to the topic flag), and the label states exactly
// what was recorded. Slice the real functions, not copies.
const feedbackStart = html.indexOf('    function feedbackPayload(');
const feedbackEnd = html.indexOf('    // END-FEEDBACK-HELPERS', feedbackStart);
assert.ok(feedbackStart >= 0 && feedbackEnd > feedbackStart, 'feedback helpers must be present');
const feedbackHelpers = html.slice(feedbackStart, feedbackEnd);

function feedbackFns(language = 'en') {
  const context = {t: key => uiCopy[language]?.[key] || uiCopy.en[key] || key};
  vm.runInNewContext(`${feedbackHelpers}; this.fns = {feedbackPayload, feedbackStateLabel};`, context);
  return context.fns;
}

test('feedback payload omits what the judge left unset', () => {
  // Loose equality: objects built inside the vm context carry a different
  // prototype, which deepStrictEqual rejects despite identical structure.
  // Round-trip through JSON so the comparison runs on realm-local objects.
  const {feedbackPayload} = feedbackFns();
  const payload = (...args) => JSON.parse(JSON.stringify(feedbackPayload(...args)));
  assert.deepStrictEqual(
    payload('ted', '1', true, undefined, undefined),
    {source: 'ted', external_id: '1', relevant: true}
  );
  assert.deepStrictEqual(
    payload('ted', '1', false, false, ''),
    {source: 'ted', external_id: '1', relevant: false, pursuable: false}
  );
  assert.deepStrictEqual(
    payload('bda', 'f1', true, true, 'bid'),
    {source: 'bda', external_id: 'f1', relevant: true, pursuable: true, outcome: 'bid'}
  );
});

test('feedback updates distinguish omitted fields from explicit clearing', () => {
  const {feedbackPayload} = feedbackFns();
  assert.deepEqual(JSON.parse(JSON.stringify(feedbackPayload('ted', '1', true, null, null))),
    {source:'ted', external_id:'1', relevant:true, pursuable:null, outcome:null});
});

test('card feedback sends only the changed dimension and adopts the saved state', async () => {
  const start = html.indexOf('      async function saveCardFeedback(');
  const end = html.indexOf("      results.querySelectorAll('[data-feedback]')", start);
  const block = html.slice(start, end);
  const outcomeSelect = {value: 'won'}, state = {};
  const card = {dataset:{source:'ted', id:'1', topic:'false', pursuable:'true'},
    querySelector: selector => selector === '[data-outcome]' ? outcomeSelect : state};
  let posted;
  const saved = {relevant:false, pursuable:true, outcome:'won'};
  const context = {t:key=>key, ...feedbackFns(), beginCardOperation:()=>true,
    finishCardOperation:()=>true, setFeedbackPending:()=>{},
    apiFetch: async (_url, options) => {
      posted = JSON.parse(options.body);
      Object.assign(saved, posted);
      return {ok:true, json:async()=>({status:'saved',feedback:saved})};
    }};
  vm.runInNewContext(`${block}; this.save = saveCardFeedback;`, context);
  await context.save(card, 'company', null);
  assert.deepEqual(posted, {source:'ted',external_id:'1',relevant:false});
  assert.equal(outcomeSelect.value, 'won');
  assert.equal(card.dataset.pursuable, 'true');
  card.dataset.pursuable = '';
  await context.save(card, 'company', null, 'pursuable');
  assert.deepEqual(posted, {source:'ted',external_id:'1',relevant:false,pursuable:null});
  assert.equal(outcomeSelect.value, 'won');
  outcomeSelect.value = '';
  await context.save(card, 'company', null, 'outcome');
  assert.deepEqual(posted, {source:'ted',external_id:'1',relevant:false,outcome:null});
  assert.equal(card.dataset.pursuable, '');
});

test('feedback state label states exactly what was recorded in the selected language', () => {
  const {feedbackStateLabel} = feedbackFns();
  assert.equal(feedbackStateLabel(true, undefined, undefined), 'relevant · pursuability unset');
  assert.equal(feedbackStateLabel(false, false, ''), 'not relevant · not pursuable');
  assert.equal(feedbackStateLabel(true, true, 'bid'), 'relevant · pursuable · outcome: bid');
  const dutch = feedbackFns('nl').feedbackStateLabel(false, false, 'won');
  assert.equal(dutch, 'niet relevant · niet geschikt om op in te schrijven · resultaat: gewonnen');
});

test('tender cards keep secondary actions grouped while preserving feedback and outcome controls', () => {
  // A scoping slip here fails silently in the browser: controls simply vanish.
  assert.ok(html.includes('data-i18n="reviewOfficialNotice"'), 'the official notice should be the primary action');
  assert.ok(html.includes('<details class="card-tools"><summary data-i18n="moreActions">'), 'secondary actions should start in a disclosure');
  assert.ok(html.includes('data-detail'), 'tender detail action must remain available');
  assert.ok(html.includes('data-jev'), 'structured analysis action must remain available');
  assert.ok(html.includes('data-feedback'), 'relevance feedback controls must remain available');
  assert.ok(html.includes('data-shortlist-toggle'), 'each tender should be addable to the shortlist');
  assert.ok(html.includes('data-pursuable'), 'pursuable tri-state buttons must be present');
  assert.ok(html.includes('data-outcome'), 'outcome select must be present');
  assert.ok(html.includes('pursue-row'), 'pursue row must be present');
  assert.ok(html.includes("t('chooseRelevance')"), 'topic-first guard should use a localized message');
  assert.ok(html.includes('prompt_version'), 'structured analysis must cite the prompt version');
});
test('health line admits when no stored row has a publication date', () => {
  const text = renderHealth({
    tenders: 88,
    tenders_oldest_publication_date: null,
    tenders_newest_publication_date: null,
    tenders_publication_unknown_rows: 88,
    tenders_last_fetched: null,
  });
  assert.match(text, /publication dates not recorded for the stored rows yet/);
  assert.match(text, /last refresh unknown/);
});

// Exercise the actual inline renderer with hostile API data and missing fields.
test('attachment hints are escaped, attributed and explicitly unreviewed', () => {
  const start = html.indexOf('    // BEGIN-ATTACHMENT-EVIDENCE');
  const end = html.indexOf('    // END-ATTACHMENT-EVIDENCE', start);
  assert.ok(start >= 0 && end > start);
  const block = html.slice(start, end);
  const escapeHtml = (v) => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
  const ctx = { escapeHtml };
  vm.runInNewContext(block, ctx);
  const rendered = ctx.renderAttachmentEvidence({bosa_attachment_evidence: [{original_name:'<img onerror=x>', version_id:'<script>', signals:[{text:'<b>Certificat</b>',source_page:3}]}]});
  assert.match(rendered, /manual review/);
  assert.match(rendered, /not verified requirements/);
  assert.match(rendered, /do not affect the score/);
  assert.match(rendered, /page 3/);
  assert.match(rendered, /&lt;img onerror=x&gt;/);
  assert.match(rendered, /&lt;b&gt;Certificat&lt;\/b&gt;/);
  assert.doesNotMatch(rendered, /<script>|<img/);
  assert.equal(ctx.renderAttachmentEvidence(null), '');
  assert.equal(ctx.renderAttachmentEvidence({bosa_attachment_evidence: 'bad'}), '');
});

test('notice lot CPVs are escaped, bounded and clearly informational', () => {
  const start = html.indexOf('    // BEGIN-LOT-CPV-EVIDENCE');
  const end = html.indexOf('    // END-LOT-CPV-EVIDENCE', start);
  assert.ok(start >= 0 && end > start);
  assert.match(html, /renderNoticeLotCpvEvidence\(detail\.raw\)/, 'detail action should render the extracted CPV evidence');
  const ctx = { escapeHtml: (v) => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])) };
  vm.runInNewContext(html.slice(start, end), ctx);
  const rendered = ctx.renderNoticeLotCpvEvidence({notice_lot_cpv_evidence:[
    {lot_id:'<img onerror=x>',cpv_codes:['45221100','45221100','<script>']},
    {lot_id:'LOT-2',cpv_codes:['71300000']},
  ]});
  assert.match(rendered, /not separately scored/);
  assert.match(rendered, /informational, not a relevance or eligibility decision/);
  assert.match(rendered, /&lt;img onerror=x&gt;/);
  assert.doesNotMatch(rendered, /<script>|<img/);
  assert.equal((rendered.match(/45221100/g) || []).length, 1, 'duplicate CPVs should collapse');
  const capped = ctx.renderNoticeLotCpvEvidence({notice_lot_cpv_evidence:Array.from({length:21},(_,i)=>({lot_id:`LOT-${i}`,cpv_codes:['45221100']}))});
  assert.match(capped, /Display limited to 20 lots/);
  assert.equal((capped.match(/<li>/g) || []).length, 20);
  assert.equal(ctx.renderNoticeLotCpvEvidence(null), '');
  assert.equal(ctx.renderNoticeLotCpvEvidence({notice_lot_cpv_evidence:'bad'}), '');
});

test('lot-to-NACE alignment is escaped, bounded, and explicitly non-scoring', () => {
  const start = html.indexOf('    // BEGIN-LOT-NACE-ALIGNMENT');
  const end = html.indexOf('    // END-LOT-NACE-ALIGNMENT', start);
  assert.ok(start >= 0 && end > start);
  assert.match(html, /renderLotCpvNaceAlignment\(item\.lot_cpv_alignment\)/, 'recommendation cards should render the company-specific alignment');
  const ctx = { escapeHtml: (v) => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])) };
  vm.runInNewContext(html.slice(start, end), ctx);
  const rendered = ctx.renderLotCpvNaceAlignment({lots:[
    {lot_id:'<img onerror=x>',cpv_codes:['45221100'],overlaps:[{cpv_code:'45221100',match_level:'class',nace_codes:['4211']}]},
    {lot_id:'LOT-2',cpv_codes:['45221100'],overlaps:[{cpv_code:'45221100',match_level:'division',nace_codes:['43']}]},
    {lot_id:'LOT-3',cpv_codes:['71300000'],overlaps:[]},
    {lot_id:'LOT-4',cpv_codes:[],overlaps:[]},
  ],truncated:false});
  assert.match(rendered, /NACE class/);
  assert.match(rendered, /NACE division/);
  assert.match(rendered, /No direct overlap in the current crosswalk/);
  assert.match(rendered, /No valid lot CPV was extracted/);
  assert.match(rendered, /does not affect the score/);
  assert.match(rendered, /not a relevance or eligibility decision/);
  assert.match(rendered, /no direct overlap does not rule out a fit/);
  assert.match(rendered, /&lt;img onerror=x&gt;/);
  assert.doesNotMatch(rendered, /<img/);
  const capped = ctx.renderLotCpvNaceAlignment({lots:Array.from({length:21},(_,i)=>({lot_id:`LOT-${i}`,cpv_codes:['45221100'],overlaps:[]})),truncated:false});
  assert.match(capped, /Display limited to 20 lots/);
  assert.equal((capped.match(/<li>/g) || []).length, 20);
  assert.equal(ctx.renderLotCpvNaceAlignment(null), '');
  assert.equal(ctx.renderLotCpvNaceAlignment({lots:'bad'}), '');
});

test('framework ceiling details preserve source currency and scope without summing', () => {
  const start = html.indexOf('    // BEGIN-FRAMEWORK-MAXIMUM-EVIDENCE');
  const end = html.indexOf('    // END-FRAMEWORK-MAXIMUM-EVIDENCE', start);
  assert.ok(start >= 0 && end > start);
  assert.match(html, /renderFrameworkMaximumEvidence\(detail\.raw\)/, 'detail action should render the source ceiling evidence');
  const ctx = { escapeHtml: (v) => String(v ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])) };
  vm.runInNewContext(html.slice(start, end), ctx);
  const rendered = ctx.renderFrameworkMaximumEvidence({framework_maximum_evidence:[
    {amount:'2600000.00',currency_id:'EUR',scope:'notice',lot_id:null},
    {amount:'2600000.00',currency_id:'EUR',scope:'lot',lot_id:'<img onerror=x>'},
  ]});
  assert.match(rendered, /Notice-wide: 2600000\.00 EUR/);
  assert.match(rendered, /Lot &lt;img onerror=x&gt;: 2600000\.00 EUR/);
  assert.match(rendered, /not converted/);
  assert.match(rendered, /retained separately; do not sum/);
  assert.doesNotMatch(rendered, /<img/);
  const capped = ctx.renderFrameworkMaximumEvidence({framework_maximum_evidence:Array.from({length:21},(_,i)=>({amount:'1',scope:'lot',lot_id:`LOT-${i}`}))});
  assert.match(capped, /Display limited to 20 declarations/);
  assert.equal((capped.match(/<li>/g) || []).length, 20);
  assert.equal(ctx.renderFrameworkMaximumEvidence(null), '');
  assert.equal(ctx.renderFrameworkMaximumEvidence({framework_maximum_evidence:'bad'}), '');
});

test('dashboard flow enforces legal transitions and rejects stale generations', () => {
  const start = html.indexOf('    const dashboardFlow = { sequence: 0, status: \'idle\' };');
  const end = html.indexOf('    let activeSort =', start);
  assert.ok(start >= 0 && end > start, 'dashboard transition model must be present');
  const context = {};
  vm.runInNewContext(`let searchSequence = 0; ${html.slice(start, end)}; this.flow = {dashboardFlow, beginDashboardFlow, transitionDashboardFlow};`, context);
  const sequence = context.flow.beginDashboardFlow('loading-company');
  assert.equal(context.flow.transitionDashboardFlow(sequence, 'ready'), false, 'cannot skip required phases');
  assert.equal(context.flow.transitionDashboardFlow(sequence, 'loading-preferences'), true);
  const newer = context.flow.beginDashboardFlow('loading-company');
  assert.equal(context.flow.transitionDashboardFlow(sequence, 'loading-recommendations'), false, 'older flow cannot advance');
  assert.equal(context.flow.transitionDashboardFlow(newer, 'loading-preferences'), true);
});

test('preference loading fails the company flow instead of silently skipping errors', async () => {
  const start = html.indexOf('    async function loadPreferences(enterpriseNumber, sequence) {');
  const end = html.indexOf('    function splitValues(', start);
  assert.ok(start >= 0 && end > start, 'preference loader must be present');
  const block = html.slice(start, end);
  const context = {
    searchSequence: 4,
    apiFetch: async () => ({ok: false, status: 401}),
    apiErrorText: async () => 'This server requires an API token: paste it above (or add #token=... to the URL).',
  };
  vm.runInNewContext(`${block}; this.load = loadPreferences;`, context);
  await assert.rejects(context.load('0404.484.060', 4), error =>
    error.message === 'This server requires an API token: paste it above (or add #token=... to the URL).');
  assert.equal(await context.load('0404.484.060', 3), false, 'stale preference responses are inert');
});

test('card workflows reject duplicate work and invalid terminal states', () => {
  const start = html.indexOf('    const cardOperations = new WeakMap();');
  const end = html.indexOf('    function setFeedbackPending(', start);
  assert.ok(start >= 0 && end > start, 'card operation state machine must be present');
  const context = {};
  vm.runInNewContext(`${html.slice(start, end)}; this.ops = {beginCardOperation, finishCardOperation};`, context);
  const card = {};
  assert.equal(context.ops.beginCardOperation(card, 'analysis'), true);
  assert.equal(context.ops.beginCardOperation(card, 'analysis'), false, 'duplicate analysis must not start');
  assert.equal(context.ops.finishCardOperation(card, 'analysis', 'loading'), false, 'loading is not a terminal state');
  assert.equal(context.ops.finishCardOperation(card, 'analysis', 'succeeded'), true);
  assert.equal(context.ops.beginCardOperation(card, 'feedback'), true);
  assert.equal(context.ops.beginCardOperation(card, 'feedback'), false, 'feedback saves are serialized');
  assert.equal(context.ops.finishCardOperation(card, 'feedback', 'failed'), true);
  assert.equal(context.ops.beginCardOperation(card, 'feedback'), true, 'failed saves can be retried');
  assert.equal(context.ops.finishCardOperation(card, 'feedback', 'succeeded'), true);
  assert.equal(context.ops.beginCardOperation(card, 'unknown'), false, 'unknown operations are rejected');
});

// Exercise the actual optional-browser adapters without launching Chrome.
const browserSmokeSource = fs.readFileSync(path.join(__dirname, 'dashboard-browser-smoke.cjs'), 'utf8');
function browserHelper(from, to) {
  const start = browserSmokeSource.indexOf(from);
  const end = browserSmokeSource.indexOf(to, start);
  assert.ok(start >= 0 && end > start, 'browser helper boundaries must be present');
  return browserSmokeSource.slice(start, end);
}

test('browser keyboard adapter sends Enter text for native button activation', async () => {
  const packets = [];
  const context = {call: async (method, params) => packets.push({method, params})};
  vm.runInNewContext(`${browserHelper('    const press = async ', '    // END-BROWSER-KEY-HELPER')}; this.press = press;`, context);
  await context.press('Enter', 'Enter', 13);
  assert.equal(packets.length, 2);
  assert.equal(packets[0].method, 'Input.dispatchKeyEvent');
  assert.equal(packets[0].params.type, 'keyDown');
  assert.equal(packets[0].params.text, '\r');
  assert.equal(packets[0].params.unmodifiedText, '\r');
  assert.equal(packets[1].params.type, 'keyUp');
  packets.length = 0;
  await context.press('ArrowDown', 'ArrowDown', 40);
  assert.equal(packets[0].params.key, 'ArrowDown');
  assert.equal(packets[0].params.text, undefined, 'navigation keys must not insert text');
});

test('browser readiness serializes booleans, not React-owned DOM graphs', async () => {
  const element = {}; element.self = element;
  const page = {document:{querySelector:()=>element}};
  const context = {
    evaluate: async expression => JSON.parse(JSON.stringify(vm.runInNewContext(expression, page))),
    until: async check => check(),
  };
  const line = browserHelper('    const waitFor = ', '    // A matching old DOM');
  vm.runInNewContext(`${line}; this.wait = waitFor;`, context);
  assert.equal(await context.wait("document.querySelector('.react-node')", 'readiness'), true);
});

test('browser navigation refuses a ready old document until a new document commits', async () => {
  for (const method of ['Page.navigate', 'Page.reload']) {
    let page = vm.createContext({document:{readyState:'complete'}});
    let polls = 0, called = false;
    const context = {
      evaluate: async expression => vm.runInContext(expression, page),
      call: async actual => { assert.equal(actual, method); called = true; },
      until: async check => {
        assert.equal(called, true, 'readiness follows navigation dispatch');
        assert.equal(await check(), false, 'the old completed DOM is not the new document');
        polls++;
        page = vm.createContext({document:{readyState:'loading'}});
        assert.equal(await check(), false, 'a new but loading document is not ready');
        polls++;
        page.document.readyState = 'complete';
        assert.equal(await check(), true);
        polls++;
      },
    };
    vm.runInNewContext(`${browserHelper('    const nextDocument = ', '    // END-BROWSER-DOCUMENT-HELPERS')}; this.next = nextDocument;`, context);
    await context.next(method, {});
    assert.equal(polls, 3);
  }
});

test('browser navigation tolerates destroyed contexts without swallowing application errors', async () => {
  const helper = browserHelper('    const nextDocument = ', '    // END-BROWSER-DOCUMENT-HELPERS');
  let errorMessage = 'Execution context was destroyed';
  const context = {
    evaluate: async expression => {
      if (expression.includes('= true')) return;
      throw new Error(errorMessage);
    },
    call: async () => {},
    until: async check => {
      assert.equal(await check(), false, 'navigation can briefly destroy the default context');
      errorMessage = 'Unexpected fixture application error';
      await assert.rejects(check(), /Unexpected fixture application error/);
    },
  };
  vm.runInNewContext(`${helper}; this.next = nextDocument;`, context);
  await context.next('Page.reload', {});
});
