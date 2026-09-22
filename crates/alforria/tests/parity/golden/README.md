# Golden TS captures (spec PARITY §7)

One JSON file per scenario check (`<check>.json`): the raw pre-normalization
capture of the TS reference run (`{named, requests, events, v2_events,
project}`). These are test vectors for CI or machines without the pinned TS
clone — they are NOT frozen `fixtures/` material and are refreshed only at a
TS re-pin.

## Golden mode

`OPENCODE_PARITY_GOLDEN=1` runs the Rust side alone and diffs its normalized
capture against the stored goldens:

```sh
OPENCODE_PARITY=1 OPENCODE_PARITY_GOLDEN=1 cargo nextest run -p opencode parity
```

## Refresh procedure

On a machine with the pinned TS clone (`/tmp/opencode-src` at the commit in
`fixtures/PINNED.md`), re-record the goldens from a live dual run:

```sh
OPENCODE_PARITY=1 OPENCODE_PARITY_RECORD=1 cargo nextest run -p opencode parity
```

Review the golden diff in the PR: golden churn means the TS side or the
normalizer changed and must be explained. Never re-generate goldens to make a
failing live run pass — a divergence there is a deviation to triage.
