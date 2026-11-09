// Optional headless-Chrome smoke for the dashboard. Uses a fresh profile and a
// local synthetic API; no real company data, credentials, or external requests.
const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const http = require('node:http');
const { spawn } = require('node:child_process');
const assert = require('node:assert/strict');

const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
async function until(check, label, timeout = 10000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const result = await check();
    if (result) return result;
    await sleep(40);
  }
  throw new Error(`Timed out waiting for ${label}`);
}

class Cdp {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 0;
    this.pending = new Map();
    socket.addEventListener('message', event => {
      const packet = JSON.parse(event.data);
      if (!packet.id) return;
      const request = this.pending.get(packet.id);
      if (!request) return;
      this.pending.delete(packet.id);
      clearTimeout(request.timer);
      packet.error ? request.reject(new Error(packet.error.message)) : request.resolve(packet.result);
    });
    socket.addEventListener('close', () => {
      for (const request of this.pending.values()) {
        clearTimeout(request.timer);
        request.reject(new Error('Chrome DevTools connection closed'));
      }
      this.pending.clear();
    });
  }
  send(method, params = {}, sessionId) {
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Chrome DevTools timed out: ${method}`));
      }, 8000);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }
  static async connect(url) {
    const socket = new WebSocket(url);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('Chrome DevTools connection timed out')), 8000);
      socket.addEventListener('open', () => { clearTimeout(timer); resolve(); }, { once: true });
      socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Chrome DevTools connection failed')); }, { once: true });
    });
    return new Cdp(socket);
  }
}

const profile = {
  enterprise_number: '0404.484.060', status: 'AC', names: ['Voorbeeld Farma Groothandel'],
  activities: [{ code: '46460', classification: 'MAIN', dutch: 'Groothandel in farmaceutische producten' }],
};
const preferences = {
  confirmed_activities: ['Groothandel in farmaceutische producten'], excluded_terms: [], preferred_regions: ['Leuven'],
  languages: ['nl'], minimum_value: null, maximum_value: null, recognition_class: null, recognition_categories: [],
};
const tender = {
  source: 'SMOKE', external_id: 'SMOKE-001', title: 'Supply of pharmaceutical products for local services',
  source_url: 'https://example.test/notices/SMOKE-001', score: 84.5, confidence: 0.78,
  buyer: 'City of Leuven', deadline: '2026-12-18', estimated_value: '48000',
  regions: ['Vlaams-Brabant'], cpv_codes: ['33600000'], languages: ['nl'],
  reasons: ['Tender activity matches registered company activities', 'Tender language matches preferred languages'], warnings: [], requirement_signals: [], requirements: [],
};
const comparisonTender = {
  ...tender, external_id: 'SMOKE-002', title: 'Provision of secure cloud hosting services',
  source_url: 'https://example.test/notices/SMOKE-002', buyer: 'Province of Antwerp',
  deadline: '2027-01-10', estimated_value: '125000', score: 72, confidence: 0.62,
};
const health = {
  status: 'ok', tenders: 2, tenders_open_procurements: 2, tenders_freshness: 'fresh',
  tenders_oldest_publication_date: '2026-11-01', tenders_newest_publication_date: '2026-11-01',
  tenders_publication_unknown_rows: 0, tenders_last_fetched: '2026-11-01T00:00:00Z',
  award_history_companies: 1, recognition_register: null,
};

function json(response, status, payload) {
  response.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  response.end(JSON.stringify(payload));
}
const fixtureState = {preferenceWrites: [], detailRequests: 0, feedbackWrites: [], feedbackByKey: new Map(), failRecommendations: false, noticeOverrides:{}, apiRequests:[]};

function createServer() {
  const webRoot = path.resolve(__dirname, '../web');
  return http.createServer((request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    const pathname = decodeURIComponent(url.pathname);
    if (/^\/(?:companies|tenders|health|reference)(?:\/|$)/.test(pathname)) fixtureState.apiRequests.push(pathname);
    if (request.method === 'GET' && pathname === '/health') return json(response, 200, health);
    if (request.method === 'GET' && pathname === '/reference/recognition') {
      return json(response, 200, { categories: [], classes: [] });
    }
    if (/^\/tenders\/SMOKE\//.test(pathname)) {
      fixtureState.detailRequests++;
      const id=pathname.split('/').at(-1);
      const item=id === tender.external_id ? tender : comparisonTender;
      return json(response, 200, {...item, raw:{notice_version:'01'}, description:'Synthetic retained notice text for manual review.', requirements:[], ...fixtureState.noticeOverrides[id]});
    }
    const companyRoute = pathname.match(/^\/companies\/([^/]+)(?:\/(.*))?$/);
    if (companyRoute) {
      const number = companyRoute[1];
      const route = companyRoute[2] || '';
      if (route === '' && number === 'html-error') {
        response.writeHead(404, { 'content-type': 'text/html; charset=utf-8' });
        return response.end('<!doctype html><html><body><h1>File not found</h1></body></html>');
      }
      if (route === '' && ![profile.enterprise_number, '0000.000.002'].includes(number)) return json(response, 404, { detail: 'company not found' });
      if (route === '') return json(response, 200, {...profile, enterprise_number: number, names: number === profile.enterprise_number ? profile.names : ['Second synthetic company']});
      if (route === 'preferences' && request.method === 'PUT') {
        let body = '';
        request.on('data', chunk => {body += chunk;});
        request.on('end', () => {
          const payload = JSON.parse(body);
          fixtureState.preferenceWrites.push({number, payload});
          Object.assign(preferences, payload);
          json(response, 200, preferences);
        });
        return;
      }
      if (route === 'feedback' && request.method === 'POST') {
        let body = '';
        request.on('data', chunk => {body += chunk;});
        request.on('end', () => {
          const payload = JSON.parse(body), key = JSON.stringify([number,payload.source,payload.external_id]);
          fixtureState.feedbackWrites.push({number,payload});
          const feedback = {...(fixtureState.feedbackByKey.get(key) || {pursuable:null,outcome:null,reason:null}), ...payload};
          fixtureState.feedbackByKey.set(key,feedback);
          json(response,200,{status:'saved',feedback});
        });
        return;
      }
      if (route === 'preferences' && request.method === 'GET') return json(response, 200, preferences);
      if (route === 'inventory-awards') return json(response, 200, { engagements: [], accounting_rows: 0, note: 'synthetic fixture' });
      if (route === 'awards') return json(response, 200, { awards: [], note: 'synthetic fixture' });
      if (route === 'recommendations' && fixtureState.failRecommendations) return json(response,500,{detail:'synthetic failure'});
      if (route === 'recommendations') return json(response, 200, url.searchParams.get('due_within') === '7' ? [] : [tender, comparisonTender].map(item => ({...item,saved_feedback:fixtureState.feedbackByKey.get(JSON.stringify([number,item.source,item.external_id])) || null})));
      return json(response, 404, { detail: 'route not found' });
    }

    const relative = pathname === '/' ? 'index.html' : pathname.slice(1);
    const filename = path.resolve(webRoot, relative);
    if (filename !== webRoot && !filename.startsWith(`${webRoot}${path.sep}`)) {
      response.writeHead(403).end('Forbidden');
      return;
    }
    let file = filename;
    try { if (fs.statSync(file).isDirectory()) file = path.join(file, 'index.html'); }
    catch { /* handled by the read below */ }
    fs.readFile(file, (error, bytes) => {
      if (error) { response.writeHead(404, { 'content-type': 'text/plain; charset=utf-8' }).end('Not found'); return; }
      const ext = path.extname(file);
      const types = { '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.svg': 'image/svg+xml', '.woff2': 'font/woff2', '.woff': 'font/woff' };
      response.writeHead(200, { 'content-type': types[ext] || 'application/octet-stream' });
      response.end(bytes);
    });
  });
}

async function browserSmoke() {
  assert.equal(typeof WebSocket, 'function', 'Node 22+ with WebSocket is required');
  const chrome = process.env.CHROME_BIN || [
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/google-chrome',
  ].find(fs.existsSync);
  assert.ok(chrome && fs.existsSync(chrome), 'Set CHROME_BIN to an installed Chrome/Chromium binary');
  const server = createServer();
  await new Promise((resolve, reject) => server.listen(0, '127.0.0.1', error => error ? reject(error) : resolve()));
  const address = server.address();
  const origin = `http://127.0.0.1:${address.port}`;
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'aanbestedingen-browser-smoke-'));
  const userData = path.join(temp, 'profile');
  fs.mkdirSync(userData, { mode: 0o700 });
  const child = spawn(chrome, [
    '--headless=new', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0',
    `--user-data-dir=${userData}`, '--no-first-run', '--no-default-browser-check', '--disable-extensions',
    '--disable-background-networking', '--disable-component-update', '--disable-default-apps', '--disable-sync',
    '--disable-breakpad', '--disable-crash-reporter', '--metrics-recording-only', '--use-mock-keychain',
    '--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE localhost, EXCLUDE 127.0.0.1', 'about:blank',
  ], { stdio: 'ignore', env: { PATH: process.env.PATH || '/usr/bin:/bin', HOME: userData, TMPDIR: os.tmpdir() } });
  let cdp;
  let spawnError;
  child.once('error', error => { spawnError = error; });
  try {
    const activeFile = path.join(userData, 'DevToolsActivePort');
    const active = await until(() => {
      if (spawnError) throw spawnError;
      if (child.exitCode !== null || child.signalCode !== null) throw new Error(`Chrome exited before startup (${child.exitCode ?? child.signalCode})`);
      return fs.existsSync(activeFile) && fs.readFileSync(activeFile, 'utf8').trim().split('\n');
    }, 'isolated Chrome startup');
    cdp = await Cdp.connect(`ws://127.0.0.1:${active[0]}${active[1]}`);
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const call = (method, params = {}) => cdp.send(method, params, sessionId);
    await call('Page.enable');
    await call('Runtime.enable');
    const evaluate = async expression => {
      const result = await call('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
      if (result.exceptionDetails) throw new Error(`${result.exceptionDetails.text}: ${result.result.description}`);
      return result.result.value;
    };
    const viewport = (width, height) => call('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false });
    const waitFor = (expression, label) => until(() => evaluate(`Boolean(${expression})`), label);
    // A matching old DOM is not evidence that a navigation has completed.
    // The marker belongs only to this document and must disappear on commit.
    const nextDocument = async (method, params) => {
      await evaluate('globalThis.__smokeNavigationPending = true');
      await call(method, params);
      await until(async () => {
        try {
          return await evaluate("!globalThis.__smokeNavigationPending && document.readyState === 'complete'");
        } catch (error) {
          if (/Execution context was destroyed|Cannot find context|Inspected target navigated/.test(error.message)) return false;
          throw error;
        }
      }, 'new document after navigation');
    };
    const openPage = url => nextDocument('Page.navigate', {url});
    const reload = () => nextDocument('Page.reload', {ignoreCache:true});
    // END-BROWSER-DOCUMENT-HELPERS
    await viewport(1536, 1000);
    await openPage(`${origin}/`);
    await waitFor("document.querySelector('.health-summary')", 'dashboard bootstrap');
    assert.equal(await evaluate("document.querySelector('.brand').textContent"), 'Tenderpad');
    await waitFor("document.querySelector('.brand-mark').complete && document.querySelector('.brand-mark').naturalWidth > 0", 'shared brand asset');
    const search = async (number = profile.enterprise_number) => {
      await evaluate(`document.querySelector('#enterprise-number').value=${JSON.stringify(number)}; document.querySelector('#lookup-form').requestSubmit()`);
      await waitFor("document.querySelectorAll('.result-item').length === 2", 'company recommendations');
    };
    const navigate = view => evaluate(`document.querySelector('[data-view="${view}"]').click()`);
    await search();
    assert.equal(await evaluate("document.querySelector('#sidebar-company-name').textContent"), profile.names[0]);
    assert.equal(await evaluate("document.querySelectorAll('article.card').length"), 2);
    assert.equal(await evaluate("document.querySelector('#selected-panel').hidden"), false, 'desktop opens first detail');
    assert.match(await evaluate("document.querySelector('#selected-content .card-head').textContent"), /Supply of pharmaceutical/);
    assert.equal(await evaluate("document.querySelector('#selected-panel').getAttribute('role')"), 'region');
    assert.match(await evaluate("document.querySelector('#results-summary').textContent"), /Recommendations shown: 2.*any deadline.*best match/);
    assert.ok(await evaluate("document.documentElement.scrollWidth <= innerWidth"), 'desktop does not overflow');
    // Server feedback survives rendering/reload; each control changes only its
    // own dimension, including explicit clearing of pursuability and outcome.
    const feedbackCard = `document.querySelector('article.card[data-id="SMOKE-001"]')`;
    const feedbackIdle = `${feedbackCard} && !${feedbackCard}.querySelector('[data-pursuable]').disabled && ${feedbackCard}.querySelector('.feedback-state').textContent.includes('Saved as')`;
    await evaluate(`${feedbackCard}.querySelector('[data-feedback][data-relevant="true"]').click()`);
    await waitFor(feedbackIdle, 'relevance feedback save');
    await evaluate(`${feedbackCard}.querySelector('[data-pursuable="true"]').click()`);
    await waitFor(feedbackIdle, 'pursuability feedback save');
    await evaluate(`${feedbackCard}.querySelector('[data-outcome]').value='won'; ${feedbackCard}.querySelector('[data-outcome]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(feedbackIdle, 'outcome feedback save');
    await reload();
    await waitFor(`${feedbackCard}?.querySelector('[data-outcome]').value === 'won'`, 'saved feedback reload');
    assert.equal(await evaluate(`${feedbackCard}.dataset.pursuable`),'true');
    await evaluate(`${feedbackCard}.querySelector('[data-feedback][data-relevant="false"]').click()`);
    await waitFor(feedbackIdle, 'relevance-only feedback correction');
    assert.equal(await evaluate(`${feedbackCard}.querySelector('[data-outcome]').value`),'won');
    assert.equal(await evaluate(`${feedbackCard}.dataset.pursuable`),'true');
    assert.deepEqual(fixtureState.feedbackWrites.at(-1).payload,{source:'SMOKE',external_id:'SMOKE-001',relevant:false});
    await evaluate(`${feedbackCard}.querySelector('[data-pursuable=""]').click()`);
    await waitFor(feedbackIdle, 'explicit pursuability clear');
    assert.equal(fixtureState.feedbackWrites.at(-1).payload.pursuable,null);
    assert.equal(await evaluate(`${feedbackCard}.querySelector('[data-outcome]').value`),'won');
    await evaluate(`${feedbackCard}.querySelector('[data-outcome]').value=''; ${feedbackCard}.querySelector('[data-outcome]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(feedbackIdle, 'explicit outcome clear');
    assert.equal(fixtureState.feedbackWrites.at(-1).payload.outcome,null);
    await reload();
    await waitFor(`${feedbackCard} && ${feedbackCard}.dataset.topic === 'false'`, 'cleared feedback reload');
    assert.equal(await evaluate(`${feedbackCard}.dataset.pursuable`),'');
    assert.equal(await evaluate(`${feedbackCard}.querySelector('[data-outcome]').value`),'');
    console.log('Feedback save/reload/relevance-only update/explicit-clear browser regression passed');
    await evaluate("document.querySelector('.result-select').focus()");
    const press = async (key,code,virtual) => {
      // Chrome needs Enter's text to synthesize keypress/native button activation.
      await call('Input.dispatchKeyEvent',{type:'keyDown',key,code,windowsVirtualKeyCode:virtual,...(key === 'Enter' ? {text:'\r',unmodifiedText:'\r'} : {})});
      await call('Input.dispatchKeyEvent',{type:'keyUp',key,code,windowsVirtualKeyCode:virtual});
    };
    // END-BROWSER-KEY-HELPER
    await press('ArrowDown','ArrowDown',40);
    assert.equal(await evaluate("document.activeElement === document.querySelectorAll('.result-select')[1]"),true);
    await press('s','KeyS',83);
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"),'1');
    await press('s','KeyS',83);
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"),'0');
    await press('Enter','Enter',13);
    assert.match(await evaluate("document.querySelector('#selected-content .card-head').textContent"), /secure cloud hosting/);
    await press('Escape','Escape',27);
    assert.equal(await evaluate("document.activeElement === document.querySelectorAll('.result-select')[1]"),true);
    await evaluate("document.querySelector('.result-select').click()");
    // The source action opens visible evidence, including when reached through More actions.
    await evaluate("document.querySelector('#selected-content [data-detail]').click()");
    await waitFor("document.querySelector('#selected-content .detail-result').textContent.includes('Synthetic retained notice')", 'retained evidence');
    assert.equal(await evaluate("document.querySelector('#selected-panel').dataset.detailView"), 'evidence');
    await evaluate("document.querySelector('#tab-overview').click(); document.querySelector('#tab-evidence').click()");
    assert.equal(fixtureState.detailRequests, 1, 'loaded evidence is reused');
    await evaluate("document.querySelector('#close-selected').click(); document.querySelectorAll('.result-select')[1].click()");
    assert.match(await evaluate("document.querySelector('#selected-content .card-head').textContent"), /secure cloud hosting/);
    await reload();
    await waitFor("document.querySelector('#selected-content .card')?.dataset.id === 'SMOKE-002'", 'restored selected tender by identity');
    await evaluate("document.querySelector('#close-selected').click()");
    assert.equal(await evaluate("document.activeElement === document.querySelectorAll('.result-select')[1]"), true);
    // Both list and detail save controls refer to the same company-scoped shortlist.
    await evaluate("document.querySelectorAll('.result-meta [data-shortlist-toggle]')[0].click(); document.querySelectorAll('.result-meta [data-shortlist-toggle]')[1].click()");
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"), '2');
    await navigate('shortlist');
    assert.equal(await evaluate("document.querySelectorAll('#shortlist-content thead th').length"), 3);
    assert.match(await evaluate("document.querySelector('#shortlist-content').textContent"), /City of Leuven[\s\S]*Province of Antwerp/);
    await evaluate("document.querySelector('[data-shortlist-remove]').click()");
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"), '1');
    await evaluate("document.querySelector('#undo-shortlist').click()");
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"), '2');
    await evaluate("const assessment=document.querySelector('[data-shortlist-assessment]'); assessment.value='explore'; assessment.dispatchEvent(new Event('change',{bubbles:true})); const note=document.querySelector('[data-shortlist-note]'); note.value='Check source documents'; note.dispatchEvent(new Event('change',{bubbles:true}))");
    await evaluate("document.querySelector('[data-shortlist-next-action]').value='Ask colleague about references'; document.querySelector('[data-shortlist-next-action]').dispatchEvent(new Event('change',{bubbles:true})); document.querySelector('[data-shortlist-follow-up]').value='2000-01-01'; document.querySelector('[data-shortlist-follow-up]').dispatchEvent(new Event('change',{bubbles:true}))");
    assert.match(await evaluate("document.querySelector('[data-shortlist-follow-up-state]').textContent"),/Overdue/);
    await evaluate("document.querySelector('[data-shortlist-action-done]').focus(); document.querySelector('[data-shortlist-action-done]').click()");
    assert.equal(await evaluate("document.activeElement === document.querySelector('[data-shortlist-action-done]')"),true,'completion preserves focus');
    assert.equal(await evaluate("document.querySelector('[data-shortlist-action-done]').textContent"),'Reopen action');
    await evaluate("document.querySelector('#undo-shortlist').click()");
    assert.match(await evaluate("document.querySelector('[data-shortlist-follow-up-state]').textContent"),/Overdue/);
    assert.equal(await evaluate("document.querySelector('[data-shortlist-next-action]').value"),'Ask colleague about references');
    await evaluate("document.querySelector('[data-shortlist-action-done]').click()");
    await reload();
    await waitFor("document.querySelector('.health-summary')", 'reload');
    await waitFor("document.querySelectorAll('.result-item').length === 2 && !document.querySelector('#view-shortlist').hidden", 'restored company and shortlist view without another lookup');
    assert.equal(await evaluate("document.querySelector('#enterprise-number').value"),profile.enterprise_number);
    assert.equal(await evaluate("document.querySelector('[data-shortlist-assessment]').value"), 'explore');
    assert.equal(await evaluate("document.querySelector('[data-shortlist-note]').value"), 'Check source documents');
    assert.equal(await evaluate("document.querySelector('[data-shortlist-next-action]').value"),'Ask colleague about references');
    assert.equal(await evaluate("document.querySelector('[data-shortlist-follow-up]').value"),'2000-01-01');
    assert.match(await evaluate("document.querySelector('[data-shortlist-follow-up-state]').textContent"),/Done/);
    await evaluate("document.querySelector('#check-shortlist-changes').click()");
    await waitFor("!document.querySelector('#check-shortlist-changes').disabled", 'bounded retained-notice check');
    assert.match(await evaluate("document.querySelector('#shortlist-changes-row').textContent"),/No recorded changes/);
    fixtureState.noticeOverrides['SMOKE-001']={deadline:'2020-01-01',estimated_value:'48000.01',raw:{notice_version:'02'}};
    await evaluate("document.querySelector('#check-shortlist-changes').click()");
    await waitFor("!document.querySelector('#check-shortlist-changes').disabled", 'changed saved notice check');
    assert.match(await evaluate("document.querySelector('#shortlist-changes-row').textContent"),/Recorded changes/);
    assert.match(await evaluate("document.querySelector('#shortlist-changes-row').textContent"),/Retained deadline has passed/);
    assert.equal(await evaluate("JSON.parse(localStorage.getItem('aanbestedingen.shortlist.v1:0404.484.060'))[0].deadline"),'2026-12-18','check preserves saved snapshot');
    assert.equal(await evaluate("JSON.parse(localStorage.getItem('aanbestedingen.shortlist.v1:0404.484.060'))[0].estimated_value"),'48000','API decimal string is preserved');
    fixtureState.noticeOverrides={};
    // Editable preferences are separate from the registered company record.
    await navigate('profile');
    assert.match(await evaluate("document.querySelector('#company').textContent"), /Official KBO data/);
    const original = await evaluate("document.querySelector('#confirmed-activities').value");
    await evaluate("document.querySelector('#confirmed-activities').value='changed'; document.querySelector('#confirmed-activities').dispatchEvent(new Event('input',{bubbles:true}))");
    assert.equal(await evaluate("document.querySelector('#save-preferences').disabled"), false);
    await navigate('shortlist'); await navigate('profile');
    assert.equal(await evaluate("document.querySelector('#confirmed-activities').value"), 'changed', 'navigation preserves draft');
    await evaluate("document.querySelector('#discard-preferences').click()");
    assert.equal(await evaluate("document.querySelector('#confirmed-activities').value"), original);
    await evaluate("document.querySelector('#minimum-value').value='12345.67'; document.querySelector('#minimum-value').dispatchEvent(new Event('input',{bubbles:true})); document.querySelector('#save-preferences').click(); document.querySelector('#save-preferences').click()");
    await waitFor("document.querySelector('#message').textContent.includes('saved and recommendations refreshed')", 'profile save and refresh');
    assert.equal(fixtureState.preferenceWrites.length,1,'duplicate save is suppressed');
    assert.equal(fixtureState.preferenceWrites[0].payload.minimum_value,'12345.67','monetary input stays exact');
    assert.equal(await evaluate("document.querySelector('#save-preferences').disabled"), true);
    // A persisted profile remains saved even if refreshing opportunities fails.
    fixtureState.failRecommendations = true;
    await evaluate("document.querySelector('#languages').value='nl, fr'; document.querySelector('#languages').dispatchEvent(new Event('input',{bubbles:true})); document.querySelector('#save-preferences').click()");
    await waitFor("document.querySelector('#message').textContent.includes('saved, but')", 'saved-refresh-failed');
    assert.equal(await evaluate("document.querySelector('#save-preferences').disabled"),true);
    fixtureState.failRecommendations = false;
    for (const [language,label,tagline] of [['nl','Uw beoordeling','Jouw weg naar overheidsopdrachten.'],['fr','Votre appréciation','Votre voie vers les marchés publics.'],['en','Your assessment','Your path to public contracts.']]) {
      await evaluate(`document.querySelector('#ui-language').value='${language}'; document.querySelector('#ui-language').dispatchEvent(new Event('change',{bubbles:true}))`);
      assert.equal(await evaluate("document.documentElement.lang"),language);
      assert.match(await evaluate("document.title"), /^Tenderpad · /);
      assert.equal(await evaluate("document.querySelector('#page-title').textContent"),tagline);
      assert.match(await evaluate("document.querySelector('#shortlist-content').textContent"), new RegExp(label));
      assert.equal(await evaluate("document.querySelector('#sidebar-company-name').textContent"),profile.names[0]);
    }
    await navigate('opportunities');
    await viewport(390,844);
    await waitFor("getComputedStyle(document.querySelector('.mobile-toolbar')).display === 'flex'", 'mobile layout');
    if (!await evaluate("document.querySelector('#selected-panel').hidden")) await evaluate("document.querySelector('#close-selected').click()");
    await evaluate("document.querySelector('.result-select').click()");
    assert.equal(await evaluate("document.querySelector('#selected-panel').getAttribute('aria-modal')"),'true');
    assert.equal(await evaluate("document.querySelector('.workspace-nav').closest('.site-header').inert"),true);
    await call('Input.dispatchKeyEvent',{type:'keyDown',key:'Escape',code:'Escape',windowsVirtualKeyCode:27});
    await call('Input.dispatchKeyEvent',{type:'keyUp',key:'Escape',code:'Escape',windowsVirtualKeyCode:27});
    assert.equal(await evaluate("document.querySelector('#selected-panel').hidden"),true);
    assert.equal(await evaluate("document.querySelector('.site-header').inert"),false);
    await evaluate("document.querySelector('#open-filters').click()");
    assert.equal(await evaluate("document.querySelector('#filter-dialog').open"),true);
    assert.equal(await evaluate("document.querySelector('#filter-dialog').contains(document.querySelector('#lookup-form'))"),true);
    await evaluate("document.querySelector('#deadline-window').value='30'; document.querySelector('#reset-filters').click()");
    assert.equal(await evaluate("document.querySelector('#deadline-window').value"),'');
    await evaluate("document.querySelector('#close-filters').click()");
    await waitFor("!document.querySelector('#filter-dialog').open", 'filter close');
    assert.equal(await evaluate("document.querySelector('.workspace-sidebar').contains(document.querySelector('#lookup-form'))"),true);
    await navigate('shortlist');
    await waitFor("document.querySelector('#shortlist-content').tabIndex === 0", 'keyboard comparison scroll');
    assert.ok(await evaluate("document.documentElement.scrollWidth <= innerWidth"), 'mobile comparison scroll stays inside its container');
    await evaluate("document.querySelector('#shortlist-content').focus()");
    await call('Input.dispatchKeyEvent',{type:'keyDown',key:'ArrowRight',code:'ArrowRight',windowsVirtualKeyCode:39});
    await call('Input.dispatchKeyEvent',{type:'keyUp',key:'ArrowRight',code:'ArrowRight',windowsVirtualKeyCode:39});
    assert.ok(await evaluate("document.querySelector('#shortlist-content').scrollLeft > 0"));
    await evaluate("document.querySelector('[data-shortlist-note]').focus(); document.querySelector('[data-shortlist-note]').setSelectionRange(5,5)");
    const scrollBefore=await evaluate("document.querySelector('#shortlist-content').scrollLeft");
    await call('Input.dispatchKeyEvent',{type:'keyDown',key:'Home',code:'Home',windowsVirtualKeyCode:36});
    await call('Input.dispatchKeyEvent',{type:'keyUp',key:'Home',code:'Home',windowsVirtualKeyCode:36});
    assert.equal(await evaluate("document.querySelector('#shortlist-content').scrollLeft"),scrollBefore,'editing keys do not scroll comparison');
    await evaluate("document.querySelector('[data-shortlist-remove]').focus(); document.querySelector('[data-shortlist-remove]').click()");
    assert.ok(await evaluate("document.activeElement.hasAttribute('data-shortlist-remove')"));
    await evaluate("document.querySelector('[data-shortlist-remove]').focus(); document.querySelector('[data-shortlist-remove]').click()");
    assert.equal(await evaluate("document.activeElement === document.querySelector('#shortlist-empty [data-go-view]')"),true,'last removal focuses visible empty action');
    await navigate('opportunities');
    await evaluate("document.querySelector('#open-filters').click(); document.querySelector('#deadline-window').value='7'; document.querySelector('#lookup-form').requestSubmit()");
    await waitFor("document.querySelector('.empty-state') && document.querySelector('#results-summary').dataset.count === '0'", 'empty filtered results');
    assert.equal(await evaluate("document.querySelector('#filter-dialog').open"),false);
    assert.match(await evaluate("document.querySelector('#results').textContent"), /longer closing-date window/);
    await evaluate("document.querySelector('#deadline-window').value=''");
    await search('0000.000.002');
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"),'0','another company never inherits shortlist');
    assert.equal(await evaluate("document.querySelector('#sidebar-company-name').textContent"),'Second synthetic company');
    await evaluate("document.querySelector('#enterprise-number').value='missing'; document.querySelector('#lookup-form').requestSubmit()");
    await waitFor("document.querySelector('#message').textContent.includes('not in the configured KBO register')",'missing company');
    assert.equal(await evaluate("document.body.classList.contains('company-loaded')"),false,'failed lookup leaves search accessible on mobile');
    await evaluate("document.querySelector('#enterprise-number').value='html-error'; document.querySelector('#lookup-form').requestSubmit()");
    await waitFor("document.querySelector('#message').textContent.includes('HTML page instead of API data')",'HTML API failure');
    const axTree=await call('Accessibility.getFullAXTree');
    const unnamed=axTree.nodes.filter(node=>['button','textbox','combobox','link'].includes(node.role?.value)&&!node.name?.value?.trim());
    assert.deepEqual(unnamed.map(node=>node.role.value),[], 'interactive controls have accessible names');
    // The explicit example uses the real UI, with no application API calls and
    // independent navigation, shortlist, language and credential boundaries.
    await viewport(1536,1000);
    await evaluate("localStorage.setItem('aanbestedingen_token','synthetic-real-token'); localStorage.setItem('aanbestedingen.uiLanguage','fr')");
    const realState = await evaluate("({workspace:localStorage.getItem('aanbestedingen.workspace.v1'),shortlist:localStorage.getItem('aanbestedingen.shortlist.v1:0404.484.060')})");
    const apiCount = fixtureState.apiRequests.length;
    await openPage(`${origin}/?example=1#token=synthetic-ignored-token`);
    await waitFor("document.querySelectorAll('.result-item').length === 3",'example onboarding');
    assert.equal(await evaluate("document.querySelector('#example-banner').hidden"),false);
    assert.equal(await evaluate("document.querySelector('#sidebar-company-name').textContent"),'Northline Digital');
    assert.equal(await evaluate("document.querySelector('#enterprise-number').readOnly"),true);
    assert.equal(await evaluate("document.querySelector('#api-token').value"),'');
    assert.equal(await evaluate("location.hash"),'');
    assert.match(await evaluate("document.querySelector('#selected-content .decision-summary').textContent"),/What remains unverified/);
    await evaluate("document.querySelector('#selected-content [data-review-evidence]').click()");
    await waitFor("document.querySelector('#selected-content .detail-result').textContent.includes('FICTIONAL EXAMPLE')",'example evidence');
    assert.match(await evaluate("document.querySelector('#selected-content .detail-result').textContent"),/Provide two recent examples/);
    await evaluate("document.querySelector('#selected-content [data-shortlist-toggle]').click()");
    assert.equal(await evaluate("document.querySelector('#nav-shortlist-count').textContent"),'1');
    await evaluate("document.querySelector('#tab-overview').click(); document.querySelector('#selected-content [data-review-profile]').click()");
    assert.equal(await evaluate("document.querySelector('#view-profile').hidden"),false);
    assert.match(await evaluate("document.querySelector('#company').textContent"),/Fictional company data/);
    assert.notEqual(await evaluate("getComputedStyle(document.querySelector('#preferences')).display"),'none');
    assert.equal(await evaluate("document.querySelector('#languages').disabled"),true);
    await navigate('shortlist');
    await evaluate("document.querySelector('[data-shortlist-note]').value='Demo note'; document.querySelector('[data-shortlist-note]').dispatchEvent(new Event('change',{bubbles:true}))");
    await evaluate("document.querySelector('[data-shortlist-next-action]').value='Explore example evidence'; document.querySelector('[data-shortlist-next-action]').dispatchEvent(new Event('change',{bubbles:true}))");
    await reload();
    await waitFor("document.querySelectorAll('.result-item').length === 3 && !document.querySelector('#view-shortlist').hidden",'example navigation restore');
    assert.equal(await evaluate("document.querySelector('[data-shortlist-note]').value"),'Demo note');
    assert.equal(await evaluate("document.querySelector('[data-shortlist-next-action]').value"),'Explore example evidence');
    assert.equal(await evaluate("localStorage.getItem('aanbestedingen_token')"),'synthetic-real-token');
    assert.equal(await evaluate("localStorage.getItem('aanbestedingen.uiLanguage')"),'fr');
    assert.deepEqual(await evaluate("({workspace:localStorage.getItem('aanbestedingen.workspace.v1'),shortlist:localStorage.getItem('aanbestedingen.shortlist.v1:0404.484.060')})"),realState);
    assert.equal(fixtureState.apiRequests.length,apiCount,'example never falls through to the application API');
    await viewport(390,844);
    await navigate('opportunities');
    await evaluate("document.querySelectorAll('.result-select')[2].click()");
    assert.match(await evaluate("document.querySelector('#selected-content .decision-next').textContent"),/Confirm the closing date/);
    assert.equal(await evaluate("document.querySelector('#selected-panel').getAttribute('aria-modal')"),'true');
    assert.ok(await evaluate("document.documentElement.scrollWidth <= innerWidth"),'example fits a mobile screen');
    await evaluate("document.querySelector('#close-selected').click()");
    await openPage(`${origin}/`);
    await waitFor("document.querySelector('.health-summary')",'return from example');
    assert.equal(await evaluate("document.querySelector('#example-banner').hidden"),true);
    assert.equal(await evaluate("document.querySelector('#api-token').value"),'synthetic-real-token');
    await waitFor("['ready','error'].includes(dashboardFlow.status)",'returning workspace settles');
    const beforeLanding=fixtureState.apiRequests.length;
    await viewport(1440,1000);
    await openPage(`${origin}/welcome/?lang=nl`);
    await waitFor("document.documentElement.lang === 'nl' && document.querySelector('#hero-title').textContent === 'Jouw weg naar overheidsopdrachten.'",'Dutch product page');
    await waitFor("document.querySelector('.wordmark img').complete && document.querySelector('.wordmark img').naturalWidth > 0",'landing brand asset');
    for(const lang of ['en','nl','fr']) {
      await evaluate(`document.querySelector('#landing-language').value='${lang}'; document.querySelector('#landing-language').dispatchEvent(new Event('change',{bubbles:true}))`);
      assert.equal(await evaluate('document.documentElement.lang'),lang);
      assert.equal(await evaluate("document.querySelector('[data-example-link]').getAttribute('href')"),`/?example=1&lang=${lang}`);
      assert.equal(await evaluate("document.querySelector('[data-workspace-link]').getAttribute('href')"),`/?lang=${lang}`);
      for(const width of [320,390,1440]) {
        await viewport(width,1000);
        assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'),`${lang} landing fits ${width}px`);
      }
    }
    await evaluate("document.querySelector('.questions-list summary').click()");
    assert.equal(await evaluate("document.querySelector('.questions-list details').open"),true);
    assert.equal(fixtureState.apiRequests.length,beforeLanding,'landing never calls the application API');
    assert.equal(await evaluate("localStorage.getItem('aanbestedingen_token')"),'synthetic-real-token');
    assert.equal(await evaluate("localStorage.getItem('aanbestedingen.uiLanguage')"),'fr','landing selection does not write workspace preferences');
    const landingTree=await call('Accessibility.getFullAXTree');
    assert.deepEqual(landingTree.nodes.filter(node=>['button','combobox','link'].includes(node.role?.value)&&!node.name?.value?.trim()).map(node=>node.role.value),[]);
    await evaluate("document.querySelector('[data-example-link]').click()");
    await waitFor("location.search === '?example=1&lang=fr' && document.querySelectorAll('.result-item').length === 3",'French example from product page');
    assert.equal(await evaluate('document.documentElement.lang'),'fr');
    await openPage(`${origin}/demo/index.html`);
    await waitFor("document.title === 'How matching works · Tenderpad' && document.querySelector('.demo-window')", 'wide matching demo');
    await viewport(390,844);
    await waitFor("document.querySelector('.demo-mobile-screen')", 'compact matching demo');
    assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'),'matching demo fits compact viewport');
    assert.equal(await evaluate("document.querySelector('.wordmark').textContent"), 'Tenderpad');
    console.log('Dashboard browser smoke passed: native keyboard activation, committed navigation/reload, feedback preservation/clear, wide/compact demo, selection/evidence, shortlist/follow-ups/Undo/notes/isolation, decimal preferences/discard/save errors, EN/NL/FR, mobile modal/filters/scroll, empty/API failures, example onboarding/state isolation, decision summaries and multilingual product page.');
  } finally {
    try { if (cdp) await cdp.send('Browser.close'); } catch {}
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGTERM');
      await new Promise(resolve => child.once('exit', resolve));
    }
    await new Promise(resolve => server.close(resolve));
    fs.rmSync(temp, { recursive: true, force: true });
  }
}

if (require.main === module) browserSmoke().catch(error => {
  console.error(`Dashboard browser smoke failed: ${error.stack || error.message}`);
  process.exitCode = 1;
});

module.exports = { browserSmoke };
