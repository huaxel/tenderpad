// Exercise the real fictional-data adapter and its boundary with the normal API.
const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname,'../web/index.html'),'utf8');
const source = fs.readFileSync(path.join(__dirname,'../web/example.js'),'utf8');
const create = vm.runInNewContext(source + '; createExampleWorkspace', {URL,Intl,Date,Response,location:{href:'https://app.test/'}});
const fixtureTime = new Date('2026-11-04T23:30:00Z'); // Already 5 November in Brussels.
const example = () => create('https://app.test/?example=1', fixtureTime);

test('the fictional workspace requires explicit opt-in and keeps real storage names intact', () => {
  for (const suffix of ['','?example=0','?example=true','#example=1']) {
    const normal = create('https://app.test/'+suffix,fixtureTime);
    assert.equal(normal.active,false);
    assert.equal(normal.storagePrefix,'aanbestedingen.');
    assert.equal(normal.copyKey('officialKbo'),'officialKbo');
  }
  assert.equal(example().active,true);
  assert.equal(example().storagePrefix,'tenderpad.example.');
  const keySource = html.match(/const shortlistStorageKey = ([^;]+);/)[1];
  const key = mode => vm.runInNewContext(keySource,{exampleWorkspace:mode,encodeURIComponent})('EXAMPLE');
  assert.equal(key(example()),'tenderpad.example.shortlist.v1:EXAMPLE');
  assert.equal(key(create('https://app.test/')),'aanbestedingen.shortlist.v1:EXAMPLE');
});

test('example requests fail closed for real companies, remote origins, writes and analysis', async () => {
  const mode = example();
  for (const [route,options,status] of [
    ['/companies/0404.484.060',{},404],
    ['https://other.test/companies/EXAMPLE',{},404],
    ['/companies/EXAMPLE/preferences',{method:'PUT',body:'{}'},405],
    ['/companies/EXAMPLE/feedback',{method:'POST',body:'{}'},405],
    ['/companies/EXAMPLE/tenders/EXAMPLE/EXAMPLE-001/analysis',{},404],
    ['/unknown',{},404],
  ]) assert.equal((await mode.request(route,options)).status,status,route);
  assert.equal((await create('https://app.test/').request('/companies/EXAMPLE')).status,404);
});

test('example API interception does not read credentials or make a network request', async () => {
  const start = html.indexOf('    async function apiFetch(url, options = {}) {');
  const block = html.slice(start,html.indexOf('    const escapeHtml',start));
  let networkCalls = 0, tokenReads = 0;
  const ctx = vm.createContext({exampleWorkspace:example(),tokenField:{get value(){tokenReads++;return 'real-token';}},fetch(){networkCalls++;throw new Error('Unexpected network call');}});
  vm.runInContext(block,ctx);
  assert.equal((await vm.runInContext("apiFetch('/companies/EXAMPLE')",ctx)).status,200);
  assert.equal((await vm.runInContext("apiFetch('/companies/EXAMPLE/feedback',{method:'POST'})",ctx)).status,405);
  assert.equal((await vm.runInContext("apiFetch('/companies/real-company')",ctx)).status,404);
  assert.equal(tokenReads,0);
  assert.equal(networkCalls,0);
});

test('normal API requests still use saved credentials and the existing fetch path', async () => {
  const start = html.indexOf('    async function apiFetch(url, options = {}) {');
  const block = html.slice(start,html.indexOf('    const escapeHtml',start));
  let request;
  const ctx = vm.createContext({exampleWorkspace:create('https://app.test/'),tokenField:{value:'saved-token'},fetch:async(url,options)=>{request={url,options};return {status:200};}});
  vm.runInContext(block,ctx);
  await vm.runInContext("apiFetch('/health')",ctx);
  assert.equal(request.url,'/health');
  assert.equal(request.options.headers.Authorization,'Bearer saved-token');
});

test('fictional notices preserve unknown amounts, missing deadlines and exact decimal values', async () => {
  const mode = example();
  const items = await (await mode.request('/companies/EXAMPLE/recommendations')).json();
  assert.equal(items.length,3);
  assert.equal(items[0].deadline,'2026-11-26');
  assert.equal(items[0].estimated_value,'48000.00');
  assert.equal(items[1].estimated_value,null);
  assert.equal(items[2].deadline,null);
  assert.equal(items[2].confidence,null);
  assert.ok(items.every(item=>item.source==='EXAMPLE' && item.source_url===null));
  assert.equal((await (await mode.request('/companies/EXAMPLE/recommendations?due_within=7')).json()).length,0);
  assert.equal((await (await mode.request('/companies/EXAMPLE/recommendations?due_within=30')).json()).length,1);
  assert.equal((await (await mode.request('/companies/EXAMPLE/recommendations?due_within=90')).json()).length,2);
  const detail = await (await mode.request('/tenders/EXAMPLE/EXAMPLE-001')).json();
  assert.match(detail.description,/FICTIONAL EXAMPLE/);
  assert.equal(detail.requirements.length,1);
  detail.title='Changed by caller';
  assert.notEqual((await (await mode.request('/tenders/EXAMPLE/EXAMPLE-001')).json()).title,detail.title);
});

test('example-specific labels have EN/NL/FR copy and do not present company data as official KBO evidence', () => {
  const copy = vm.runInNewContext(html.slice(html.indexOf('    const uiCopy = {'),html.indexOf('    const languageSelect'))+';uiCopy');
  for (const key of ['enterpriseNumber','officialKbo','registeredActivities','sourceEvidenceHelp','inspectNotice','matchingProfileCopy','shapeProfile']) {
    const mapped = example().copyKey(key);
    assert.notEqual(mapped,key);
    for (const lang of ['en','nl','fr']) assert.ok(copy[lang][mapped],`${lang}:${mapped}`);
  }
});

const copy = vm.runInNewContext(html.slice(html.indexOf('    const uiCopy = {'),html.indexOf('    const languageSelect'))+';uiCopy');
const escapeHtml = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#039;'}[c]));
const renderer = html.slice(html.indexOf('    function renderRequirementEvidence('),html.indexOf("    form.addEventListener('submit'"));
function render(item, controls = {}) {
  const results = {innerHTML:'',querySelectorAll:selector=>controls[selector] || []};
  const context = vm.createContext({
    results, heading:{classList:{remove(){}}}, resultsSummary:{dataset:{},classList:{remove(){}}},
    exampleWorkspace:{active:false}, activeSort:'score',activeDueWithin:'',shortlist:[],uiLanguage:'en',
    t:key=>copy.en[key] || key,escapeHtml,formatNumber:String,formatDate:String,formatValue:String,
    comparableAmount:value=>value,shortlistIdentity:value=>JSON.stringify([value.source,value.external_id]),
    renderLotCpvNaceAlignment:()=>'',safeNoticeUrl:()=>'',applyUiLanguage(){},closeTenderSelection(){},mountTenderWorkspace(){},
    ...controls.context,
  });
  vm.runInContext(renderer,context);
  context.item=item;
  vm.runInContext("renderResults([item],'TEST')",context);
  return {markup:results.innerHTML,context};
}
const recommendation = overrides => ({source:'TEST',external_id:'001',title:'Test opportunity',score:48,confidence:null,
  deadline:'2026-12-01',estimated_value:'0.00',reasons:[],requirements:[],requirement_signals:[],warnings:[],...overrides});

test('decision summary preserves missing evidence, real reasons and zero values without inventing support', () => {
  const {markup} = render(recommendation({reasons:['<img onerror=x>'],warnings:['<script>warning</script>']}));
  assert.match(markup,/&lt;img onerror=x&gt;/);
  assert.match(markup,/&lt;script&gt;warning&lt;\/script&gt;/);
  assert.doesNotMatch(markup,/<img|<script/);
  assert.match(markup,/Selection requirements and lot scope still need review/);
  assert.match(markup,/data-format-currency="0.00"/);
  assert.doesNotMatch(markup,/data-i18n="unknownValue"/);
  assert.match(markup,/data-i18n="nextReviewCopy"/);
  assert.match(markup,/data-review-profile/);
  assert.match(markup,/data-review-evidence/);
  const empty = render(recommendation()).markup;
  assert.match(empty,/No matching reasons were supplied/);
  assert.doesNotMatch(empty,/Tender activity matches registered company activities/);
});

test('next review step prioritizes absent deadlines, then extracted requirements, then absent amounts', () => {
  const cases = [
    [{deadline:null,estimated_value:null,requirements:[{kind:'experience',text:'References required'}]},'nextCheckDeadline'],
    [{estimated_value:null,requirements:[{kind:'experience',text:'References required'}]},'nextCheckRequirements'],
    [{estimated_value:null},'nextCheckValue'],
    [{requirement_signals:[{label:'Possible reference requirement',values:[]}]},'nextReviewCopy'],
  ];
  for (const [overrides,key] of cases) assert.match(render(recommendation(overrides)).markup,new RegExp(`data-i18n="${key}"`));
});

test('summary actions open the company profile and the original selected notice evidence tab', () => {
  const handlers = {};
  const card = {};
  const button = key => ({addEventListener(_event,handler){handlers[key]=handler;},closest(){return card;}});
  let view,tab,focused=false;
  render(recommendation(),{'[data-review-profile]':[button('profile')],'[data-review-evidence]':[button('evidence')],context:{
    selectedCard:card,showWorkspaceView:value=>{view=value;},setDetailTab:value=>{tab=value;},document:{querySelector:()=>({focus(){focused=true;}})},
  }});
  handlers.profile(); assert.equal(view,'profile');
  handlers.evidence(); assert.equal(tab,'evidence'); assert.equal(focused,true);
});

test('retained requirements preserve source pages and recorded mandatory status without inventing missing facts', () => {
  const {context} = render(recommendation());
  const show = value => {context.value=value;return vm.runInContext('renderRequirementEvidence(value)',context);};
  const markup = show([
    {kind:'experience',text:'<script>Two references</script>',mandatory:true,source_page:3},
    {kind:'other',text:'Optional site visit',mandatory:false,source_page:null},
    {text:'Unspecified status',source_page:-1},null,{text:''},
  ]);
  assert.match(markup,/&lt;script&gt;Two references&lt;\/script&gt;/);
  assert.match(markup,/Recorded as mandatory/); assert.match(markup,/Recorded as optional/);
  assert.match(markup,/Mandatory status not stated/);
  assert.match(markup,/data-i18n="requirementPage">Page<\/span> 3/);
  assert.equal((markup.match(/data-i18n="requirementPage"/g)||[]).length,1);
  assert.doesNotMatch(markup,/<script/);
  for (const value of [null,[],[{text:' '}],{}]) assert.match(show(value),/requirements may still apply/);
});

test('evidence action renders the retained requirement text and record identity, not just a count', async () => {
  let load;
  const output = {};
  const card = {dataset:{source:'TEST',id:'001'},isConnected:true,querySelector:()=>output};
  const button = {addEventListener(_event,handler){load=handler;},closest:()=>card};
  const {context} = render(recommendation(),{'[data-detail]':[button],context:{selectedCard:card,
    beginCardOperation:()=>true,finishCardOperation:()=>true,setDetailTab(){},rememberNoticeRevision(){},
    apiFetch:async()=>({ok:true,json:async()=>({description:'Retained <excerpt>',requirements:[{text:'Two references',source_page:5,mandatory:true}]})}),
    renderFrameworkMaximumEvidence:()=>'',renderNoticeLotCpvEvidence:()=>'',renderAttachmentEvidence:()=>'',
  }});
  await load();
  assert.match(output.innerHTML,/TEST · 001/);
  assert.match(output.innerHTML,/Retained &lt;excerpt&gt;/);
  assert.match(output.innerHTML,/Two references/);
  assert.match(output.innerHTML,/Page<\/span> 5/);
  assert.equal(button.disabled,true);
});
