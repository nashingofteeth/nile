# nile audit state

Pin: `<pin>`
Started: `<date>`
Queue: `queue.md` (frozen at the run's start)

## Coverage

One row per `### S…` heading in the frozen queue; `Status` is `pending`,
`done`, or `partial (blocked on …)`.

| Slice | Slug | Status | Report |
|---|---|---|---|
| S00 | baseline | pending | — |

## Findings ledger

One line per finding, copied from the slice reports: id, severity, `file:line`,
status.

## Verified clean

One line per area a slice checked and cleared, with the evidence named.
