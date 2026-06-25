# Paper: Region Ownership

**Status: working draft** (not peer-reviewed, not yet submitted). The evaluation
is preliminary and being strengthened; see `REVIEW.md`.

Source for an arXiv submission on BML's single defensible new contribution: the
unification of the CPU priority-ceiling protocol and the DMA release/reclaim
handshake under one compile-time "region ownership" model.

- `region-ownership.tex` -- self-contained LaTeX (`article` class, two-column).
  No exotic class files; builds in any TeXLive.

## Build

```bash
# macOS: a minimal TeX is enough
brew install --cask basictex          # ~100 MB, vs ~4 GB for mactex
eval "$(/usr/libexec/path_helper)"    # put tlmgr/pdflatex on PATH
sudo tlmgr update --self
sudo tlmgr install microtype          # if the build complains

pdflatex region-ownership.tex
pdflatex region-ownership.tex         # second pass for references
```

`latexmk -pdf region-ownership.tex` does both passes if you have it.

## Submitting to arXiv (you do this, not the tooling)

arXiv submission is permanent and public (papers are indexed and cached even if
later withdrawn), so this step is deliberately manual.

1. Account + endorsement. You need an arXiv account. The likely primary category
   is **cs.PL** (Programming Languages); **cs.OS** / **cs.SE** are reasonable
   cross-lists. cs.PL may require an endorsement if you have not posted there
   before.
2. Upload the **source**, not just a PDF -- arXiv prefers to compile LaTeX
   itself. Upload `region-ownership.tex` alone (the bibliography is inline via
   `thebibliography`, so there is no `.bib` to include).
3. Title/abstract on the web form must match the paper. License: arXiv's default
   (non-exclusive) is fine and keeps the Apache-2.0 code unaffected; CC BY is an
   option if you want it reusable.

## Build status

`region-ownership.pdf` builds clean with Tectonic (6 pages, two-column). Only a
cosmetic underfull-hbox warning, no errors.

## Open decisions before submitting

- **Affiliation.** The author block says "Independent." Change if you have one.
- **Evaluation.** The strongest reviewer objection (stated honestly in the
  Limitations section) is that the model was validated on programs it was
  co-designed with. A bug-finding pass over independent third-party Cortex-M
  drivers would materially strengthen the claim. This is the highest-value
  remaining work item.

## Resolved during the related-work pass

- **Monniaux (EMSOFT 2007)** and **PISTIS** (Grisafi, Ammar, Roveri, Crispo,
  USENIX Security 2022) -- the two closest "static-analysis / compiler + DMA"
  neighbors -- are now read firsthand and cited (refs [8], [9]). Monniaux states
  our founding observation (you cannot verify a driver without modeling the
  asynchronous device); the new related-work paragraph cites it and distinguishes
  it.
- **Singularity citation.** Kept as a paraphrase, not a verbatim quote: the exact
  "DMA is the one unsafe aspect" wording could not be confirmed against the
  primary source (the MSR tech report PDF was unreachable). The paraphrase is
  accurate and corroborated; do not promote it to a direct quote without
  confirming the wording.
