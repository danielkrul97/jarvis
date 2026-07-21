# Doctrine: Favorites (srdíčko / oblíbené)

Reference implementation of one **doctrine** — the unit at the center of the Auction24
"doctrine engine" thesis: *a favorite is a data signal, not a button.* This package is a
single doctrine published as a machine-consumable registry item a coding agent (Claude
Code) can pull and assemble.

It exists to test the whole thesis end-to-end on **one** block before scaling to many —
the "musíme testovat" step.

## The four layers

A doctrine bundles what today's ecosystems keep in separate systems (shadcn = UI,
MCP = tools, PBC = human-assembled business capability): one unit that is at once a
business function, a data layer, a UX layer, and a marketing sensor.

| Layer | File | What it is |
|---|---|---|
| **Data** | `src/schema.sql` | Event-sourced favorites + the `0..1` value function |
| **UX** | `src/component-contract.ts` | Heart toggle + list; emits a domain event, never talks to Meta directly |
| **Signal** | `src/signal.ts` | Fan-out to Meta AddToWishlist CAPI + GA4 + internal Intel, consent-gated |
| **Registry** | `registry/favorites.json` | The doctrine as a publishable, agent-installable unit |

### 1. Data layer (`src/schema.sql`, PostgreSQL)

Append-only `fav_event` is the source of truth; everything else is a projection so the
doctrine can be recomputed from raw events.

- `fav_event` — raw signal (add/remove, actor, listing, surface, **consent at emit time**, dedup `event_id`)
- `fav_state` — materialized current membership (powers UX + the list), tracks `add_count` churn
- `fav_listing_rollup` — per-listing popularity (ranking, "nejhledanější")
- `fav_subject_affinity` — **the restructured data**: favorites decomposed into weighted attribute preferences → Meta audiences / recommendations
- `fav_conversion` — downstream conversions, feed the value function
- `fav_signal_value(is_known, age_days, converted, add_count) → numeric [0,1]` — the value function

**The `0..1` value function.** Weighs each data point by usefulness (Daniel's "priority 0–1"):

```
0.35·actor  + 0.35·recency + 0.30·conversion − 0.25·churn , clamped to [0,1]
  actor      known user 1.0, anon 0.4          (a contactable favorite is worth more)
  recency    max(0, 1 − age_days/60)           (linear decay; identical in PG and the check)
  conversion 1.0 if the subject later converted on this listing
  churn      min(1, (add_count−1)·0.2)          (repeated toggling = noise)
```

Verified ranking: known/recent/converted = **0.994** ≫ anon/old/churned = **0.027**.

### 2. UX layer (`src/component-contract.ts`)

Type-only contract for the two surfaces (button + list = one doctrine, per Daniel's
"srdíčko i ten výpis"). Key rules:

- Optimistic toggle with rollback (`empty → pending → filled | error`)
- Guest favorites in localStorage under a stable anon id; `mergeGuestInto(userId)` folds them into the account on login (surface `merge`)
- Toggling emits a `FavoriteDomainEvent` — the UX layer is **decoupled** from marketing
- a11y: `aria-pressed`, state-dependent Czech labels, `prefers-reduced-motion`
- `event.eventId` (uuid) is **shared with the client Pixel** for Meta dedup

### 3. Signal layer (`src/signal.ts`) — the moat

One favorite fans out to three sinks:

| Sink | Event | Gated on consent? |
|---|---|---|
| Meta CAPI | `AddToWishlist` (standard) | **yes** |
| GA4 MP | `add_to_wishlist` (recommended) | **yes** |
| Internal Intel | `favorite` (0–1 value + affinity) | **no** (first-party functional) |

- **Consent gate**: `buildMetaAddToWishlist` / `buildGa4` return `null` without marketing consent (GDPR/ePrivacy). Intel is ungated — the favorites feature itself is first-party functional data.
- **Two different "values", never conflated**: `signalValue ∈ [0,1]` (internal usefulness, Intel only) vs `custom_data.value` (Meta monetary worth, in `currency`).
- Meta contract honored: `content_type: "product"`, `contents: [{id, quantity}]`, `user_data` hashed per Meta rules (`em/ph/external_id` SHA-256 after trim+lowercase; `fbp/fbc` plaintext), ≥1 identifier or no send.

### 4. Registry item (`registry/favorites.json`) — how an agent consumes it

A valid shadcn `registry:block`. The doctrine-specific descriptor lives in `meta.doctrine`
(shadcn's `meta` is an arbitrary object — schema-valid, agent-readable). It declares the
business capability, the `0..1` priority, the emitted signals + their consent gates, and
**`requiresData`** — the machine-readable "jaká data mi chybí" contract the agent resolves
before assembling (e.g. `listing.id` required; `listing.price` needed for the Meta value;
`subject.identity` for `user_data`).

This is the new *type* of registry entry the research flagged as missing: not a UI
component (shadcn) nor a tool (MCP), but a business function carrying data + UX + the
signal it emits.

## Verification (what is proven vs. not)

Run: `python3 verify/registry_validate.py && python3 verify/value_function_check.py && node verify/signal_check.ts && node src/component-contract.ts`

| Gate | Proves | Status |
|---|---|---|
| `registry_validate.py` | registry item valid vs shadcn schema + doctrine descriptor invariants + files exist | **green** |
| `value_function_check.py` | value ∈ [0,1], clamps both ends, ranks correctly (sqlite, same formula as PG) | **green** |
| `signal_check.ts` | Meta AddToWishlist contract + consent gate + hashing + intel (real signal.ts under node) | **green** |
| `component-contract.ts` | UX contract parses / strips cleanly | **green** |

## Known limits — calibration pending (read before trusting)

1. **shadcn schema is a faithful reconstruction**, not the byte-exact upstream file (raw download was sandbox-blocked). Validate against the live schema with `npx shadcn build` when online — a stricter `additionalProperties` upstream is the one thing this gate wouldn't catch.
2. **Postgres DDL not executed** (no PG in env). The value *logic* is verified in sqlite with the identical formula; the plpgsql trigger, view, `jsonb`, and `pgcrypto` are review-only. Run against real Postgres before shipping.
3. **TypeScript not type-checked by `tsc`** — only runtime-executed via node type-stripping. Add `tsc --noEmit` to CI to catch type-only errors.
4. **The value function is a first-cut heuristic.** Weights (0.35/0.35/0.30/0.25) and the 60-day decay are chosen, not learned. The `converted` term is the hook to calibrate against real conversion data — do that before treating `signalValue` as ground truth.
5. **Meta `value = price · 0.02`** is an invented estimate of wishlist worth (isolated in `WISHLIST_VALUE_FRACTION`). Replace with a modeled lead value.
6. **Partial**: affinity decomposition wires only `make`/`body` (schema supports more); consent is binary (no TCF purpose granularity); `mergeGuestInto` is specified but its dedup/replay algorithm is not implemented here.

## Next

If this one doctrine assembles into Auction24 end-to-end (data → UX → signal, consent
respected, `event_id` deduped against the Pixel), the thesis holds and the pattern
generalizes. If it doesn't, the friction shows up here — on one block — instead of across
367.
