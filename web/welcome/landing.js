/* Static product page. No application API requests or credential access. */
const landingCopy = {
  en: {
    title: 'Tenderpad · Your path to public contracts', description: 'Find Belgian public contracts that fit your business, inspect the evidence and plan your next step.',
    skip: 'Skip to content', navigation: 'Main navigation', language: 'Language', how: 'How it works', questions: 'Questions',
    openWorkspace: 'Open workspace', audience: 'For Belgian SMEs', tagline: 'Your path to public contracts.',
    heroCopy: 'Find public contracts that match your business. See why they may fit, check the evidence and decide what deserves your time.',
    tryExample: 'Explore the example', findOpportunities: 'Find opportunities',
    demoHelp: 'Start with a fictional company. No enterprise number or API token needed for the example.',
    fictionalExample: 'Fictional example', company: 'The company', companyActivities: 'Software development · Brussels & Leuven',
    opportunity: 'An opportunity to explore', exampleTitle: 'Digital services for community organisations', exampleBuyer: 'Example municipality',
    whyFit: 'Why it may fit', fitReason: 'Digital services align with the company’s activities. The notice is in its working languages.',
    stillCheck: 'What remains to check', checkReason: 'The notice asks for two comparable projects. The company’s references have not been reviewed.',
    nextStep: 'Next step', nextStepCopy: 'Read the notice, check the requirement’s scope and gather the relevant references.',
    openExample: 'Open this example ↗', fictionalHelp: 'The company and notice are fictional. Explore the same example in the workspace.',
    howTitle: 'From a promising match to a clear next step.',
    stepOneTitle: 'Start with your business', stepOneCopy: 'Look up your Belgian enterprise number, then refine the activities, regions and languages that matter to you.',
    stepTwoTitle: 'Read the reasons', stepTwoCopy: 'See the matching signals and what is still unknown. Inspect the retained notice and verify the official documents.',
    stepThreeTitle: 'Keep the review moving', stepThreeCopy: 'Compare saved opportunities, add your assessment and notes, and plan a next action with a follow-up date.',
    beforeStart: 'Before you start', questionsTitle: 'A few useful answers.', questionsIntro: 'Tenderpad helps you decide what to investigate. Your review stays central.',
    faqOneTitle: 'Does a high score mean I can bid?', faqOneCopy: 'A match score helps order opportunities by relevance. Eligibility, capacity and requirements still need to be checked against the official tender documents. Missing information stays unknown.',
    faqTwoTitle: 'Can I try it before looking up my company?', faqTwoCopy: 'Yes. The example workspace uses a fictional company and three fictional notices. You can inspect the evidence, try the filters and build a demo shortlist separately from your real company.',
    faqThreeTitle: 'Where do my shortlist and notes go?', faqThreeCopy: 'Saved opportunities, personal notes, assessments and next actions stay in this browser, separately for each company. Follow-up dates appear in the app; they do not send notifications.',
    faqFourTitle: 'Are the underlying documents always current?', faqFourCopy: 'The workspace shows retained records. Checking a saved notice compares it with the stored record; it does not refresh the official source. Confirm current deadlines and requirements before preparing a bid.',
    takeLook: 'Take a closer look', closingTitle: 'See what a useful starting point feels like.',
    footer: 'A working prototype for opportunity review. Check the official tender documents before deciding to bid.',
  },
  nl: {
    title: 'Tenderpad · Jouw weg naar overheidsopdrachten', description: 'Vind Belgische overheidsopdrachten die bij uw onderneming passen, bekijk de bronnen en plan uw volgende stap.',
    skip: 'Ga naar de inhoud', navigation: 'Hoofdnavigatie', language: 'Taal', how: 'Hoe werkt het?', questions: 'Vragen',
    openWorkspace: 'Open werkruimte', audience: 'Voor Belgische kmo’s', tagline: 'Jouw weg naar overheidsopdrachten.',
    heroCopy: 'Vind overheidsopdrachten die bij uw onderneming passen. Ontdek waarom ze interessant kunnen zijn, bekijk de bronnen en bepaal wat uw tijd verdient.',
    tryExample: 'Verken het voorbeeld', findOpportunities: 'Vind opdrachten',
    demoHelp: 'Begin met een fictief bedrijf. Voor het voorbeeld hebt u geen ondernemingsnummer of API-token nodig.',
    fictionalExample: 'Fictief voorbeeld', company: 'Het bedrijf', companyActivities: 'Softwareontwikkeling · Brussel & Leuven',
    opportunity: 'Een opdracht om te onderzoeken', exampleTitle: 'Digitale diensten voor lokale verenigingen', exampleBuyer: 'Voorbeeldgemeente',
    whyFit: 'Waarom dit kan passen', fitReason: 'Digitale diensten sluiten aan bij de bedrijfsactiviteiten. De aankondiging is in de werktalen van het bedrijf.',
    stillCheck: 'Wat nog te controleren is', checkReason: 'De aankondiging vraagt twee vergelijkbare projecten. De bedrijfsreferenties zijn nog niet beoordeeld.',
    nextStep: 'Volgende stap', nextStepCopy: 'Lees de aankondiging, controleer de reikwijdte van de vereiste en verzamel de relevante referenties.',
    openExample: 'Open dit voorbeeld ↗', fictionalHelp: 'Het bedrijf en de aankondiging zijn fictief. Verken hetzelfde voorbeeld in de werkruimte.',
    howTitle: 'Van een interessante match naar een duidelijke volgende stap.',
    stepOneTitle: 'Begin bij uw onderneming', stepOneCopy: 'Zoek uw Belgische ondernemingsnummer op en verfijn de activiteiten, regio’s en talen die voor u belangrijk zijn.',
    stepTwoTitle: 'Bekijk de onderbouwing', stepTwoCopy: 'Bekijk de matchsignalen en wat nog onbekend is. Lees de bewaarde aankondiging en controleer de officiële documenten.',
    stepThreeTitle: 'Maak de volgende stap concreet', stepThreeCopy: 'Vergelijk bewaarde opdrachten, voeg uw beoordeling en notities toe en plan een volgende actie met een opvolgdatum.',
    beforeStart: 'Voor u begint', questionsTitle: 'Een paar nuttige antwoorden.', questionsIntro: 'Tenderpad helpt u bepalen wat u verder onderzoekt. Uw beoordeling blijft centraal staan.',
    faqOneTitle: 'Betekent een hoge score dat ik kan inschrijven?', faqOneCopy: 'Een matchscore helpt opdrachten op relevantie te rangschikken. Geschiktheid, capaciteit en vereisten moet u nog controleren in de officiële opdrachtdocumenten. Ontbrekende informatie blijft onbekend.',
    faqTwoTitle: 'Kan ik het proberen voordat ik mijn bedrijf opzoek?', faqTwoCopy: 'Ja. De voorbeeldwerkruimte gebruikt een fictief bedrijf en drie fictieve aankondigingen. U kunt bronnen bekijken, filters proberen en een voorbeeldshortlist maken, los van uw echte onderneming.',
    faqThreeTitle: 'Waar worden mijn shortlist en notities bewaard?', faqThreeCopy: 'Bewaarde opdrachten, persoonlijke notities, beoordelingen en volgende acties blijven in deze browser, afzonderlijk voor elk bedrijf. Opvolgdatums verschijnen in de app; ze sturen geen meldingen.',
    faqFourTitle: 'Zijn de onderliggende documenten altijd actueel?', faqFourCopy: 'De werkruimte toont bewaarde dossiers. Een controle vergelijkt een bewaarde opdracht met het opgeslagen bronrecord en haalt geen nieuwe gegevens bij de officiële bron op. Bevestig de actuele deadlines en vereisten voordat u een offerte voorbereidt.',
    takeLook: 'Bekijk het van dichtbij', closingTitle: 'Ontdek hoe een bruikbaar vertrekpunt eruitziet.',
    footer: 'Een werkend prototype voor de beoordeling van opdrachten. Controleer de officiële opdrachtdocumenten voordat u beslist in te schrijven.',
  },
  fr: {
    title: 'Tenderpad · Votre voie vers les marchés publics', description: 'Trouvez des marchés publics belges adaptés à votre entreprise, examinez les sources et préparez la prochaine étape.',
    skip: 'Aller au contenu', navigation: 'Navigation principale', language: 'Langue', how: 'Comment ça marche', questions: 'Questions',
    openWorkspace: 'Ouvrir l’espace de travail', audience: 'Pour les PME belges', tagline: 'Votre voie vers les marchés publics.',
    heroCopy: 'Trouvez des marchés publics qui correspondent à votre entreprise. Comprenez ce qui les rend intéressants, examinez les sources et choisissez où consacrer votre temps.',
    tryExample: 'Explorer l’exemple', findOpportunities: 'Trouver des marchés',
    demoHelp: 'Commencez avec une entreprise fictive. Aucun numéro d’entreprise ni jeton API n’est nécessaire pour l’exemple.',
    fictionalExample: 'Exemple fictif', company: 'L’entreprise', companyActivities: 'Développement logiciel · Bruxelles & Louvain',
    opportunity: 'Un marché à explorer', exampleTitle: 'Services numériques pour les associations locales', exampleBuyer: 'Commune fictive',
    whyFit: 'Pourquoi cela peut correspondre', fitReason: 'Les services numériques correspondent aux activités de l’entreprise. L’avis est rédigé dans ses langues de travail.',
    stillCheck: 'Ce qui reste à vérifier', checkReason: 'L’avis demande deux projets comparables. Les références de l’entreprise n’ont pas encore été examinées.',
    nextStep: 'Prochaine étape', nextStepCopy: 'Lisez l’avis, vérifiez le périmètre de l’exigence et rassemblez les références pertinentes.',
    openExample: 'Ouvrir cet exemple ↗', fictionalHelp: 'L’entreprise et l’avis sont fictifs. Explorez ce même exemple dans l’espace de travail.',
    howTitle: 'D’une correspondance prometteuse à une prochaine étape claire.',
    stepOneTitle: 'Partez de votre entreprise', stepOneCopy: 'Recherchez votre numéro d’entreprise belge, puis précisez les activités, régions et langues qui comptent pour vous.',
    stepTwoTitle: 'Examinez les raisons', stepTwoCopy: 'Consultez les signaux de correspondance et les inconnues. Examinez l’avis conservé et vérifiez les documents officiels.',
    stepThreeTitle: 'Faites avancer l’examen', stepThreeCopy: 'Comparez les marchés enregistrés, ajoutez votre appréciation et vos notes, puis planifiez une prochaine action avec une date de suivi.',
    beforeStart: 'Avant de commencer', questionsTitle: 'Quelques réponses utiles.', questionsIntro: 'Tenderpad vous aide à choisir ce qui mérite un examen. Votre appréciation reste centrale.',
    faqOneTitle: 'Un score élevé signifie-t-il que je peux soumissionner ?', faqOneCopy: 'Le score classe les marchés par pertinence. L’admissibilité, la capacité et les exigences restent à vérifier dans les documents officiels du marché. Une information manquante reste inconnue.',
    faqTwoTitle: 'Puis-je essayer avant de rechercher mon entreprise ?', faqTwoCopy: 'Oui. L’espace de démonstration utilise une entreprise et trois avis fictifs. Vous pouvez examiner les sources, essayer les filtres et créer une liste de démonstration séparée de votre véritable entreprise.',
    faqThreeTitle: 'Où ma liste et mes notes sont-elles enregistrées ?', faqThreeCopy: 'Les marchés enregistrés, notes personnelles, appréciations et prochaines actions restent dans ce navigateur, séparément pour chaque entreprise. Les dates de suivi s’affichent dans l’app ; elles n’envoient pas de notifications.',
    faqFourTitle: 'Les documents sous-jacents sont-ils toujours à jour ?', faqFourCopy: 'L’espace de travail affiche des données conservées. La vérification d’un avis enregistré le compare au dossier stocké ; elle n’actualise pas la source officielle. Confirmez les dates limites et exigences actuelles avant de préparer une offre.',
    takeLook: 'Regardez de plus près', closingTitle: 'Découvrez un point de départ utile.',
    footer: 'Un prototype fonctionnel pour examiner les opportunités. Vérifiez les documents officiels avant de décider de soumissionner.',
  },
};

function initialLandingLanguage(href, saved, browser = 'en') {
  const requested = new URL(href).searchParams.get('lang');
  return [requested, saved, String(browser).split('-')[0].toLowerCase()].find(value => Object.hasOwn(landingCopy, value)) || 'en';
}
function applyLandingLanguage(language, updateUrl = false) {
  if (!Object.hasOwn(landingCopy, language)) return;
  const copy = landingCopy[language];
  document.documentElement.lang = language;
  document.title = copy.title;
  document.querySelectorAll('[data-copy]').forEach(element => { element.textContent = copy[element.dataset.copy]; });
  document.querySelectorAll('[data-copy-content]').forEach(element => { element.setAttribute('content', copy[element.dataset.copyContent]); });
  document.querySelectorAll('[data-copy-aria]').forEach(element => { element.setAttribute('aria-label', copy[element.dataset.copyAria]); });
  // Build known paths afresh: never forward token fragments or arbitrary query data.
  document.querySelectorAll('[data-workspace-link]').forEach(link => { link.href = `/?lang=${language}`; });
  document.querySelectorAll('[data-example-link]').forEach(link => { link.href = `/?example=1&lang=${language}`; });
  document.querySelectorAll('[data-landing-link]').forEach(link => { link.href = `/welcome/?lang=${language}`; });
  document.querySelector('#landing-language').value = language;
  if (updateUrl) {
    const url = new URL(location.href);
    url.search = new URLSearchParams({lang:language}).toString();
    history.replaceState(null, '', url.pathname + url.search + url.hash);
  }
}
let savedLandingLanguage;
try { savedLandingLanguage = localStorage.getItem('aanbestedingen.uiLanguage'); } catch {}
applyLandingLanguage(initialLandingLanguage(location.href, savedLandingLanguage, navigator.language));
document.querySelector('#landing-language').addEventListener('change', event => applyLandingLanguage(event.target.value, true));
