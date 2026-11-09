# Fixture provenance

The Rust test fixtures are real open data, reproduced with attribution. The
in-dashboard example workspace is a separate, explicitly labelled illustration.

## KBO company records

`tests/fixtures/kbo/*.csv`

- **Source:** KBO Open Data, FPS Economy (Belgian Crossroads Bank for Enterprises).
- **Snapshot date:** 24-06-2026. **Extract timestamp:** 25-06-2026 09:17:28.
  **Extract number:** 401.
- **Records:** the rows for enterprise `0404.484.060` (SOFT AND INFORMATICS
  COMPANY, also registered as SOFICO), reproduced unmodified: enterprise,
  both denominations, the registered address in Gent, the activity rows, and
  the establishment unit.
- **License:** KBO Open Data reuse terms. Article 2.7 requires the data to stay
  unchanged and keep its meaning, which is why no field is edited here.
  Article 2.8 requires the source and the update date, which is the block above.
- **Not included:** no other enterprise is copied, and no personal data of
  natural-person businesses is present in this extract.

## Procurement notice (real, drives the demo)

`tests/fixtures/bda/brugge-notice.xml`

- **Source:** Belgian e-Procurement platform (BOSA), publication workspace
  `e7aefb33-defc-4485-9d4e-58ad7f3f75a1`.
- **Notice:** dossier `BRUGGE-2026-037-TL-software autoluw`, published
  2026-10-22, contracting authority Stad Brugge, CPV 72210000, lot `LOT-0001`.
- **Modification:** the `cbc:ElectronicMail`, `cbc:Telephone` and `cbc:Telefax`
  elements were removed. The notice itself carries no personal names; those
  three elements held an institutional mailbox and the switchboard numbers.
  The modification is declared in a comment at the top of the file.
- **Reuse:** procurement notices published in the Supplement to the Official
  Journal and on the national platform are reusable under the Commission reuse
  policy; the modification is stated as that policy requires.
- **Used by:** `scripts/generate-fixtures.sh`, so `just demo` and `just
  api-smoke` rank a real notice.

## Procurement notice (synthetic, drives the unit tests)

`tests/fixtures/bda/notice.xml`

- **Source:** the product repository, copied unchanged.
- **What it is:** a synthetic UBL contract notice with placeholder identifiers
  (`FIXTURE-0001`, `fixture-folder-0001`) and a fictional contracting authority.
  The product's `store` tests rewrite those placeholders to build a second
  version, so the file has to keep that shape.
- **Why it is here:** `store::tests::source_distinct_folders_survive_version_selection`
  reads it. Substituting the Bruges notice made two distinct source folders
  collapse into one row, which is the behaviour that test exists to catch.
- **Personal data:** none. The municipality name is fictional, including in the
  product.

## Learned tables

Copied unchanged from the product. They are aggregate statistics, not documents:

- `rust/data/text_tfidf.json`: char n-gram IDF weights over 3,441 tender
  notices. No notice text is retained, only n-gram frequencies.
- `rust/data/winner_profile.json`: learned winner-profile affinities over 9,002
  history rows, keyed by CPV and NACE.

These two are why the crate behaves here exactly as it does in the product
rather than approximating it.

## Evaluation corpus

`tests/fixtures/eval/*.json`

- **What it is:** a pilot collection (10 procedures) and a holdout collection
  (30 procedures) with CPV codes, regions, values, deadlines and review
  records. The Rust evaluation contract reads them, and the isolation test
  checks that the two collections stay disjoint.
- **Synthetic, verified:** the procedure keys look like TED references but are
  not. TED's `publication-number` field only accepts `NNN-YYYY`, and free-text
  searches for two of the corpus titles return zero notices while a control
  query returns 15,576. The procedures are constructed, not captured.
- **Buyer names:** the original corpus named 30 real Belgian public bodies
  (Stad Gent, FOD BOSA, MIVB/STIB and so on) against fabricated tenders. Those
  are replaced with fictional counterparts of the same organisation type, and
  both manifests now say `(synthetisch)` in their dataset name.
- **Why it ships:** the evaluation contract's isolation test reads these files.
  A substitute corpus would change what that test proves.

## Example workspace (not a fixture)

`web/example.js` is a self-contained walkthrough that never calls the API. It
uses a fictional company and fictional notices, and says so in the UI
("Fictional company and notices"). Its scores are fixed illustrations, not
output of the matcher. It is not derived from the KBO extract or from a
published notice.
