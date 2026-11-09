# Tenderpad Showcase: publication review

**Verdict: published at https://github.com/huaxel/tenderpad. No blockers remain.**
The fixture contamination is gone. The allowlist enforces itself. And the crate
no longer compiles the data-acquisition surface: `network` is a feature, on by
default in the product and off here, so this repository publishes the offline
configuration of the same source rather than a trimmed copy of it.

The last open question was whether the learned tables and the evaluation
corpus stay published. Both do, and the corpus turned out to be synthetic; the
evidence is in the section below.

Reviewed 2026-11-08. `just ci` passes: 527 Rust tests, API smoke, 103 dashboard
tests, safety check, export check. The product's `just all-check` passes too,
including the offline configuration.

---

## Resolved: real entities in the fixtures

The previous fixtures fabricated a tender attributed to **Stad Hasselt**, used a
real street address in that city, and carried a checksum-valid enterprise number
(`0895.492.518`) that is indistinguishable from a real KBO registration.

They are replaced with real, attributed data. See `FIXTURES.md` for the full
provenance.

| Artifact | Now | Attribution |
|---|---|---|
| `tests/fixtures/kbo/*.csv` | unmodified KBO rows for `0404.484.060` | KBO Open Data, snapshot 24-06-2026, extract 401, FPS Economy |
| `tests/fixtures/bda/brugge-notice.xml` | real BOSA notice, Stad Brugge, CPV 72210000; drives the demo fixtures | BOSA workspace `e7aefb33-...`, published 2026-10-22 |
| `tests/fixtures/bda/notice.xml` | the product's synthetic UBL fixture, now attributed to a fictional municipality | none needed; contains no real entity |
| `web/example.js` | unchanged fictional walkthrough, labelled as such | not derived from either source |

The notice had `cbc:ElectronicMail`, `cbc:Telephone` and `cbc:Telefax` removed
(institutional mailbox and switchboard numbers, no personal names) and the
removal is declared in a comment inside the file. The KBO rows are unmodified,
as the licence's "unchanged meaning" clause requires.

`tests/fixtures/bda/notice.xml` is the one fixture this repository takes from the
product unchanged. It is synthetic, but it previously attributed a fabricated
tender to a real municipality; that name is now fictional in the product too, so
the file carries no real entity and the product's unit tests still pass.

Verified end to end: `just demo` ranks the Bruges notice for SOFT AND
INFORMATICS COMPANY, `bda-ingest` stores the notice, and `kbo-rebuild` imports
the company.

---

## Resolved: the allowlist now enforces itself

`tests/safety_check.cjs` previously checked that `ALLOWLIST-MANIFEST.md` existed
and printed OK. It never compared the tree to the manifest, so the manifest could
not fail and 23 files were unlisted.

The file set is now declared in two machine-checked lists and the check runs on
every gate:

| File | Meaning |
|---|---|
| `scripts/publish/published-paths.txt` | every product path that is published, one per line, no directory catch-alls |
| `showcase/` | the overlay files, each stored at the path it is published under |
| `export/published.txt` | generated: every published path with its sha256, plus the source commit |

The publish script prints the current counts (`N product paths, M overlay
files`); this table deliberately states none, because a number here drifts the
moment a file is added.

The tree cannot grow unreviewed, and `tests/safety_check.cjs` proves it in both
directions on every gate: a file present but unlisted fails, and a listed file
that is missing fails. Adding a file therefore means adding it to one of the two
lists, which is the review.

The publish script also records whether it built from a commit or from a dirty
working tree, so an artifact never claims a commit its content does not match.

## Resolved: the acquisition surface is a feature, not a fork

The crate used to compile its HTTP clients and fetchers unconditionally, so
"offline by default with zero API keys" described the binaries, not the
artifact. Deleting the modules was the alternative, but that is how the eleven
divergent `rust/src` files arose in the first place: a trimmed copy that drifts.

Instead the product gained a `network` feature, on by default there and off
here. It gates `http_retry`, `service_calls`, `kbo_live`, `nbb_api`, the bundle
assembly that calls them, the `TedClient` and `BosaClient` impls, the award XML
fetch, and the fifteen binaries that drive them. `rust/src/lib.rs` is the
product's file, unchanged.

The product's gate gained `just offline-check`, which runs the same crate with
`--no-default-features` through check, test and clippy. The offline
configuration therefore cannot rot between publications: it is a gate, not a
promise.

What the feature does not cover is the LLM analysis path in `analysis.rs`. It is
a documented API capability, it is inert without a configured key, and removing
it would remove a feature the showcase exists to demonstrate. The README says
so instead of implying the crate has no outbound code at all.

## Resolved: the crate is the product's source, not a copy of it

Sixteen `rust/src` files used to differ from the product, 960 changed lines,
mostly the NBB capacity report deleted from `store.rs`. Nothing is overridden
now: all 84 published `rust/src` paths are the product's bytes, and all 42
binaries ship rather than five.

The two former overrides are gone because the product changed instead.
`rust/src/bosa.rs` and `rust/src/award_xml.rs` asserted against a synthetic F03
notice; the product's F03 fixture now names `nv Voorbeeld Leverancier` and
declares the substitution in the file, so the code and the fixture are the
product's.

Exactly one rewrite remains, in `scripts/publish/publish-showcase.sh`:
`default = ["network"]` becomes `default = []`. The product keeps the network
on by default; the published crate must not. The script fails loudly if that
line is not found, rather than publishing a crate that silently keeps the
fetchers.

---

## Resolved: codelist attribution

`rust/data/cpv.json` and `rust/data/nuts-bel.json` are vendored from the OP-TED
eForms-SDK, whose repository licence is CC BY 4.0 rather than MIT. `NOTICE` now
carries what that licence requires: the creator (the Publications Office of the
European Union, which manages the project), the licence notice and link, the URI
of the licensed material at the vendored release, and a description of the
extraction. `LICENSE` states that the two data files are outside the MIT grant,
and `rust/data/codelists-README.md` repeats the pointer.

The facts were checked against upstream, not inferred: the `LICENSE` file at
`OP-TED/eForms-SDK` is the CC BY 4.0 text, release `1.15.1` exists and was
published 2026-07-20, and the source files are `codelists/cpv.gc` and
`codelists/nuts_nuts-bel-lvl3.gc`.

---

## Resolved: the learned tables and the evaluation corpus

The projection ships two learned tables and one evaluation corpus. They are not
documents and they are not real tenders.

`rust/data/text_tfidf.json` and `rust/data/winner_profile.json` are aggregate
statistics: char n-gram IDF weights over 3,441 notices, and CPV/NACE affinity
weights over 9,002 history rows. No notice text is retained.

The evaluation corpus was the open question, because its procedures carry
titles, buyers and descriptions. It is synthetic. TED's `publication-number`
field only accepts `NNN-YYYY`, which the corpus keys do not fit, and free-text
searches for two of its titles return zero notices while a control query returns
15,576. The corpus had named 30 real Belgian public bodies against fabricated
tenders; those names are now fictional counterparts of the same organisation
type, and both manifests say `(synthetisch)` in their dataset name.

---

## Minor

1. `tests/fixtures/bda/brugge-notice.xml` keeps the real postal code and NUTS
   codes of Bruges, which is correct now that the notice is real.
2. `rust/tests/fixtures/synthetisch-bestek.txt` is a synthetic specification with
   a fictional authority. It moved out of the product's `docs/` tree into the
   crate's fixture directory, so the product and this repository read the same
   path.
3. Fonts ship with their OFL text files (`web/fonts/OFL-*.txt`).
4. The product's internal name appears in the source of modules this crate does
   not compile: `nbb_api` sends `User-Agent: ehb-fin-aanbestedingen/<version>`,
   and `service_calls` writes under `~/.local/state/ehb-fin-aanbestedingen/`.
   Renaming those is a product change with its own consequences (live request
   identity, existing state directories), so the sources ship verbatim and the
   inconsistency is recorded here instead. Neither module is built in this
   crate.

## Order of work

Done. The tree was published with
`scripts/publish/publish-showcase.sh --ref c745c44 --gate --push`, so it mirrors
a reviewed commit and its own gate ran before it left.

Republishing after a product change is the same command with a new `--ref`. Pin
the sha rather than `HEAD`: this product repository is worked on by more than one
session, and `HEAD` moves.

What remains is outside this repository: the portfolio card still describes the
source as private and carries no link.

## Reproduce

```bash
cd tenderpad-showcase
just ci
just demo
scripts/publish/publish-showcase.sh --out /tmp/showcase --gate
node tests/safety_check.cjs

# The product's side of the same contract
cd ../EHB/ehb-fin-aanbestedingen
just offline-check
```
