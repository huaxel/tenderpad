> **Published.** This repository is the public showcase of Tenderpad, generated from the product repository by `scripts/publish/publish-showcase.sh`; `export/published.txt` records the source commit and the sha256 of every file here. The fixtures use real, attributed open data (see `FIXTURES.md`), the file set is machine-checked in both directions, and the crate is built with the acquisition surface compiled out. No item in `PUBLICATION-REVIEW.md` remains open.

# Tenderpad Portfolio Showcase Report

## 1. Executive Summary

A fully runnable local showcase of **Tenderpad** (Belgian Public Procurement SME Matcher), generated from the product repository by `scripts/publish/publish-showcase.sh`.

- **Generated, not rewritten:** The crate here is the product's own source. The publish script copies 136 product paths and adds 13 overlay files; it rewrites one line, `default = ["network"]` to `default = []`. No module is trimmed, stubbed or reimplemented.
- **Local & Offline Operation:** Runs 100% locally and offline without live credentials, cloud services, or external AI models. The API binds strictly to `127.0.0.1` (loopback). The HTTP clients and the NBB, KBO, TED and BOSA fetchers are not compiled here.
- **Ranking Fidelity:** The production 6-signal scoring model (`activity_match`, `region_match`, `value_match`, `history_match`, `deadline_match`, `recognition_match`, and candidate ranking) is the product's, unmodified, including the learned tables it reads.
- **Privacy & Safety Discipline:** Provenance and the declared modifications are recorded in `FIXTURES.md`; the publication audit, with its evidence, is `PUBLICATION-REVIEW.md`, and no item in it remains open.
- **Verification:** 527 offline Rust unit/integration tests, 103 Node.js dashboard tests, the end-to-end API smoke test, and a safety check that verifies every file against `export/published.txt` all pass.

---

## 2. What Ships

### Core modules, the product's own source
| File / Module | Rationale |
|---|---|
| `rust/src/lib.rs` | Complete 6-signal scoring formulas, breakdown data models, warning terms, and tie-breaking logic. |
| `rust/src/main.rs` | CLI entry point accepting a `MatchRequest` JSON document on stdin and returning scored results. |
| `rust/src/recommendations.rs` | Candidate selection, DuckDB analytical retrieval, notice-version collapsing, lot triage, and reasons generation. |
| `rust/src/store.rs` | Full DuckDB analytical schema, persistence layer, queries, and advisory analysis cache. |
| `rust/src/analysis.rs` | Deterministic explanation generator (`deterministic-rust`) and fail-closed AI interface. |
| `rust/src/cpv.rs` | Fast offline CPV code resolution and Belgian NUTS regional hierarchy lookups. |
| `rust/src/text_tfidf.rs` | Character 3–5 n-gram TF-IDF cosine similarity text matcher. |
| `rust/src/winner_profile.rs` | NACE-CPV log-odds affinity matrix calculation. |
| `rust/src/kbo.rs` & `kbo_build.rs` | Enterprise number normalization, KBO schema, and DuckDB compiler from CSV tables. |
| `rust/src/recognition.rs`, `recognition_catalog.rs`, `recognized_contractors.rs` | Belgian public works contractor recognition categories (A–V) and classes (1–8) logic. |
| `rust/src/requirements.rs` | Notice requirement and tender criteria extraction. |
| `rust/src/bda.rs` | eForms UBL ContractNotice XML parser and DuckDB ingester. |
| `rust/src/awards.rs` & `bxl_inventory.rs` | Award history and Brussels public contract inventory domain models. |
| `rust/src/cross_source.rs` | Multi-source notice matching and dossier linking. |
| `rust/src/ted.rs` | In-memory notice-to-tender data normalizer. |
| `rust/src/bosa/evidence.rs` | Conservative keyword and attachment signal extraction. |
| `rust/src/bin/api.rs` | Loopback HTTP REST API and static web dashboard server. |
| `rust/src/bin/recommend.rs` | CLI opportunity recommendation runner for SMEs. |
| `rust/src/bin/kbo-rebuild.rs` | CLI fixture compiler building KBO DuckDB from CSVs. |
| `rust/src/bin/bda-ingest.rs` | CLI notice compiler ingesting eForms XML into DuckDB. |
| `rust/src/bin/score-tender.rs` | CLI tool scoring individual tenders. |
| `rust/src/bin/feedback.rs` & `preferences.rs` | Feedback recording and SME preferences management. |
| `rust/src/bin/report.rs` | Store reporting and metrics inspection tool. |
| `web/*` | Full multi-lingual web dashboard and workspace state machine. |

### Compiled out, not removed
| Component | How it is excluded |
|---|---|
| Network surface: `nbb_api`, `kbo_live`, `service_calls`, `http_retry`, and the fetch binaries (`ted-ingest`, `ted-search`, `bosa-ingest`, `bosa-search`, `bosa-dossier`, `bosa-documents`, `nbb-fetch`, `nbb-bundle`, `nbb-import`, `pras-rebuild`, `inventory-ingest`) | Behind the `network` feature, which the publish script turns off by default. The sources ship unchanged; `cargo test --no-default-features` is the published configuration. |

### Not published
| Component | Rationale |
|---|---|
| `infra/runner-controller/` | Cloud VM orchestration for private isolated CI runners; irrelevant locally. |
| `docs/final-work/` and academic operational notes | Confidential bachelorproef drafts and evaluations. |
| The product's gitignored `data/` tree | Captured provider data; the fixtures the tests need are published instead. |

### The F03 award notice
The product's `tests/fixtures/bosa/bosa-2017-stad-gent-f03.xml` is published as-is: real notice structure, with the winning contractor replaced by `nv Voorbeeld Leverancier` and the substitution declared in the file. The earlier mechanism kept a separate synthetic copy; that is gone.

---

## 3. Data & Vocabulary Provenance

1. **Codelists (`cpv.json`, `nuts-bel.json`):**
   - Public open data from European Union OP-TED eForms-SDK release 1.15.1 (`codelists/cpv.gc` and `codelists/nuts_nuts-bel-lvl3.gc`). Embedded via `include_str!` for instantaneous offline resolution.
2. **Matcher Tables (`matcher_tables.json`):**
   - Curated domain taxonomy mapping CPV divisions to supplying NACE-BEL divisions, stopword lists, and calibrated scoring weights.
3. **Learned tables (`text_tfidf.json`, `winner_profile.json`):**
   - Real, and the product's own. `text_tfidf.json` carries 60,000 char 3–5 gram IDF weights built over 3,441 tender notices (`built_by` in the file); `winner_profile.json` carries smoothed NACE-CPV log-odds built from 9,002 award-history rows, 8,493 training pairs, 2017–2021. Both record their provenance in the file, and neither retains notice text. They are why the ranking here is the product's ranking.
4. **Test Fixtures (`tests/fixtures/`):**
   - `tests/fixtures/bda/brugge-notice.xml`: real published BOSA ContractNotice (Stad Brugge, CPV 72210000, published 2026-10-22), with contact email, telephone and telefax removed and declared in the file. Drives the demo fixtures.
   - `tests/fixtures/bda/notice.xml`: the product's synthetic UBL fixture, copied unchanged; the store tests need its placeholder identifiers.
   - `tests/fixtures/kbo/*.csv`: real KBO Open Data records for `0404.484.060` (snapshot 24-06-2026, extract 401), reproduced unmodified.
   - `rust/tests/fixtures/bosa-synthetic-f03.xml`: synthetic legacy F03 notice (`Gemeente Voorbeeldstad`).
   - `rust/tests/fixtures/synthetisch-bestek.txt`: synthetic tender specification with trap criteria.
   - `tests/fixtures/eval/manifest.json` & profiles: synthetic 10-procedure evaluation manifest.
   - Full provenance, licence terms and the declared modification: `FIXTURES.md`.

---

## 4. Verification Results

All gates were executed and verified locally:

1. **Deterministic Safety Check (`tests/safety_check.cjs`):**
   - Scans for forbidden strings, emails and database files, and verifies every file in the tree against `export/published.txt` in both directions: an unlisted file fails, and a listed file that is missing fails.
   - **Result:** `PASS` (0 violations, all 146 files listed).
2. **Rust Unit & Integration Tests (`cargo test --manifest-path rust/Cargo.toml --lib`):**
   - **Result:** `527 passed; 0 failed; 0 ignored`.
3. **Web Dashboard Unit & Regression Tests (`node --test scripts/dashboard-*.test.cjs`):**
   - **Result:** `103 passed; 0 failed; finished in 71ms`.
4. **End-to-End API Smoke Test (`scripts/rust-api-smoke.sh`):**
   - Verified `/health`, `/companies/{id}/recommendations`, `/feedback`, preferences, and deterministic advisory `/analysis` cache on random high loopback ports.
   - **Result:** `PASS`.
5. **Showcase Demo (`just demo`):**
   - Rebuilt the DuckDB fixtures from the KBO CSV rows and the BOSA notice, ranked opportunities via CLI, and validated the API smoke test.
   - **Result:** `PASS`.

---

## 5. Prototype Boundaries & Known Limitations

- **Advisory Only:** Tenderpad assists human triage and highlights missing evidence or requirements (e.g. required contractor recognition or turnover criteria). It does **not** provide legal conclusions or predict winning probabilities.
- **Offline-by-default Operation:** Live ingestion is compiled out; all data flows from local DuckDB databases built from the published fixtures.
- **DuckDB Exclusivity:** The persistence layer requires DuckDB (bundled). No external database service or cloud instance is needed.
