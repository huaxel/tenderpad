// Offline interaction regressions for the real workspace functions, with a minimal DOM.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const html = fs.readFileSync(path.join(__dirname, '../web/index.html'), 'utf8');
const workspace = fs.readFileSync(path.join(__dirname, '../web/workspace.js'), 'utf8') + '\n' + fs.readFileSync(path.join(__dirname, '../web/workspace-tools.js'), 'utf8');
const copy = vm.runInNewContext(html.slice(html.indexOf('    const uiCopy = {'), html.indexOf('    const languageSelect')) + '; uiCopy');
const escapeHtml = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c]));

function environment() {
  const elements = new Map();
  let ctx;
  class Element {
    constructor() {
      this.isConnected = true; this.dataset = {}; this.value = ''; this.hidden = false; this.disabled = false;
      this.scrollTop = 0; this.attributes = {}; this.children = []; this.handlers = {};
      const classes = new Set();
      this.classList = {add: v => classes.add(v), remove: v => classes.delete(v), contains: v => classes.has(v), toggle: (v, on) => on ? classes.add(v) : classes.delete(v)};
    }
    append(node) {
      if (node.parentElement) node.parentElement.children = node.parentElement.children.filter(v => v !== node);
      this.children.push(node); node.parentElement = this;
    }
    contains(node) { return this === node || this.children.some(child => child.contains(node)); }
    querySelectorAll() { return []; }
    setAttribute(key, value) { this.attributes[key] = value; }
    removeAttribute(key) { delete this.attributes[key]; }
    focus() { ctx.document.activeElement = this; this.focused = true; }
    addEventListener(type, handler) { this.handlers[type] = handler; }
    reportValidity() { return true; }
  }
  const element = selector => {
    if (!elements.has(selector)) elements.set(selector, new Element());
    return elements.get(selector);
  };
  const tabs = ['overview', 'evidence'].map(tab => {
    const button = element(`#tab-${tab}`); button.dataset.detailTab = tab; return button;
  });
  ctx = vm.createContext({
    exampleWorkspace:{active:false,storagePrefix:'aanbestedingen.'},
    document: {querySelector: element, querySelectorAll: selector => selector === '[data-detail-tab]' ? tabs : [], body: element('body'), activeElement: null},
    window: {scrollY:0, addEventListener() {}, scrollTo(_x,y) {ctx.window.scrollY=y;}}, requestAnimationFrame: fn => fn(),
    dashboardFlow:{status:'error'}, activeSort:'score', activeDueWithin:'', numberLocale:()=> 'en-BE', formatDate:String, matchMedia: () => ({matches: false}),
    t: key => copy.en[key] || key, escapeHtml,
    shortlist: [], shortlistEnterpriseNumber: 'company-a', activeEnterpriseNumber: 'company-a',
    shortlistIdentity: item => JSON.stringify([item.source, item.external_id]),
    renderShortlist() {}, persistShortlist: () => true, updateShortlistKeyboardAccess() {},
    uiLanguage: 'en', searchSequence: 1,
  });
  vm.runInContext(workspace, ctx);
  return {ctx, element, Element, run: code => vm.runInContext(code, ctx)};
}

test('all new static labels and accessibility names have EN/NL/FR translations', () => {
  const keys = [...html.matchAll(/data-i18n(?:-aria-label|-placeholder|-content)?="([a-zA-Z]+)"/g)].map(match => match[1]);
  for (const lang of ['en', 'nl', 'fr']) for (const key of keys) assert.ok(copy[lang][key], `${lang}:${key}`);
});

test('language switching keeps Tenderpad branding and localizes its heading, home name and page description', () => {
  const start = html.indexOf('    function applyUiLanguage(language) {');
  const languageFunction = html.slice(start, html.indexOf("    languageSelect.addEventListener('change'", start));
  const heading = {dataset:{i18n:'pageTitle'}}, home = {dataset:{i18nAriaLabel:'homeLabel'},setAttribute(key,value){this[key]=value;}}, description = {dataset:{i18nContent:'introCopy'},setAttribute(key,value){this[key]=value;}};
  const document = {documentElement:{},querySelectorAll:selector=>({'[data-i18n]':[heading],'[data-i18n-aria-label]':[home],'[data-i18n-content]':[description]}[selector] || [])};
  const ctx = vm.createContext({document,uiCopy:copy,uiLanguage:'en',languageSelect:{},updateResultsSummary(){}});
  vm.runInContext("function t(key) { return uiCopy[uiLanguage][key]; }\n" + languageFunction, ctx);
  for (const [language,tagline] of [['nl','Jouw weg naar overheidsopdrachten.'],['fr','Votre voie vers les marchés publics.'],['en','Your path to public contracts.']]) {
    ctx.language = language;
    vm.runInContext('applyUiLanguage(language)', ctx);
    assert.equal(document.documentElement.lang, language);
    assert.match(document.title, /^Tenderpad · /);
    assert.equal(heading.textContent, tagline);
    assert.match(home['aria-label'], /Tenderpad/);
    assert.equal(description.content, copy[language].introCopy);
  }
  const before = document.title;
  vm.runInContext("applyUiLanguage('unsupported')", ctx);
  assert.equal(document.title, before);
  assert.doesNotMatch(html, /Aanbestedingen/);
  assert.match(html, /class="brand-mark" src="\/tenderpad-mark\.svg"/);
  assert.ok(fs.existsSync(path.join(__dirname, '../web/tenderpad-mark.svg')));
});

test('shortlist assessments are bounded, escaped and distinct from source evidence', () => {
  const {ctx, run} = environment();
  ctx.shortlist = [{source:'<script>', external_id:'1', title:'<img onerror=x>', assessment:'invalid', note:'"><svg onload=x>'}];
  const markup = run('renderShortlistAssessments()');
  assert.match(markup, /Saved in this browser only; not source evidence/);
  assert.match(markup, /&lt;img onerror=x&gt;/);
  assert.match(markup, /&quot;&gt;&lt;svg onload=x&gt;/);
  assert.doesNotMatch(markup, /<img|<svg|<script/);
  assert.match(markup, /value="unreviewed" selected/);
  for (const value of [null, 'approved', '<script>', {}, 42]) {
    ctx.input = value; assert.equal(run('validAssessment(input)'), 'unreviewed');
  }
});

test('Undo restores a snapshot once and refuses a different company', () => {
  const {ctx, run, element} = environment();
  ctx.shortlist = [{title:'First'}];
  run('recordShortlistUndo()');
  ctx.shortlist[0].title = 'Changed'; ctx.shortlist.push({title:'Second'});
  run('undoShortlistChange()');
  assert.equal(ctx.shortlist.length, 1); assert.equal(ctx.shortlist[0].title, 'First');
  assert.equal(element('#undo-shortlist').hidden, true);
  run('undoShortlistChange()'); assert.equal(ctx.shortlist.length, 1);
  run('recordShortlistUndo()'); ctx.shortlistEnterpriseNumber = 'company-b'; ctx.shortlist = [];
  run('undoShortlistChange()'); assert.equal(ctx.shortlist.length, 0);
});

test('follow-up metadata defaults safely for old snapshots and rejects malformed local values', () => {
  const {ctx,run}=environment();
  ctx.item={};
  assert.deepEqual(plain(run('shortlistFollowUp(item)')),{next_action:'',follow_up_date:'',action_done:false});
  ctx.item={next_action:'  '+ 'a'.repeat(300),follow_up_date:'2026-02-30',action_done:'true'};
  const action=plain(run('shortlistFollowUp(item)'));
  assert.equal(action.next_action.length,240); assert.equal(action.follow_up_date,'');assert.equal(action.action_done,false);
  ctx.item={next_action:42,follow_up_date:'0000-01-01',action_done:true};
  assert.deepEqual(plain(run('shortlistFollowUp(item)')),{next_action:'',follow_up_date:'',action_done:false});
});

test('follow-up indicators use the Brussels calendar and completion overrides overdue dates', () => {
  const {ctx,run}=environment();
  ctx.now=new Date('2026-11-03T23:30:00Z'); // 4 November in Brussels.
  for (const [item,status] of [
    [{follow_up_date:'2026-11-03'},'overdue'],[{follow_up_date:'2026-11-04'},'today'],
    [{follow_up_date:'2026-11-05'},'upcoming'],[{},'unscheduled'],
    [{follow_up_date:'2026-11-03',action_done:true},'done'],
  ]) {ctx.item=item;assert.equal(run('followUpStatus(item,brusselsToday(now))'),status);}
});

test('follow-up controls escape local text, provide unique accessible names and translate all states', () => {
  const {ctx,run}=environment();
  ctx.shortlist=[{source:'<script>',external_id:'1',title:'<img onerror=x>',next_action:'"><svg onload=x>',follow_up_date:'invalid'}];
  const markup=run('renderShortlistFollowUps()');
  assert.match(markup,/&lt;img onerror=x&gt;/);assert.match(markup,/&quot;&gt;&lt;svg onload=x&gt;/);
  assert.match(markup,/maxlength="240"/);assert.match(markup,/aria-label="Follow-up date:/);
  assert.doesNotMatch(markup,/aria-pressed/,'completion is an action command; its separate status conveys state');
  assert.doesNotMatch(markup,/<img|<svg|<script|value="invalid"/);
  const keys=['nextAction','followUpDate','followUpHelp','markActionDone','reopenAction','invalidFollowUpDate',
    'followUpOverdue','followUpToday','followUpUpcoming','followUpUnscheduled','followUpDone'];
  for (const lang of ['en','nl','fr']) for (const key of keys) assert.ok(copy[lang][key],`${lang}:${key}`);
});

test('follow-up edits persist with Undo, reopen changed plans, and leave notice comparisons unchanged', () => {
  const {ctx,run}=environment();let saves=0;
  ctx.shortlist=[notice({next_action:'Check documents',follow_up_date:'2026-11-05'})];
  ctx.persistShortlist=()=>{saves++;return true;};
  const before=run('snapshotCheckKey(shortlist[0])');
  const key=JSON.stringify(['bda','fixture-folder']);ctx.key=key;
  assert.equal(run("updateShortlistFollowUp(key,'action_done',true)"),true);
  assert.equal(run('shortlist[0].action_done'),true);
  run('undoShortlistChange()');assert.equal(run('shortlistFollowUp(shortlist[0]).action_done'),false);
  run("updateShortlistFollowUp(key,'action_done',true); updateShortlistFollowUp(key,'next_action','Call colleague')");
  assert.equal(run('shortlist[0].action_done'),false);
  assert.equal(run('shortlist[0].next_action'),'Call colleague');
  run('undoShortlistChange()');assert.equal(run('shortlist[0].next_action'),'Check documents');
  assert.equal(run('shortlist[0].action_done'),true);
  assert.equal(run('snapshotCheckKey(shortlist[0])'),before);
  assert.equal(run("updateShortlistFollowUp(key,'follow_up_date','2026-02-30')"),false);
  assert.equal(run("updateShortlistFollowUp(key,'next_action','Check documents')"),false,'no-op preserves Undo');
  assert.equal(run("updateShortlistFollowUp(key,'deadline','2026-01-01')"),false,'source deadline cannot be edited');
  assert.equal(run('snapshotCheckKey(shortlist[0])'),before);assert.ok(saves>=4);
});

test('follow-up date controls reject invalid edits and stale company events without losing the saved plan', () => {
  const {ctx,run,element}=environment();
  ctx.shortlist=[notice({next_action:'Inspect requirements',follow_up_date:'2026-11-05'})];
  let handler,reported=false,error='';
  const date={value:'2026-02-30',dataset:{shortlistFollowUp:JSON.stringify(['bda','fixture-folder'])},validity:{badInput:false},
    addEventListener(_event,fn){handler=fn;},hasAttribute:()=>true,setCustomValidity(value){error=value;},reportValidity(){reported=true;}};
  element('#shortlist-content').querySelectorAll=selector=>selector==='[data-shortlist-next-action], [data-shortlist-follow-up]'?[date]:[];
  run('bindShortlistFollowUps()');handler();
  assert.equal(reported,true);assert.equal(error,copy.en.invalidFollowUpDate);
  assert.equal(ctx.shortlist[0].follow_up_date,'2026-11-05');
  date.value='';date.validity.badInput=true;handler();assert.equal(ctx.shortlist[0].follow_up_date,'2026-11-05');
  date.validity.badInput=false;date.value='2026-11-06';handler();
  assert.equal(ctx.shortlist[0].follow_up_date,'2026-11-06');assert.equal(error,'');
  ctx.shortlistEnterpriseNumber='company-b';date.value='2026-11-07';handler();
  assert.equal(ctx.shortlist[0].follow_up_date,'2026-11-06');
});

test('failed browser storage keeps the follow-up in memory and explains visit-only saving', () => {
  const {ctx,run,element}=environment();
  ctx.shortlist=[notice()];ctx.persistShortlist=()=>false;
  run(`updateShortlistFollowUp(${JSON.stringify(JSON.stringify(['bda','fixture-folder']))},'next_action','Check capacity')`);
  assert.equal(ctx.shortlist[0].next_action,'Check capacity');
  assert.equal(element('#toast-text').textContent,copy.en.shortlistNotPersisted);
  assert.equal(element('#undo-shortlist').hidden,false);
});

test('returning to Shortlist refreshes dates without replacing focused controls', () => {
  const {ctx,run,element}=environment();
  ctx.shortlist=[notice({follow_up_date:'2026-11-04'})];
  const status={dataset:{shortlistFollowUpState:JSON.stringify(['bda','fixture-folder'])},innerHTML:''};
  element('#shortlist-content').querySelectorAll=selector=>selector==='[data-shortlist-follow-up-state]'?[status]:[];
  run("brusselsToday = () => '2026-11-04'; showWorkspaceView('shortlist',false)");
  assert.match(status.innerHTML,/Due today/);
  run("brusselsToday = () => '2026-11-05'; showWorkspaceView('shortlist',false)");
  assert.match(status.innerHTML,/Overdue/);
});

test('loaded company labels survive language application and reset on a new lookup', () => {
  const {run, element, ctx} = environment();
  element('#sidebar-company-name').dataset.i18n = 'chooseCompany';
  element('#sidebar-company-meta').dataset.i18n = 'lookupHint';
  ctx.profile = {names:['Demo company'], enterprise_number:'0000000000'};
  run('workspaceCompanyLoaded(profile)');
  assert.equal(element('#sidebar-company-name').textContent, 'Demo company');
  assert.equal(element('#sidebar-company-name').dataset.i18n, undefined);
  assert.equal(element('#sidebar-company-meta').dataset.i18n, undefined);
  run('resetWorkspaceForSearch()');
  assert.equal(element('#sidebar-company-name').dataset.i18n, 'chooseCompany');
  assert.equal(ctx.activeEnterpriseNumber, '');
  assert.equal(ctx.shortlistEnterpriseNumber, '');
  assert.equal(element('#save-preferences').disabled, true);
});

test('selection moves the original card, preserves its state and restores keyboard focus', () => {
  const {run, ctx, element, Element} = environment();
  const card = new Element(), home = new Element(), trigger = new Element();
  card.dataset.topic = 'true'; home.append(card);
  Object.assign(ctx, {card, home, trigger});
  run('selectTender(card, home, trigger)');
  assert.equal(element('#selected-content').children[0], card);
  assert.equal(home.children.length, 0);
  assert.equal(element('#selected-panel').hidden, false);
  assert.equal(trigger.attributes['aria-expanded'], 'true');
  run('closeTenderSelection()');
  assert.equal(home.children[0], card);
  assert.equal(card.dataset.topic, 'true');
  assert.equal(element('#selected-content').children.length, 0);
  assert.equal(element('#selected-panel').hidden, true);
  assert.equal(ctx.document.activeElement, trigger);
});

test('Evidence reuses the detail loader and does not duplicate a pending request', () => {
  const {run, ctx, Element} = environment();
  const card = new Element(), home = new Element(), trigger = new Element();
  const detail = {disabled:false, click() { this.disabled = true; this.calls = (this.calls || 0) + 1; }};
  card.querySelector = selector => { assert.equal(selector, '[data-detail]'); return detail; };
  home.append(card); Object.assign(ctx, {card, home, trigger});
  run("selectTender(card, home, trigger); setDetailTab('evidence'); setDetailTab('evidence')");
  assert.equal(detail.calls, 1);
});

test('compact detail mode makes the background inert and releases it on close', () => {
  const {run, ctx, element, Element} = environment();
  ctx.matchMedia = () => ({matches:true});
  const card = new Element(), home = new Element(), trigger = new Element();
  home.append(card); Object.assign(ctx, {card,home,trigger});
  run('selectTender(card,home,trigger)');
  assert.equal(element('#selected-panel').attributes.role, 'dialog');
  assert.equal(element('#selected-panel').attributes['aria-modal'], 'true');
  assert.equal(element('.site-header').inert, true);
  run('closeTenderSelection()');
  assert.equal(element('.site-header').inert, false);
  assert.equal(element('#selected-panel').attributes['aria-modal'], undefined);
});

test('example profile is readable but preference controls stay disabled after loading and view changes', () => {
  const {ctx, run, element} = environment();
  ctx.exampleWorkspace.active = true;
  run('capturePreferenceBaseline()');
  element('#languages').value = 'nl, fr';
  run("updatePreferenceState(); showWorkspaceView('profile', false)");
  assert.equal(element('#languages').value, 'nl, fr');
  assert.equal(element('#languages').disabled, true);
  assert.equal(element('#save-preferences').disabled, true);
  assert.equal(element('#discard-preferences').disabled, true);
});

test('demo navigation resumes only its fictional company and never submits a second startup lookup', () => {
  const {ctx, run, element} = environment();
  ctx.exampleWorkspace = {active:true, companyNumber:'EXAMPLE', storagePrefix:'tenderpad.example.'};
  const state = {version:1, company:'EXAMPLE', sort:'deadline', dueWithin:'30', view:'shortlist', selection:null};
  let submits = 0;
  ctx.localStorage = {getItem: () => JSON.stringify(state)};
  ctx.form = {requestSubmit() { submits++; ctx.dashboardFlow.status='loading-company'; }};
  assert.ok(run(`validWorkspaceState(${JSON.stringify(state)})`));
  assert.equal(run(`validWorkspaceState(${JSON.stringify({...state,company:'0404.484.060'})})`),null);
  run('resumeWorkspace()');
  assert.equal(element('#enterprise-number').value,'EXAMPLE');
  assert.equal(element('#deadline-window').value,'30');
  // Execute the actual example DOMContentLoaded handler after the memory handler.
  let startup;
  const start = html.indexOf("    window.addEventListener('DOMContentLoaded', () => {");
  const block = html.slice(start,html.indexOf('  </script>',start));
  ctx.window.addEventListener = (_event, handler) => { startup=handler; };
  ctx.tokenField = element('#api-token');
  run(block);
  startup();
  assert.equal(submits,1,'restored search is not restarted by demo bootstrap');
  assert.equal(element('#example-banner').hidden,false);
  assert.equal(element('#enterprise-number').readOnly,true);
  assert.equal(ctx.tokenField.disabled,true);
});

test('dirty preferences remain intact across views and are locked only while saving', () => {
  const {run, element} = environment();
  element('#minimum-value').value = '12345.67';
  run('capturePreferenceBaseline()');
  assert.equal(run('preferencesDirty()'), false);
  element('#languages').value = 'nl, fr'; run('updatePreferenceState()');
  assert.equal(element('#save-preferences').disabled, false);
  run("showWorkspaceView('shortlist', false)");
  assert.equal(element('#languages').value, 'nl, fr');
  run('setPreferenceSaving(true)');
  assert.equal(element('#save-preferences').disabled, true);
  assert.equal(element('#languages').disabled, true);
  run('setPreferenceSaving(false)');
  assert.equal(element('#languages').disabled, false);
  assert.equal(run('preferencesDirty()'), true);
  assert.match(html, /id="minimum-value" type="number" min="0" step="any"/);
});

test('shortlist scroll keys do not intercept editing inside notes or assessments', () => {
  const at = html.indexOf("    shortlistContent.addEventListener('keydown'");
  const block = html.slice(at, html.indexOf('    const shortlistLimit', at));
  let handler;
  const region = {scrollWidth:900, clientWidth:300, scrollLeft:80, addEventListener:(_name, fn) => {handler = fn;}};
  vm.runInNewContext(block, {shortlistContent:region});
  let prevented = false;
  handler({key:'Home',target:{},preventDefault() {prevented=true;}});
  assert.equal(region.scrollLeft,80); assert.equal(prevented,false);
  handler({key:'Home',target:region,preventDefault() {prevented=true;}});
  assert.equal(region.scrollLeft,0); assert.equal(prevented,true);
});

function preferenceHandler(env) {
  const at = html.indexOf("    document.querySelector('#save-preferences').addEventListener('click', async () => {");
  const block = html.slice(at, html.indexOf('    // Pure judging helpers', at));
  Object.assign(env.ctx, {
    activeSort:'score', activeDueWithin:'', message:env.element('#message'),
    splitValues: value => value.split(',').map(v => v.trim()).filter(Boolean),
    beginDashboardFlow:() => ++env.ctx.searchSequence,
    transitionDashboardFlow:() => true,
    renderResults() {}, apiErrorText:async () => 'Rejected preference values',
  });
  vm.runInContext(block, env.ctx);
  env.run('capturePreferenceBaseline()');
  env.element('#minimum-value').value = '12345.67';
  return env.element('#save-preferences').handlers.click;
}

test('preference saving is serialized and sends decimal inputs unchanged', async () => {
  const env=environment(); const handler=preferenceHandler(env);
  let resolve, calls=0, payload;
  env.ctx.apiFetch=async (_url, options) => {
    calls++;
    if (options) {payload=JSON.parse(options.body);return new Promise(done=>{resolve=done;});}
    return {ok:true,json:async()=>[]};
  };
  const pending=handler(); await handler();
  assert.equal(calls,1);
  assert.equal(payload.minimum_value,'12345.67');
  assert.equal(env.element('#minimum-value').disabled,true);
  resolve({ok:true}); await pending;
  assert.equal(calls,2);
  assert.equal(env.run('preferencesDirty()'),false);
  assert.equal(env.element('#minimum-value').disabled,false);
});

test('failed preference save preserves edits; failed refresh preserves the saved baseline', async () => {
  const env=environment(); const handler=preferenceHandler(env);
  env.ctx.apiFetch=async()=>({ok:false,status:400});
  await handler();
  assert.equal(env.run('preferencesDirty()'),true);
  assert.equal(env.element('#save-preferences').disabled,false);
  assert.match(env.element('#message').textContent,/Rejected/);
  env.ctx.apiFetch=async (_url,options)=> options ? {ok:true} : {ok:false};
  await handler();
  assert.equal(env.run('preferencesDirty()'),false);
  assert.match(env.element('#message').textContent,/saved, but/);
});

test('fragment credentials remain usable when browser storage is unavailable', () => {
  const at=html.indexOf("    const tokenField = document.querySelector('#api-token');");
  const block=html.slice(at,html.indexOf('    async function apiFetch(',at));
  const field={value:'',addEventListener() {}};
  const url='https://app.test/#token=synthetic-token';
  let cleaned;
  vm.runInNewContext(block,{
    URL, URLSearchParams, exampleWorkspace:{active:false}, location:{href:url}, document:{querySelector:()=>field},
    localStorage:{getItem(){throw new Error('Unavailable');},setItem(){throw new Error('Unavailable');}},
    history:{replaceState(_state,_title,value){cleaned=value;}}
  });
  assert.equal(field.value,'synthetic-token');
  assert.equal(cleaned,'/');
});

test('a rejected sixth shortlist addition preserves the preceding real Undo action', () => {
  const env=environment(), {ctx,run,element}=env;
  let toggleHandler;
  const item={source:'test',external_id:'six',title:'Sixth',score:12,confidence:0.3};
  const button={dataset:{shortlistKey:JSON.stringify(['test','six'])},addEventListener(_event,fn){toggleHandler=fn;}};
  const resultElement={set innerHTML(_value){},querySelectorAll:selector=>selector==='[data-shortlist-toggle]'?[button]:[]};
  Object.assign(ctx,{
    heading:element('#heading'),results:resultElement, resultsSummary:element('#summary'),
    activeSort:'score',activeDueWithin:'',shortlistLimit:5,message:element('#message'),
    applyUiLanguage(){}, mountTenderWorkspace(){},renderLotCpvNaceAlignment:()=>'',safeNoticeUrl:()=>'',
    formatNumber:String,formatValue:String,formatDate:String,item,
  });
  ctx.shortlist=Array.from({length:4},(_,i)=>({source:'test',external_id:String(i),title:String(i)}));
  run('recordShortlistUndo()');
  ctx.shortlist.push({source:'test',external_id:'five',title:'Fifth'});
  const at=html.indexOf('    function renderResults(items, enterpriseNumber) {');
  const block=html.slice(at,html.indexOf("    form.addEventListener('submit'",at));
  vm.runInContext(block,ctx);
  run("renderResults([item], 'company-a')");
  toggleHandler();
  assert.equal(ctx.shortlist.length,5);
  assert.match(element('#message').textContent,/limit is 5/);
  run('undoShortlistChange()');
  assert.equal(ctx.shortlist.length,4);
});

const notice = (extra = {}) => ({source:'bda', external_id:'fixture-folder', title:'Digital services', buyer:'Demo authority', deadline:'2026-12-18', estimated_value:'12345.67', raw:{notice_version:'01'}, ...extra});
const plain = value => JSON.parse(JSON.stringify(value));

test('workspace memory validates and whitelists navigation state, excluding credentials and drafts', () => {
  const {ctx,run} = environment();
  ctx.input={version:1,company:'0404.484.060',sort:'deadline',dueWithin:'30',view:'shortlist',selection:{source:'bda',external_id:'fixture-folder',token:'secret'},scrollY:210,detailScroll:32,token:'secret',preferences:{languages:['private']}};
  const state=plain(run('validWorkspaceState(input)'));
  assert.equal(state.company,'0404.484.060'); assert.equal(state.scrollY,210);
  assert.equal(JSON.stringify(state).includes('secret'),false);
  assert.equal(state.preferences,undefined);
  ctx.input={...ctx.input,scrollY:-1,detailScroll:Infinity};
  assert.equal(run('validWorkspaceState(input).scrollY'),0);
  assert.equal(run('validWorkspaceState(input).detailScroll'),0);
  for (const extra of [{version:2},{company:'../../private'},{sort:'arbitrary'},{view:'admin'},{dueWithin:'365'}]) {
    ctx.invalid={...ctx.input,...extra};assert.equal(run('validWorkspaceState(invalid)'),null);
  }
});

test('workspace remembering uses applied filters and the current company; storage failure is harmless', () => {
  const {ctx,run,element}=environment();let saved;
  Object.assign(ctx,{activeEnterpriseNumber:'0404.484.060',activeSort:'deadline',activeDueWithin:'7',dashboardFlow:{status:'ready'},localStorage:{setItem(key,value){saved={key,value:JSON.parse(value)};}}});
  element('#sort-order').value='score';ctx.window.scrollY=240;
  run('rememberWorkspace()');
  assert.equal(saved.value.company,'0404.484.060');assert.equal(saved.value.sort,'deadline');assert.equal(saved.value.dueWithin,'7');assert.equal(saved.value.scrollY,240);
  ctx.dashboardFlow.status='loading-company';saved=null;run('rememberWorkspace()');assert.equal(saved,null);
  ctx.dashboardFlow.status='ready';ctx.localStorage.setItem=()=>{throw new Error('Unavailable');};
  assert.doesNotThrow(()=>run('rememberWorkspace()'));
});

test('workspace resume only submits a bounded valid state and manual searches cancel restoration', () => {
  const {ctx,run,element}=environment();let submissions=0;
  const state={version:1,company:'0404.484.060',sort:'deadline',dueWithin:'30',view:'shortlist',selection:null,scrollY:80,detailScroll:0};
  ctx.localStorage={getItem:()=>JSON.stringify(state)};
  ctx.form={requestSubmit(){submissions++;run('workspaceSearchStarted(12)');}};
  run('resumeWorkspace()');
  assert.equal(submissions,1);assert.equal(element('#enterprise-number').value,state.company);
  assert.equal(element('#deadline-window').value,'30');assert.equal(run('workspaceRestore.sequence'),12);
  run('workspaceSearchStarted(13)');assert.equal(run('workspaceRestore'),null);
  ctx.localStorage.getItem=()=> 'x'.repeat(5000);run('resumeWorkspace()');assert.equal(submissions,1);
  ctx.localStorage.getItem=()=>'{broken';run('resumeWorkspace()');assert.equal(submissions,1);
});

test('restoring a missing selection does not select a different tender or revive another company', () => {
  const {ctx,run}=environment();const selectors=ctx.document.querySelectorAll;
  ctx.document.querySelectorAll=selector=>selector==='article.card'?[]:selectors(selector);
  ctx.activeEnterpriseNumber='0404.484.060';ctx.searchSequence=4;
  ctx.saved={version:1,company:'0404.484.060',sort:'score',dueWithin:'',view:'opportunities',selection:{source:'bda',external_id:'gone'},scrollY:120,detailScroll:20,sequence:4};
  run('workspaceRestore=saved; workspaceSearchReady()');
  assert.equal(run('selectedCard'),null);assert.equal(ctx.window.scrollY,120);
  ctx.window.scrollY=0;ctx.activeEnterpriseNumber='0000.000.002';
  run('workspaceRestore=saved; workspaceSearchReady()');assert.equal(ctx.window.scrollY,0);
});

test('restoration waits for the matching successful generation and honours the last view', () => {
  const {ctx,run,element}=environment();
  ctx.activeEnterpriseNumber='0404.484.060';ctx.searchSequence=9;
  ctx.saved={company:'0404.484.060',view:'profile',selection:null,scrollY:64,detailScroll:0,sequence:9};
  run('workspaceRestore=saved; workspaceSearchReady()');
  assert.equal(run('workspaceView'),'profile');assert.equal(ctx.window.scrollY,64);
  assert.equal(element('#save-preferences').disabled,false); // No profile controls are rewritten by restoration.
  ctx.window.scrollY=0;ctx.saved.sequence=8;run('workspaceRestore=saved; workspaceSearchReady()');
  assert.equal(ctx.window.scrollY,0);
});

function keyEvent(key,target,extra={}) {
  return {key,target,defaultPrevented:false,isComposing:false,ctrlKey:false,metaKey:false,altKey:false,shiftKey:false,repeat:false,
    preventDefault(){this.defaultPrevented=true;},...extra};
}

test('result arrow keys move focus; S saves once; typing/modifiers and native controls are untouched', () => {
  const {ctx,run,Element}=environment();let saves=0,scrolled=0;
  const card=new Element();card.querySelector=()=>({click(){saves++;}});
  const rows=[new Element(),new Element()];
  rows.forEach(row=>{row.closest=selector=>selector==='.result-select'?row:selector==='.result-item'?{querySelector:()=>card}:null;row.scrollIntoView=()=>{scrolled++;};});
  const selectors=ctx.document.querySelectorAll;ctx.document.querySelectorAll=selector=>selector==='.result-select'?rows:selectors(selector);
  ctx.event=keyEvent('ArrowDown',rows[0]);run('handleWorkspaceShortcut(event)');
  assert.equal(ctx.document.activeElement,rows[1]);assert.equal(ctx.event.defaultPrevented,true);assert.equal(scrolled,1);
  ctx.event=keyEvent('s',rows[1]);run('handleWorkspaceShortcut(event)');assert.equal(saves,1);
  for (const extra of [{repeat:true},{ctrlKey:true},{metaKey:true},{altKey:true},{shiftKey:true},{isComposing:true},{defaultPrevented:true}]) {
    ctx.event=keyEvent('s',rows[1],extra);run('handleWorkspaceShortcut(event)');
  }
  const editable={closest:()=>({})};ctx.event=keyEvent('s',editable);run('handleWorkspaceShortcut(event)');
  assert.equal(saves,1);
  ctx.event=keyEvent('s',{closest:()=>null});run('handleWorkspaceShortcut(event)');assert.equal(saves,1,'global letters never toggle a hidden selection');
});

test('decimal comparisons preserve exact changes and do not confuse unknown amounts with zero', () => {
  const {ctx,run}=environment();
  for (const [left,right] of [['12345.6700','12345.67'],['1e3','1000'],['0.00','-0'],['120.0','120']]) {
    ctx.left=left;ctx.right=right;assert.equal(run('comparableAmount(left)'),run('comparableAmount(right)'));
  }
  assert.equal(run('comparableAmount(null)'),null);assert.equal(run('comparableAmount("garbage")'),undefined);
  ctx.saved=notice();ctx.current=notice({estimated_value:'12345.670000000000001'});
  let changes=plain(run('compareSavedNotice(saved,current).changes'));
  assert.equal(changes[0].field,'estimated_value');
  ctx.current=notice({estimated_value:null});changes=plain(run('compareSavedNotice(saved,current).changes'));
  assert.equal(changes[0].after,null);
});

test('notice comparison refuses partial, misattributed or invalid evidence and does not infer versions', () => {
  const {ctx,run}=environment();ctx.saved=notice();
  for (const current of [null,{},notice({source:'ted'}),notice({external_id:'other'}),notice({deadline:'2026-02-30'}),notice({estimated_value:undefined})]) {
    ctx.current=current;assert.equal(run('compareSavedNotice(saved,current)'),null);
  }
  ctx.current=notice({raw:{notice_version:'02'}});
  assert.equal(run('compareSavedNotice(saved,current).revisionCompared'),false);
  ctx.saved.notice_revision={notice_version:'01'};
  assert.equal(run('compareSavedNotice(saved,current).revisionCompared'),true);
  assert.equal(run('compareSavedNotice(saved,current).changes[0].field'),'revision');
});

test('revision baselines come only from loaded matching evidence and never from a newer changed record', () => {
  const {ctx,run}=environment();ctx.saved=notice();ctx.current=notice();
  run('rememberNoticeRevision(current)');assert.deepEqual(plain(run('savedNoticeRevision(saved)')),{notice_version:'01'});
  ctx.current=notice({deadline:'2027-01-10'});run('rememberNoticeRevision(current)');
  assert.equal(run('savedNoticeRevision(saved)'),null);
});

test('expired indicators use valid dates and Brussels midnight rather than the host timezone', () => {
  const {ctx,run}=environment();ctx.now=new Date('2026-11-03T23:30:00Z');
  assert.equal(run('brusselsToday(now)'),'2026-11-04');
  assert.equal(run('validNoticeDate("2026-02-30")'),null);
  assert.equal(run('validNoticeDate("n/a")'),null);
  ctx.shortlist=[notice({deadline:'2020-01-01'})];
  assert.match(run('renderShortlistChanges()'),/Saved deadline has passed \(not rechecked\)/);
  assert.match(run('renderShortlistChanges()'),/Not checked/);
});

test('change rendering escapes stored/source values and labels unavailable revision comparisons', () => {
  const {ctx,run}=environment();ctx.shortlist=[notice()];
  ctx.state={status:'checked',deadline:'2026-12-18',revisionCompared:false,checkedAt:'2026-11-04T10:00:00Z',changes:[{field:'title',before:'<img onerror=x>',after:'<script>bad</script>'}]};
  run('shortlistChecks.set(snapshotCheckKey(shortlist[0]),state)');
  const markup=run('renderShortlistChanges()');
  assert.match(markup,/&lt;img onerror=x&gt;/);assert.match(markup,/&lt;script&gt;/);assert.doesNotMatch(markup,/<img|<script/);
  assert.match(markup,/Revision comparison unavailable/);assert.match(markup,/Brussels time/);
});

test('saved-notice checks are sequential, bounded to five and cannot mutate the saved baseline', async () => {
  const {ctx,run}=environment();let calls=0,active=0,maximum=0;
  ctx.shortlist=Array.from({length:7},(_,i)=>notice({external_id:String(i)}));
  ctx.apiFetch=async url=>{calls++;active++;maximum=Math.max(maximum,active);await new Promise(resolve=>setImmediate(resolve));active--;const id=url.split('/').at(-1);return {ok:true,json:async()=>notice({external_id:id,deadline:'2027-01-10'})};};
  await run('checkShortlistChanges()');
  assert.equal(calls,5);assert.equal(maximum,1);
  assert.equal(ctx.shortlist[0].deadline,'2026-12-18');
  assert.equal(run('shortlistChecks.get(snapshotCheckKey(shortlist[0])).status'),'checked');
});

test('authorization/quota failures stop the batch; missing or malformed records remain unavailable', async () => {
  for (const status of [401,403,429]) {
    const {ctx,run}=environment();let calls=0;ctx.shortlist=[notice(),notice({external_id:'second'})];
    ctx.apiFetch=async()=>{calls++;return {ok:false,status};};await run('checkShortlistChanges()');
    assert.equal(calls,1);
    assert.equal(run('shortlistChecks.get(snapshotCheckKey(shortlist[1])).status'),'unavailable');
  }
  const {ctx,run}=environment();ctx.shortlist=[notice()];ctx.apiFetch=async()=>({ok:true,json:async()=>({description:'insufficient payload'})});
  await run('checkShortlistChanges()');assert.equal(run('shortlistChecks.get(snapshotCheckKey(shortlist[0])).status'),'unavailable');
});

test('in-flight changes cannot leak across company searches or resurrect a removed snapshot', async () => {
  const {ctx,run}=environment();let complete;
  ctx.shortlist=[notice()];ctx.apiFetch=()=>new Promise(resolve=>{complete=resolve;});
  const pending=run('checkShortlistChanges()');
  ctx.shortlistEnterpriseNumber='company-b';run('workspaceSearchStarted(8)');
  complete({ok:true,json:async()=>notice({deadline:'2027-01-10'})});await pending;
  assert.equal(run('shortlistChecks.size'),0);
  ctx.shortlistEnterpriseNumber='company-a';ctx.shortlist=[notice()];
  const removed=run('checkShortlistChanges()');ctx.shortlist=[];
  complete({ok:true,json:async()=>notice()});await removed;
  assert.notEqual(run('shortlistChecks.get(JSON.stringify(["company-a","bda","fixture-folder","Digital services","Demo authority","2026-12-18","1234567e-2",null]))?.status'),'checked');
});

test('retained-notice checks survive a same-company preference-save generation', async () => {
  const {ctx,run,element}=environment();let resolve;
  ctx.shortlist=[notice()];ctx.apiFetch=()=>new Promise(done=>{resolve=done;});
  const checking=run('checkShortlistChanges()');
  ctx.searchSequence++;
  resolve({ok:true,json:async()=>notice()});await checking;
  assert.equal(run('shortlistChecking'),false);assert.equal(element('#check-shortlist-changes').disabled,false);
  assert.equal(run('shortlistChecks.get(snapshotCheckKey(shortlist[0])).status'),'checked');
});

test('saved snapshot loading preserves real API decimal strings', () => {
  const {ctx,run}=environment();let rendered=0;
  ctx.localStorage={getItem:()=>JSON.stringify([{...notice(),score:84,confidence:0.7,source_url:null,notice_revision:{notice_version:'01'}}])};
  ctx.shortlistStorageKey=company=>`shortlist:${company}`;ctx.shortlistLimit=5;ctx.renderShortlist=()=>{rendered++;};
  const at=html.indexOf('    function loadShortlist(');
  vm.runInContext(html.slice(at,html.indexOf('    function persistShortlist(',at)),ctx);
  run("loadShortlist('company-a')");assert.equal(ctx.shortlist[0].estimated_value,'12345.67');assert.equal(rendered,1);
  ctx.current=notice();assert.equal(run('compareSavedNotice(shortlist[0],current).changes.length'),0);
});

test('saved follow-up plans survive reload while old and malformed metadata leave snapshots usable', () => {
  const {ctx,run}=environment();
  const base={...notice(),score:84,confidence:0.7,source_url:null};
  const saved=[{...base,next_action:'Review with colleague',follow_up_date:'2026-11-06',action_done:true},
    {...base,external_id:'old'},{...base,external_id:'bad',next_action:42,follow_up_date:'2026-02-30',action_done:'true'}];
  ctx.localStorage={getItem:()=>JSON.stringify(saved)};ctx.shortlistStorageKey=company=>company;ctx.shortlistLimit=5;
  const at=html.indexOf('    function loadShortlist(');
  vm.runInContext(html.slice(at,html.indexOf('    function persistShortlist(',at)),ctx);
  run("loadShortlist('company-a')");
  assert.equal(ctx.shortlist.length,3);
  assert.deepEqual(plain(run('shortlistFollowUp(shortlist[0])')),{next_action:'Review with colleague',follow_up_date:'2026-11-06',action_done:true});
  for (const item of ctx.shortlist.slice(1)) {
    assert.equal(item.next_action,'');assert.equal(item.follow_up_date,'');assert.equal(item.action_done,false);
    assert.equal(item.estimated_value,'12345.67');
  }
});

test('saving a recommendation keeps the exact API decimal amount in its snapshot', () => {
  const {ctx,run,element}=environment();let handler;
  ctx.item={...notice(),score:84,confidence:0.7,source_url:null};
  const button={dataset:{shortlistKey:JSON.stringify(['bda','fixture-folder'])},closest:()=>({}),addEventListener(_event,fn){handler=fn;}};
  Object.assign(ctx,{heading:element('#heading'),results:{set innerHTML(_value){},querySelectorAll:selector=>selector==='[data-shortlist-toggle]'?[button]:[]},resultsSummary:element('#summary'),shortlistLimit:5,message:element('#message'),applyUiLanguage(){},mountTenderWorkspace(){},renderLotCpvNaceAlignment:()=>'',safeNoticeUrl:()=>'',formatNumber:String,formatValue:String});
  const at=html.indexOf('    function renderResults(items, enterpriseNumber) {');
  vm.runInContext(html.slice(at,html.indexOf("    form.addEventListener('submit'",at)),ctx);
  run("renderResults([item], 'company-a')");handler();
  assert.equal(ctx.shortlist[0].estimated_value,'12345.67');
  assert.equal(run('compareSavedNotice(shortlist[0],item).changes.length'),0);
});

test('currency rendering retains API cents and integers beyond binary-float precision', () => {
  const {ctx,run}=environment();
  const at=html.indexOf('    function formatValue(value) {');
  vm.runInContext(html.slice(at,html.indexOf('    // BEGIN-ATTACHMENT-EVIDENCE',at)),ctx);
  const expected = value => new Intl.NumberFormat('en-BE',{style:'currency',currency:'EUR',minimumFractionDigits:0,maximumFractionDigits:2}).format(value);
  ctx.amount='9007199254740993.12';assert.equal(run('formatValue(amount)'),expected(ctx.amount));
  ctx.amount='12345.67';assert.equal(run('formatValue(amount)'),expected(ctx.amount));
  assert.equal(run('formatValue(null)'),copy.en.notStated);
});
