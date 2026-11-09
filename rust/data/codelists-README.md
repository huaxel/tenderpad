# Reference codelists (`cpv.json`, `nuts-bel.json`)

Human labels for the codes TED notices carry. Embedded at compile time via
`include_str!` (see `src/cpv.rs`) — binaries work offline.

- **Licence:** CC BY 4.0, not MIT. Attribution, licence link and the
  description of the extraction are in the repository's `NOTICE`.
- **Source:** OP-TED eForms-SDK release **1.15.1** (2026-07-20),
  `codelists/cpv.gc` (CPV 2008, 9,454 codes) and
  `codelists/nuts_nuts-bel-lvl3.gc` (45 Belgian NUTS regions).
- **Shape per code:** `{Name (en), parentCode (cpv only), nld_label,
  fra_label}`. Full fidelity kept in the files; the Rust structs only
  deserialise what code reads today.
- **Regen:** download both `.gc` Genericode files from
  `https://github.com/OP-TED/eForms-SDK/tree/main/codelists`, extract the
  columns above per `<Row>`.
- **Coverage (2026-10-23):** all 1,396 distinct CPVs in `data/qc-seed.duckdb`
  resolve; `just rust-report` prints `cpv_unmapped: 0`. If that counter ever
  goes non-zero on live data, the SDK moved — re-vendor, don't patch codes.
