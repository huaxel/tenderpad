/* Explicit, fictional workspace. This adapter never calls the application API. */
function createExampleWorkspace(href, now = new Date()) {
  const active = new URL(href).searchParams.get('example') === '1';
  const companyNumber = 'EXAMPLE';
  const storagePrefix = active ? 'tenderpad.example.' : 'aanbestedingen.';
  const exampleKeys = {
    enterpriseNumber: 'exampleCompany', officialKbo: 'exampleCompanyData',
    registeredActivities: 'exampleActivities', sourceEvidenceHelp: 'exampleSourceHelp',
    inspectNotice: 'inspectExampleNotice',
    matchingProfileCopy: 'examplePreferencesCopy', shapeProfile: 'exampleProfileTitle',
  };
  // Dates keep the fictional walkthrough usable; scores are fixed illustrations,
  // not results of a second matcher or a claim about recommendation quality.
  const today = new Intl.DateTimeFormat('sv-SE', {timeZone: 'Europe/Brussels', year: 'numeric', month: '2-digit', day: '2-digit'}).format(now);
  const day = offset => {
    const [year, month, date] = today.split('-').map(Number);
    return new Date(Date.UTC(year, month - 1, date + offset)).toISOString().slice(0, 10);
  };
  const profile = {
    enterprise_number: companyNumber, names: ['Northline Digital'], status: 'EXAMPLE',
    activities: [{code:'62010', dutch:'Softwareontwikkeling en digitale dienstverlening', french:'Développement logiciel et services numériques'}],
  };
  const preferences = {
    confirmed_activities:['Software development and digital services'], excluded_terms:[],
    preferred_regions:['Brussels','Leuven'], languages:['nl','fr','en'],
    minimum_value:null, maximum_value:null, recognition_class:null, recognition_categories:[],
  };
  const items = [
    {
      source:'EXAMPLE', external_id:'EXAMPLE-001', title:'Digital services for community organisations',
      buyer:'Example municipality', deadline:day(21), estimated_value:'48000.00',
      regions:['Brussels'], cpv_codes:['72000000'], languages:['nl','fr'], score:84.5, confidence:0.78,
      reasons:['Tender activity matches registered company activities','Tender language matches preferred languages'],
      warnings:['The fictional notice asks for two comparable projects. Company references have not been reviewed.'],
      requirement_signals:[], requirements:[{kind:'experience',text:'Provide two recent examples of comparable digital projects.'}],
      description:'FICTIONAL EXAMPLE. Design and maintain a website for community organisations. The example asks for two comparable projects. The sample does not establish whether the company meets that requirement.',
    },
    {
      source:'EXAMPLE', external_id:'EXAMPLE-002', title:'Data platform development and support',
      buyer:'Example public agency', deadline:day(36), estimated_value:null,
      regions:['Leuven'], cpv_codes:['72200000'], languages:['nl'], score:72, confidence:0.62,
      reasons:['Tender activity matches registered company activities'], warnings:[], requirement_signals:[], requirements:[],
      description:'FICTIONAL EXAMPLE. Develop and support a data platform. The sample does not state a contract value or selection requirements. These remain unknown.',
    },
    {
      source:'EXAMPLE', external_id:'EXAMPLE-003', title:'Accessibility review of public websites',
      buyer:'Example intermunicipal association', deadline:null, estimated_value:'18000.00',
      regions:['Brussels'], cpv_codes:['72200000'], languages:['fr'], score:64, confidence:null,
      reasons:['Tender language matches preferred languages'], warnings:[], requirement_signals:[], requirements:[],
      description:'FICTIONAL EXAMPLE. Review the accessibility of public websites. The sample does not state a closing date. Confirming a real deadline would be a first review step.',
    },
  ];
  const reply = (value, status = 200) => new Response(JSON.stringify(value), {status, headers:{'content-type':'application/json'}});
  async function request(path, options = {}) {
    // Closed routing: unknown companies, endpoints, methods and remote origins
    // fail inside the example. They never fall through to a real request.
    if (!active) return reply({detail:'Example workspace is inactive'}, 404);
    if ((options.method || 'GET').toUpperCase() !== 'GET') return reply({detail:'Example data is read-only'}, 405);
    const url = new URL(path, 'https://example.invalid');
    if (url.origin !== 'https://example.invalid') return reply({detail:'Unknown example route'}, 404);
    if (url.pathname === '/health') return reply({status:'example',tenders:items.length,tenders_open_procurements:items.length,tenders_freshness:'unknown'});
    if (url.pathname === '/reference/recognition') return reply({categories:[],classes:[]});
    if (url.pathname === `/companies/${companyNumber}`) return reply(profile);
    if (url.pathname === `/companies/${companyNumber}/preferences`) return reply(preferences);
    if (url.pathname === `/companies/${companyNumber}/inventory-awards`) return reply({engagements:[],accounting_rows:0});
    if (url.pathname === `/companies/${companyNumber}/awards`) return reply({awards:[]});
    if (url.pathname === `/companies/${companyNumber}/recommendations`) {
      let selected = [...items];
      const window = url.searchParams.get('due_within');
      if (['7','30','90'].includes(window)) selected = selected.filter(item => item.deadline && item.deadline >= today && item.deadline <= day(Number(window)));
      if (url.searchParams.get('sort') === 'deadline') selected.sort((a,b) => (a.deadline || '9999').localeCompare(b.deadline || '9999'));
      return reply(selected.map(({description, ...item}) => ({...item,source_url:null})));
    }
    const item = items.find(value => url.pathname === `/tenders/EXAMPLE/${value.external_id}`);
    if (item) return reply({...item,source_url:null,raw:{example:true}});
    return reply({detail:'Unknown example route'}, 404);
  }
  return {active,companyNumber,storagePrefix,request,copyKey:key => active ? exampleKeys[key] || key : key};
}

const exampleWorkspace = createExampleWorkspace(location.href);
