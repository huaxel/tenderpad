const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const root = path.resolve(__dirname,'../web');
const html = fs.readFileSync(path.join(root,'welcome/index.html'),'utf8');
const source = fs.readFileSync(path.join(root,'welcome/landing.js'),'utf8');
const dashboard = fs.readFileSync(path.join(root,'index.html'),'utf8');

function landing({href='https://app.test/welcome/',saved='en',browser='en',blocked=false}={}) {
  const reads=[],changes=[];
  const nodes = attribute => [...html.matchAll(new RegExp(`${attribute}="([a-zA-Z]+)"`,'g'))].map(match => ({
    dataset:{[attribute==='data-copy'?'copy':attribute==='data-copy-content'?'copyContent':'copyAria']:match[1]},
    setAttribute(key,value){this[key]=value;},
  }));
  const copy=nodes('data-copy'), content=nodes('data-copy-content'), aria=nodes('data-copy-aria');
  const workspace=[{}], example=[{}], home=[{}], select={addEventListener(_event,handler){this.change=handler;}};
  const document={documentElement:{},querySelector:()=>select,querySelectorAll:selector=>({
    '[data-copy]':copy,'[data-copy-content]':content,'[data-copy-aria]':aria,
    '[data-workspace-link]':workspace,'[data-example-link]':example,'[data-landing-link]':home,
  }[selector]||[])};
  const context=vm.createContext({URL,URLSearchParams,document,location:{href},navigator:{language:browser},
    localStorage:{getItem(key){reads.push(key);if(blocked)throw new Error('Unavailable');return saved;},setItem(){throw new Error('Unexpected storage write');}},
    history:{replaceState(_state,_title,url){changes.push(url);}},fetch(){throw new Error('Unexpected API request');},
  });
  vm.runInContext(source,context);
  return {document,copy,content,aria,workspace,example,home,select,reads,changes,run:code=>vm.runInContext(code,context)};
}

test('landing language honours a valid link, then saved preference, then browser language', () => {
  const {run}=landing();
  for (const [href,saved,browser,expected] of [
    ['https://app.test/welcome/?lang=fr','nl','en','fr'],
    ['https://app.test/welcome/?lang=constructor','nl','fr','nl'],
    ['https://app.test/welcome/','invalid','fr-BE','fr'],
    ['https://app.test/welcome/',null,'NL-be','nl'],
    ['https://app.test/welcome/?lang=<script>',null,'de-BE','en'],
  ]) assert.equal(run(`initialLandingLanguage(${JSON.stringify(href)},${JSON.stringify(saved)},${JSON.stringify(browser)})`),expected);
});

test('the complete landing page translates copy, metadata and destination links without reading credentials or writing storage', () => {
  const env=landing({href:'https://app.test/welcome/?lang=fr&token=synthetic-secret#token=ignored'});
  assert.deepEqual(env.reads,['aanbestedingen.uiLanguage']);
  assert.equal(env.document.documentElement.lang,'fr');
  assert.equal(env.document.title,'Tenderpad · Votre voie vers les marchés publics');
  assert.equal(env.workspace[0].href,'/?lang=fr');
  assert.equal(env.example[0].href,'/?example=1&lang=fr');
  assert.equal(env.home[0].href,'/welcome/?lang=fr');
  assert.equal(env.changes.length,0);
  for (const element of env.copy) assert.ok(element.textContent,element.dataset.copy);
  assert.match(env.content[0].content,/marchés publics belges/);
  assert.equal(env.aria[0]['aria-label'],'Navigation principale');
});

test('language selection keeps its deep link while every outgoing route uses only known parameters', () => {
  const env=landing({href:'https://app.test/welcome/?extra=ignored#questions'});
  for (const lang of ['nl','fr','en']) {
    env.select.change({target:{value:lang}});
    assert.equal(env.document.documentElement.lang,lang);
    assert.equal(env.workspace[0].href,`/?lang=${lang}`);
    assert.equal(env.example[0].href,`/?example=1&lang=${lang}`);
    assert.equal(env.changes.at(-1),`/welcome/?lang=${lang}#questions`);
  }
  const title=env.document.title;
  env.select.change({target:{value:'unsupported'}});
  assert.equal(env.document.title,title);
});

test('blocked browser storage still permits a complete translated landing page', () => {
  const env=landing({blocked:true,browser:'nl-BE'});
  assert.equal(env.document.documentElement.lang,'nl');
  assert.equal(env.select.value,'nl');
  assert.ok(env.copy.every(node=>typeof node.textContent==='string'&&node.textContent.length));
});

test('workspace language links override only language and respect demo storage isolation', () => {
  const uiCopy={en:{},nl:{},fr:{}};
  const start=dashboard.indexOf('    const uiLanguageStorageKey =');
  const block=dashboard.slice(start,dashboard.indexOf('    const t =',start));
  for(const [href,saved,example,expected] of [
    ['https://app.test/?lang=fr','nl',false,'fr'],['https://app.test/?example=1&lang=nl','fr',true,'nl'],
    ['https://app.test/?lang=constructor','nl',false,'nl'],['https://app.test/','en',false,'en'],
  ]) {
    const prefix=example?'tenderpad.example.':'aanbestedingen.';
    const context={uiCopy,URL,location:{href},exampleWorkspace:{storagePrefix:prefix},localStorage:{getItem(key){assert.equal(key,prefix+'uiLanguage');return saved;}}};
    assert.equal(vm.runInNewContext(block+';uiLanguage',context),expected);
  }
  assert.equal(vm.runInNewContext(block+';uiLanguage',{uiCopy,URL,location:{href:'https://app.test/?lang=fr'},exampleWorkspace:{storagePrefix:'aanbestedingen.'},localStorage:{getItem(){throw new Error('Blocked');}}}),'fr');
});

test('changing workspace language updates an existing language link so reload does not undo the choice', () => {
  const start=dashboard.indexOf("    languageSelect.addEventListener('change'");
  const block=dashboard.slice(start,dashboard.indexOf('    applyUiLanguage(uiLanguage);',start));
  let handler,cleaned,written;
  const context=vm.createContext({URL,location:{href:'https://app.test/?example=1&lang=fr#section'},uiLanguage:'fr',
    uiLanguageStorageKey:'tenderpad.example.uiLanguage',languageSelect:{value:'nl',addEventListener(_event,fn){handler=fn;}},
    localStorage:{setItem(key,value){written={key,value};}},history:{replaceState(_state,_title,url){cleaned=url;}},renderShortlist(){},
  });
  context.applyUiLanguage=value=>{context.uiLanguage=value;};
  vm.runInContext(block,context);handler();
  assert.equal(cleaned,'/?example=1&lang=nl#section');
  assert.deepEqual(written,{key:'tenderpad.example.uiLanguage',value:'nl'});
});

test('workspace home, enter-example, exit-example and product links preserve the chosen language', () => {
  const start=dashboard.indexOf('    function applyUiLanguage(language) {');
  const block=dashboard.slice(start,dashboard.indexOf("    languageSelect.addEventListener('change'",start));
  const links={'[data-product-page]':[{}],'[data-real-workspace]':[{},{}],'[data-example-workspace]':[{}]};
  const context=vm.createContext({uiCopy:{en:{},nl:{},fr:{}},uiLanguage:'en',t:key=>key,languageSelect:{},updateResultsSummary(){},
    document:{documentElement:{},querySelectorAll:selector=>links[selector]||[]},
  });
  vm.runInContext(block,context);
  for(const language of ['en','nl','fr']) {
    vm.runInContext(`applyUiLanguage('${language}')`,context);
    for(const link of links['[data-real-workspace]'])assert.equal(link.href,`/?lang=${language}`);
    assert.equal(links['[data-example-workspace]'][0].href,`/?example=1&lang=${language}`);
    assert.equal(links['[data-product-page]'][0].href,`/welcome/?lang=${language}`);
  }
});

test('all landing translation keys and destinations are complete; local assets and fragment targets exist', () => {
  const {run}=landing();
  const copy=run('landingCopy');
  const keys=Object.keys(copy.en).sort();
  for(const language of ['en','nl','fr'])assert.deepEqual(Object.keys(copy[language]).sort(),keys);
  for(const match of html.matchAll(/data-copy(?:-content|-aria)?="([a-zA-Z]+)"/g))for(const language of ['en','nl','fr'])assert.ok(copy[language][match[1]],`${language}:${match[1]}`);
  const ids=[...html.matchAll(/\bid="([^"]+)"/g)].map(match=>match[1]);
  assert.equal(new Set(ids).size,ids.length);
  for(const match of html.matchAll(/(?:href|src)="([^"]+)"/g)) {
    const url=match[1];
    if(url.startsWith('#'))assert.ok(ids.includes(url.slice(1)),url);
    else {
      assert.ok(url.startsWith('/'),`local link: ${url}`);
      const pathname=new URL(url,'https://app.test').pathname;
      const target=path.join(root,pathname.endsWith('/')?pathname+'index.html':pathname);
      assert.ok(fs.existsSync(target),target);
    }
  }
  assert.equal((html.match(/<h1\b/g)||[]).length,1);
  assert.equal((html.match(/<details>/g)||[]).length,4);
  assert.match(html,/<h1[^>]*>Your path to public contracts\.<\/h1>/,'no-JS content remains useful');
});

test('the labelled landing story matches the existing fictional company and first notice', async () => {
  const fixture=fs.readFileSync(path.join(root,'example.js'),'utf8');
  const create=vm.runInNewContext(fixture+';createExampleWorkspace',{URL,Intl,Date,Response,location:{href:'https://app.test/'}});
  const example=create('https://app.test/?example=1',new Date('2026-11-04T12:00:00Z'));
  const profile=await(await example.request('/companies/EXAMPLE')).json();
  const items=await(await example.request('/companies/EXAMPLE/recommendations')).json();
  const detail=await(await example.request('/tenders/EXAMPLE/EXAMPLE-001')).json();
  assert.ok(html.includes(profile.names[0]));
  assert.ok(html.includes(items[0].title));assert.ok(html.includes(items[0].buyer));
  assert.match(detail.requirements[0].text,/two recent examples/);
  assert.match(html,/The notice asks for two comparable projects/);
  assert.match(html,/Fictional example/);assert.match(html,/references have not been reviewed/);
  assert.equal(items.length,3);assert.match(landing().run('landingCopy.en.faqTwoCopy'),/three fictional notices/);
  assert.doesNotMatch(html,/\d+\s*\/100|\d+%|guaranteed|approved bidder/i);
});
