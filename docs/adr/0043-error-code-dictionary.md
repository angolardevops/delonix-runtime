# ADR-0043: A dictionary of numbered codes — `DX-CDNN`

- **Status:** Accepted (2026-09-17)
- **Date:** 2026-09-17
- **Deciders:** Walter (owner)
- **Builds on, does not reopen:** the exit-code classes and the `DX_*` identities
  (`crates/foundation/delonix-model/src/exitcode.rs`, `error.rs`, `docs/cli-stability.md`);
  ADR-0040 P3 (errors per crate, mapped to the codes the foundation owns); ADR-0042 (the
  node API reports errors as RFC 9457 problem+json).

## Context

Two identities exist today, and neither answers «which failure was this?»:

- **The exit code** is a *class*: `4` is «no such resource», whatever the resource.
  There are ten numbers (`0 1 2 3 4 5 69 74 77 124`) and a process exit code cannot hold
  more than 255, so it can never name a specific failure.
- **The `DX_*` string** is the same class in another spelling (`DX_NOT_FOUND`), for callers
  that read text.

The message names the failure, but the message is translated (`--l18n=pt`), so neither a
person reading a log from another locale nor a script can rely on it. The owner asked for a
dictionary: **a number that, once seen, says what happened**, for success, invalid input and
failures alike.

Measured before deciding (2026-09-17, `origin/main`): ~460 call sites in the adapters still
build the shared error with a free-text message (246 `Invalid`, 143 `Runtime`), and 50–90
sites `match` directly on a variant of the shared error.

## Decision

### D1. One number per concrete failure, `DX-CDNN`

A code is four digits:

| digit | meaning |
|---|---|
| `C` (thousands) | the **class** — what the caller does next |
| `D` (hundreds) | the **domain** — where it happened |
| `NN` | the failure within that class and domain, `01`–`99`; `00` is the class itself |

**Classes** (`C`), each with the exit code it answers:

| `C` | class | exit | `DX_*` |
|---|---|---|---|
| 0 | success / notice | `0` | — |
| 1 | invalid argument | `1` | `DX_INVALID_ARGUMENT` |
| 2 | invalid usage / changes pending | `2` | — |
| 3 | exists, not running | `3` | `DX_NOT_RUNNING` |
| 4 | no such resource | `4` | `DX_NOT_FOUND` |
| 5 | conflict — already exists | `5` | `DX_CONFLICT` |
| 6 | unavailable on this host | `69` | `DX_UNAVAILABLE` |
| 7 | permission denied | `77` | `DX_PERMISSION_DENIED` |
| 8 | deadline passed | `124` | `DX_TIMEOUT` |
| 9 | system failure (syscall, I/O, state, registry) | `1` or `74`, per entry | `DX_SYSCALL_FAILED`, `DX_IO`, `DX_INVALID_STATE`, `DX_REGISTRY` |

Class 9 is the one class whose exit code is not implied by the digit: ten exit codes and
eleven `DX_*` identities do not fit ten digits, and splitting «the kernel said no» by the
number a shell happens to see would put `EIO` and `EPERM`-from-a-syscall in different
thousands for no reason a reader can use. The dictionary entry says which.

**Domains** (`D`):

| `D` | domain |
|---|---|
| 0 | engine / general |
| 1 | container, pod |
| 2 | volume, storage |
| 3 | network |
| 4 | image, build, scan |
| 5 | virtual machine |
| 6 | stack, manifest, compose |
| 7 | cluster, CRI |
| 8 | secret, security |
| 9 | host, CLI, tools |

`DX-4201` is therefore «no such volume», and anyone who has learnt the table reads «not
found, storage» before opening the dictionary.

### D2. The number never changes meaning

A code may be **added**. An existing code never changes class, domain or meaning, and is
never reused — a failure that stops existing keeps its number as *retired*. The message text
may be reworded and is translated; the number is the contract. `DX-C000` always exists and
stands for «this class, no more specific entry yet», which is what every call site still
building the shared error with free text reports until it gets its own entry.

### D3. One table in code, everything else generated from it

The dictionary is a static table in `delonix-model` (`codes`): number, stable id
(`volume.not_found`), class, domain, the English message, what it means and what to do.
Tests enforce the invariants: numbers unique and sorted; the class digit matches the class;
the domain digit matches the domain; the exit code of an entry is its class's (or, for class
9, one of the two); no `NN = 00` except the generic class entry. Portuguese goes through the
translation catalogue, like every other user-facing string.

Generated from that table, with a gate that fails on drift:

- `delonix explain DX-4201` (also `4201`): the message, meaning and remedy, in the chosen
  language;
- a documentation page with the whole dictionary;
- the `code` of an error in `-o json` output and in the node API's problem+json.

### D4. The number travels inside the error

The CLI prints `error[DX-4201]: no such volume db`. For that, the shared error must carry the
specific code from the crate that raised it to the printer, **without** changing its class.
Each crate's own error converts into the shared class and attaches its code; the class,
the `DX_*` identity and the exit code are read through it unchanged.

Because a wrapped error no longer matches `Err(Error::NotFound(_))`, and that mismatch is
silent, the sites that match on a variant move to accessors on the error (`class()`,
`is_not_found()`, …) **before** any code is attached. A ratchet counts direct variant patterns
outside the foundation and only goes down.

### D5. Order of delivery

1. This ADR; the `codes` table with the ten generic class entries; `Error::number()`;
   `delonix explain`.
2. The CLI error line, the `-o json` field and the generated page.
3. Variant matches replaced by accessors; the ratchet.
4. Codes carried inside the error; the scanner and the volume store get their specific
   entries.
5. The remaining crates, one per change, each with its entries.

## Consequences

- A number seen in a log, a ticket or a screenshot identifies the failure regardless of the
  language the message was printed in.
- The exit codes and `DX_*` identities stay exactly as published; this adds a finer identity,
  it does not replace a coarser one.
- The dictionary grows with the per-crate errors of ADR-0040 P3; until a call site has its
  own entry it honestly reports `DX-C000`.
- Every new failure with a name costs one table row, reviewed like any other contract change.

## Rejected

- **Numbers by domain only** (`2xxx` = volume): the class — the thing a caller acts on —
  would not be readable from the number.
- **One number per class only**: says «not found», never «which»; that is the exit code the
  engine already has.
- **Encoding the code in the exit status**: 255 values, and the shell reserves 126+.
