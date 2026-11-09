> **Published.** This repository is the public showcase of Tenderpad, generated from the product repository by `scripts/publish/publish-showcase.sh`; `export/published.txt` records the source commit and the sha256 of every file here. The fixtures use real, attributed open data (see `FIXTURES.md`), the file set is machine-checked in both directions, and the crate is built with the acquisition surface compiled out. No item in `PUBLICATION-REVIEW.md` remains open.

# Tenderpad — Belgian Public Procurement Matcher

[![CI Gate](https://img.shields.io/badge/CI%20Gate-deterministic%20pass-success.svg)](justfile)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Toolchain](https://img.shields.io/badge/Rust-1.88.0-orange.svg)](rust-toolchain.toml)

Tenderpad is an explainable public-procurement opportunity matching engine and review dashboard for Belgian SMEs. It ingests procurement notices, scores open opportunities against company profiles across six grounded dimensions, and provides transparent evidence for human review.

---

## ⚠️ Prototype Boundaries & Legal Disclaimer

- **Decision Support, Not Legal Advice:** Tenderpad assists human reviewers in triaging opportunities. It **never** makes automated legal eligibility conclusions, procurement approvals, or bid outcome predictions.
- **Offline-by-default & Offline Baseline:** The deterministic matching core, candidate ranking, DuckDB persistence, and interactive dashboard run completely offline with zero API keys or remote AI dependencies. Optional AI analysis providers are strictly advisory and fail closed. The crate itself is built with the `network` feature off, so the HTTP clients and the NBB, KBO, TED and BOSA fetchers are not compiled here; the sources are the product's, unchanged.
- **Real open data, attributed:** The KBO fixtures are real records for enterprise `0404.484.060` from the KBO Open Data extract (snapshot 24-06-2026, extract 401, FPS Economy), reproduced unmodified. `tests/fixtures/bda/brugge-notice.xml` is a real published notice from the Belgian e-Procurement platform (BOSA), Stad Brugge, CPV 72210000, with contact email, telephone and telefax removed and the removal declared; it drives the demo fixtures. `tests/fixtures/bda/notice.xml` is a synthetic UBL fixture with a fictional authority, kept because the store tests need its placeholder identifiers. See `FIXTURES.md` for source, dates and licence terms.
- **The example workspace is an illustration:** `web/example.js` is a self-contained walkthrough with a fictional company and fictional notices, labelled as such in the interface. Its scores are fixed illustrations, not matcher output.

---

## 👤 Attribution & Reuse

- **Author & Architecture:** Juan Benjumea.
- **License:** Released under the [MIT License](LICENSE).
- **Reference Vocabularies Provenance:**
  - **CPV 2008 & Belgian NUTS:** Derived from the European Union [OP-TED eForms-SDK release 1.15.1](https://github.com/OP-TED/eForms-SDK) (`codelists/cpv.gc` and `codelists/nuts_nuts-bel-lvl3.gc`), embedded at compile time for fast offline resolution.
  - **Matcher Tables:** Standard taxonomy mapping CPV divisions to NACE-BEL divisions, stopword filters, and calibrated term weights (`rust/data/matcher_tables.json`).
  - **Learned tables:** `rust/data/text_tfidf.json` holds 60,000 character 3–5 gram IDF weights built over 3,441 tender notices; `rust/data/winner_profile.json` holds smoothed NACE-CPV log-odds built from 9,002 award-history rows (2017–2021). Both record their provenance in the file and retain no notice text.

---

## 🚀 Quick Start

### Prerequisites
- [Rust 1.88.0](rust-toolchain.toml) (`rustup toolchain install 1.88.0`). The tree
  ships a `mise.toml` that points `RUSTUP_TOOLCHAIN` at 1.88.0, because a global
  mise config pins an older toolchain that `Cargo.lock` cannot build with. If you
  use mise, run `mise trust` once in the checkout before the first `just` call.
- [`just`](https://github.com/casey/just) command runner
- Node.js 22+ (for dashboard regression checks)

### 1. Run One-Stop Showcase Demo
Run the automated end-to-end showcase:
```bash
just demo
```
This recipe:
1. Rebuilds the DuckDB fixtures (`target/fixtures/kbo.duckdb` and `target/fixtures/tenders.duckdb`) from the published KBO CSV rows and the BOSA notice.
2. Runs the CLI ranking binary (`recommend`) to match open opportunities for enterprise `0404.484.060`.
3. Runs the API integration smoke tests on a random loopback port.

### 2. Launch Interactive Web Dashboard
Start the local server and web UI:
```bash
just serve
```
Then open [http://127.0.0.1:18000](http://127.0.0.1:18000) in your browser.
The API binds strictly to `127.0.0.1` (loopback) to prevent unauthorized network exposure.

### 3. Run the CI Verification Gate
Run the full deterministic gate:
```bash
just ci
```
This executes:
- `safety-check`: Deterministic secret, real record, and allowlist audit.
- `fmt-check`: Rust code formatting validation.
- `test`: Rust unit and integration test suite.
- `api-smoke`: End-to-end HTTP API smoke test.
- `dashboard-test`: Vanilla JS / DOM workspace regression tests.

---

## 📐 Architecture & Scoring Model

See [ARCHITECTURE.md](ARCHITECTURE.md) for full architectural documentation.

Opportunities are scored on a scale of `0` to `100` via seven weighted signals, with the weights defined in `rust/data/matcher_tables.json`:
1. **Activity Fit (37.5%):** Exact CPV match, division match, NACE-to-CPV taxonomy crosswalk, or character 3–5 gram TF-IDF cosine similarity.
2. **Geographic Fit (11.25%):** NUTS region and province hierarchy proximity.
3. **Value Fit (11.25%):** Tender value against the company's declared budget range, assessed at the smallest lot when a notice has lots.
4. **Award History (15%):** Historical SME domain presence in similar CPVs.
5. **Deadline Feasibility (10%):** Submission lead time and preparation buffer.
6. **Contractor Recognition (10%):** Category (A–V) and Class (1–8) validation under Belgian public procurement regulations.
7. **Requirement Load (5%):** How much qualification burden the notice states.

Two adjustments sit outside that weighted sum: a language match bonus or mismatch penalty, and the optional capacity triage, which subtracts 30 points when a notice states a turnover requirement and the contract value exceeds the company's annual turnover. **Annual turnover is not a scoring weight.**

No filing-age threshold applies by default, so an older filing still counts and the warning states how old it is. Passing `--max-filing-age-days N` (API `max_filing_age_days`) withholds turnover from a filing older than that limit, and from one whose age cannot be established, so the comparison becomes an explicit unknown instead of a barrier. Leaving the flag out restores the previous behaviour.

---

## 📂 Repository Layout

```text
├── LICENSE                         # MIT License
├── README.md                       # This file
├── ARCHITECTURE.md                 # System architecture documentation
├── ALLOWLIST-MANIFEST.md           # Publication audit manifest
├── SHOWCASE-REPORT.md              # Export verification and boundaries report
├── justfile                        # Showcase workflow recipes
├── rust-toolchain.toml             # Pinned 1.88.0 toolchain
├── rust/
│   ├── Cargo.toml                  # Crate manifest, 5 binaries, offline by default
│   ├── data/                       # Reference codelists and learned tables
│   └── src/                        # Core matching engine, recommendations, store
├── web/                            # Responsive multi-lingual web dashboard
├── tests/
│   ├── fixtures/                   # Real KBO rows, a real BOSA notice, synthetic UBL and eval fixtures
│   └── safety_check.cjs            # Safety and allowlist verification script
└── scripts/
    ├── generate-fixtures.sh # Seeder for the DuckDB fixtures
    └── rust-api-smoke.sh           # End-to-end API test runner
```
