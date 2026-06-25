# Independent review + revision log

A four-reviewer panel (novelty, technical-soundness-vs-artifact, evaluation,
writing/venue) reviewed `region-ownership.tex`. Each grounded its critique in
quoted text; the consolidator verified every quoted criticism against the draft
(all accurate) and verified suggested citations before adding them.

## Panel verdict

| Lens | Recommendation | Confidence |
|---|---|---|
| Novelty & related work | borderline -> weak accept | medium |
| Technical soundness (vs. artifact docs) | weak accept | high on fact-check |
| Evaluation & methodology | weak accept | medium-high |
| Writing, framing, venue | weak accept | high |

**Consensus: weak accept for an arXiv / experience venue.** The technical
reviewer fact-checked the paper against `doc/regions-agents.md`, `doc/verify.md`,
and `doc/design-decisions.md` and found *no claim the docs contradict*. The
problems were calibration and evaluation, not correctness.

## Best-fit venue

Onward! (SPLASH essays/experience) is the strongest real fit; LCTES is the
topical home but gates on a quantitative axis (the bug-finding study); EMSOFT is
plausible but skews more formal. arXiv now.

## Revision checklist

### P0 -- done (no new experiments)
- [x] Defend the unification against the RTIC/embassy coexistence kill-shot;
  pin novelty to *derivation from the sharer set* + *forced composition*
  (related work, DMA-handoff paragraph; contribution bullet 2; abstract).
- [x] Concede the sync/async asymmetry: "same obligation, distinct derived
  mechanisms," not "same concept" (S4.1, S4 intro).
- [x] Recalibrate five over-strong phrasings: "unrepresentable" -> "unguarded
  access ... in well-typed code"; "every ... volatile" + inline laundering
  caveat; "dominated by" -> "within the guard span ... non-flow-sensitive";
  "provably stable" -> "discharged as stable by IKOS"; "closed schema" -> "so
  far ... not proven minimal."
- [x] Cut the "not a proof" disclaimer from ~4 occurrences to 2 (dropped from
  S1 and the Conclusion lead; kept in abstract and S8).
- [x] Promote the frame-5 "falsified both ways" result to lead the Evaluation,
  and flag that the three bring-ups do not isolate the unification.
- [x] Name the vague invariant (LOG_SUM = 4*TICKS - 10) and split the dense
  H723 sentence.
- [x] Reorder the Conclusion to lead positive.

### P1 -- done
- [x] Surface the live false positive (continuous-copy loop + IFCR flag clear
  rejected today) in Limitations, not just as a "blind spot."
- [x] Add missing related work, verified firsthand before citing: RefinedC
  (PLDI'21), CN (POPL'23), Cogent (ASPLOS'16), embassy/embedded-dma, Ada
  Ravenscar (Burns & Wellings), verified ZynqMP DMA driver in CSL (seL4 Summit
  '25 talk). **Dropped** the reviewer's "I/O separation model, Jia/Li, S&P 2021"
  -- a web search found no such paper (likely a reviewer hallucination).
- [x] Add a "BML in one example" figure (Figure 1), real syntax condensed from
  `copy_dma.bml`, with the rejection cases in the caption.
- [x] Move the AI-provenance disclosure out of threats-to-validity to a
  de-apologized first-page footnote.

### P2 / open -- not done
- [ ] **Affiliation** still says "Independent."
- [ ] **The evaluation gap (the one real reviewer vulnerability).** The paper
  evidences runnability + one counterfactual, not the unification. Cheap,
  high-value additions, all using the boards already in hand:
  - false-positive count on the three existing bring-ups;
  - annotation-burden table (target + region/owns/claim lines vs. driver SLOC);
  - code-size / cycle overhead of the generated MPU regions and the
    volatile-vs-non-volatile-in-window lowering.
  The bigger one (the LCTES gate): a seeded or third-party bug-finding study
  showing the derived checks reject the four founding bug classes while a
  plain-C / embedded-Rust baseline compiles them clean.

## Build status

`region-ownership.pdf` rebuilds clean with Tectonic (7 pages, two-column; only
cosmetic hbox warnings, no errors; no undefined references; 19 bib entries).
