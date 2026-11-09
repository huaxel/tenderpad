# Tenderpad Technical Architecture

Tenderpad is an open-source, deterministic opportunity matching engine and review dashboard for Belgian public procurement, designed specifically to assist small and medium-sized enterprises (SMEs) in navigating complex public tenders.

---

## 1. Core Principles & Boundaries

- **Human Review Support, Not Legal Advice:** Tenderpad identifies potential opportunities and highlights qualification risks (e.g. required recognition classes, annual turnover thresholds, or ISO certificates). It **never** issues a legal eligibility conclusion or guarantees bid success.
- **Offline & Air-Gapped by Default:** The core matching engine, scoring pipeline, local database, and dashboard operate completely offline without requiring API keys, cloud credentials, or external AI models.
- **Transparent, Grounded Ranking:** Ranking is computed using an explainable, deterministic scoring formula across 6 explicit dimensions. Every recommendation includes human-readable reasons and score breakdown components.

---

## 2. System Architecture

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        Tenderpad Dashboard                             │
│       Vanilla JS/HTML5 Workspace (web/index.html & web/workspace.js)   │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │ HTTP (Loopback 127.0.0.1:18000)
┌───────────────────────────────────▼────────────────────────────────────┐
│                    API Gateway (rust/src/bin/api.rs)                   │
│   - Endpoints: /health, /companies, /recommendations, /preferences     │
│   - Static Asset Serving: /web                                         │
│   - Authentication: Loopback-unrestricted; Bearer token for remote     │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼────────────────────────────────────┐
│                  Recommendation & Matching Core                        │
│   - rust/src/recommendations.rs: Candidate selection & version collapse│
│   - rust/src/lib.rs: 6-signal scoring engine & breakdown calculation   │
│   - rust/src/cpv.rs & text_tfidf.rs: Taxonomy lookup & text similarity │
│   - rust/src/recognition.rs: Belgian contractor recognition triage     │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
┌───────────────────────────────────▼────────────────────────────────────┐
│                  Persistence Layer (rust/src/store.rs)                 │
│   - Embedded DuckDB (tenders.duckdb, kbo.duckdb)                       │
│   - Analytical SQL queries for rapid candidate retrieval               │
│   - Local preference storage, feedback history, and analysis cache     │
└────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Scoring & Ranking Formulas

Every candidate tender $T$ evaluated against company profile $C$ receives an overall match score $S \in [0, 100]$ composed of weighted terms:

$$S = \sum_{i} w_i \cdot s_i(C, T)$$

Where weights $w_i$ are defined in `rust/data/matcher_tables.json`:

| Signal ($s_i$) | Weight ($w_i$) | Description |
|---|---|---|
| **Activity Fit** | 0.375 | Exact CPV code match, division-level match, NACE-to-CPV crosswalk, or character 3–5 gram TF-IDF cosine similarity fallback. |
| **Geographic Match** | 0.1125 | NUTS code hierarchy alignment (province / region / national). |
| **Value Fit** | 0.1125 | Tender value against the company's declared budget range; assessed at the smallest lot when the notice has lots. |
| **Award History** | 0.15 | Historical presence in similar CPVs or prior public contracts. |
| **Deadline Feasibility** | 0.10 | Submission lead-time feasibility. |
| **Contractor Recognition** | 0.10 | Category (A–V) and Class (1–8) validation for Belgian works tenders. |
| **Requirement Load** | 0.05 | How much qualification burden the notice states. |

The weights sum to 1.0. Two adjustments are applied after the weighted sum and are
**not** part of it: a language match bonus or mismatch penalty, and the optional
capacity triage (`apply_capacity_triage`), which subtracts 30 points when a notice
states a turnover requirement and the contract value exceeds the company's annual
turnover. Annual turnover therefore never enters $w_i$; it is a separate,
opt-in triage step.

### Notice Version Collapsing & Lot Triage
Belgian procurement notices frequently undergo corrigenda and version updates. The store deduplicates and aggregates versions under the same contract folder, ensuring that only the most recent active version is recommended, while preserving individual lot CPV codes, geographic locations, and financial criteria.

---

## 4. Reference Vocabularies & Codelists

1. **CPV (Common Procurement Vocabulary 2008):**
   - Source: European Union OP-TED eForms-SDK release 1.15.1 (`codelists/cpv.gc`).
   - Embedded at compile time via `include_str!` in `rust/src/cpv.rs` for instant offline label lookups.
2. **NUTS (Nomenclature of Territorial Units for Statistics):**
   - Source: European Union OP-TED eForms-SDK release 1.15.1 (`codelists/nuts_nuts-bel-lvl3.gc`).
   - Embedded at compile time for Belgian provincial and regional hierarchy mappings.
3. **Matcher Tables:**
   - Source: `rust/data/matcher_tables.json`.
   - Maps CPV divisions to NACE-BEL divisions that typically supply them, excludes overly generic wholesale divisions (46/47), and defines stopword filters.

---

## 5. Security & Isolation Model

- **Loopback Default:** The REST API binds strictly to `127.0.0.1` by default and refuses to bind external interfaces unless an explicit security token `AANBESTEDINGEN_API_TOKEN` is configured.
- **Fail-Closed AI Advisory:** The optional analysis provider (`analysis.rs`) provides local deterministic justifications by default. Any remote model provider remains disabled unless explicitly configured, and responses must adhere strictly to validated schemas or fail closed.
