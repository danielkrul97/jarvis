-- Doctrine: favorites (srdíčko / oblíbené)
-- Data layer — PostgreSQL canonical DDL.
--
-- Design premise (Daniel's thesis): a favorite is NOT a UI toggle, it is an
-- append-only data signal with a computable value. Everything below is derived
-- from the event log so the whole doctrine can be recomputed from raw events.
--
-- Layout:
--   fav_event            append-only source of truth (the signal)
--   fav_state            materialized current membership (powers UX + the list)
--   fav_listing_rollup   per-listing popularity (ranking, "nejhledanější")
--   fav_subject_affinity per-subject preference vector (restructured data → audiences)
--   fav_conversion       downstream conversions, used by the value function
--   fav_signal_value()   the 0..1 value function
--   fav_event_valued     view: every event with its 0..1 value

CREATE EXTENSION IF NOT EXISTS pgcrypto;  -- gen_random_uuid()

-- ── 1. Raw signal (append-only) ────────────────────────────────────────────
CREATE TABLE fav_event (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id       uuid        NOT NULL DEFAULT gen_random_uuid(),  -- dedup key shared with the client Pixel
    occurred_at    timestamptz NOT NULL DEFAULT now(),
    action         text        NOT NULL CHECK (action IN ('add', 'remove')),

    -- Actor: known user OR anonymous guest. Exactly one identity is expected;
    -- guests carry a stable anon_id from a first-party cookie / localStorage.
    subject_kind   text        NOT NULL CHECK (subject_kind IN ('user', 'anon')),
    subject_id     text        NOT NULL,

    listing_id     text        NOT NULL,                 -- vehicle / advert being favorited
    surface        text        NOT NULL                  -- WHERE it happened; the button AND the list are one doctrine
                               CHECK (surface IN ('card', 'detail', 'list', 'compare', 'merge')),
    session_id     text,
    device         text,

    -- Consent captured at emit time — gates the outbound marketing signal (GDPR/ePrivacy).
    marketing_consent boolean  NOT NULL DEFAULT false,

    raw            jsonb       NOT NULL DEFAULT '{}'::jsonb  -- forward-compatible extension slot
);

CREATE INDEX fav_event_subject_idx ON fav_event (subject_kind, subject_id, occurred_at DESC);
CREATE INDEX fav_event_listing_idx ON fav_event (listing_id, occurred_at DESC);
CREATE UNIQUE INDEX fav_event_event_id_uidx ON fav_event (event_id);

-- ── 2. Materialized current state (projection of the event log) ─────────────
CREATE TABLE fav_state (
    subject_kind   text        NOT NULL,
    subject_id     text        NOT NULL,
    listing_id     text        NOT NULL,
    favorited      boolean     NOT NULL,
    first_added_at timestamptz NOT NULL,
    last_changed_at timestamptz NOT NULL,
    add_count      integer     NOT NULL DEFAULT 1,       -- toggle churn signal
    PRIMARY KEY (subject_kind, subject_id, listing_id)
);

CREATE INDEX fav_state_active_idx ON fav_state (subject_kind, subject_id) WHERE favorited;

-- ── 3. Per-listing popularity rollup (derived) ─────────────────────────────
CREATE TABLE fav_listing_rollup (
    listing_id     text        PRIMARY KEY,
    active_favs    integer     NOT NULL DEFAULT 0,
    favs_24h       integer     NOT NULL DEFAULT 0,
    favs_7d        integer     NOT NULL DEFAULT 0,
    uniq_subjects  integer     NOT NULL DEFAULT 0,
    updated_at     timestamptz NOT NULL DEFAULT now()
);

-- ── 4. Per-subject affinity vector (the "restructured data") ────────────────
-- Favorites are decomposed into weighted attribute preferences (make, price band,
-- body type, year…). This is what feeds recommendations and Meta custom audiences.
CREATE TABLE fav_subject_affinity (
    subject_kind    text       NOT NULL,
    subject_id      text       NOT NULL,
    attribute_key   text       NOT NULL,                 -- e.g. 'make', 'body', 'price_band'
    attribute_value text       NOT NULL,                 -- e.g. 'Mercedes-Benz', 'SUV', '500k-750k'
    weight          numeric(6,4) NOT NULL DEFAULT 0,      -- accumulated 0..1 signal value
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subject_kind, subject_id, attribute_key, attribute_value)
);

-- ── 5. Downstream conversions (feeds the value function) ────────────────────
CREATE TABLE fav_conversion (
    subject_kind   text        NOT NULL,
    subject_id     text        NOT NULL,
    listing_id     text        NOT NULL,
    kind           text        NOT NULL CHECK (kind IN ('lead', 'contact', 'purchase')),
    converted_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (subject_kind, subject_id, listing_id, kind)
);

-- ── 6. The 0..1 value function ─────────────────────────────────────────────
-- signal_value ∈ [0,1] weighs the data point by usefulness. Deliberately uses a
-- linear recency decay (no exp) so the exact same formula runs in Postgres and in
-- the sqlite verification harness — canonical == checked.
--
--   actor    known user 1.0, anon 0.4  (a contactable favorite is worth more)
--   recency  max(0, 1 - age_days/60)   (linear decay over ~2 months)
--   conv     1.0 if the subject later converted on this listing
--   churn    penalty min(1, (add_count-1)*0.2)  (repeated toggling = noise)
CREATE OR REPLACE FUNCTION fav_signal_value(
    is_known   boolean,
    age_days   numeric,
    converted  boolean,
    add_count  integer
) RETURNS numeric
LANGUAGE sql IMMUTABLE AS $$
    SELECT LEAST(1.0, GREATEST(0.0,
          0.35 * (CASE WHEN is_known THEN 1.0 ELSE 0.4 END)
        + 0.35 * GREATEST(0.0, 1.0 - age_days / 60.0)
        + 0.30 * (CASE WHEN converted THEN 1.0 ELSE 0.0 END)
        - 0.25 * LEAST(1.0, (add_count - 1) * 0.2)
    ));
$$;

-- ── 7. Valued event view ────────────────────────────────────────────────────
CREATE OR REPLACE VIEW fav_event_valued AS
SELECT
    e.id,
    e.event_id,
    e.subject_kind,
    e.subject_id,
    e.listing_id,
    e.occurred_at,
    fav_signal_value(
        e.subject_kind = 'user',
        EXTRACT(EPOCH FROM (now() - e.occurred_at)) / 86400.0,
        c.subject_id IS NOT NULL,
        COALESCE(s.add_count, 1)
    ) AS signal_value
FROM fav_event e
LEFT JOIN fav_state s
       ON s.subject_kind = e.subject_kind AND s.subject_id = e.subject_id AND s.listing_id = e.listing_id
LEFT JOIN fav_conversion c
       ON c.subject_kind = e.subject_kind AND c.subject_id = e.subject_id AND c.listing_id = e.listing_id
WHERE e.action = 'add';

-- ── 8. Projection maintenance: fav_event → fav_state ────────────────────────
CREATE OR REPLACE FUNCTION fav_apply_event() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO fav_state AS st (subject_kind, subject_id, listing_id, favorited,
                                 first_added_at, last_changed_at, add_count)
    VALUES (NEW.subject_kind, NEW.subject_id, NEW.listing_id, NEW.action = 'add',
            NEW.occurred_at, NEW.occurred_at, CASE WHEN NEW.action = 'add' THEN 1 ELSE 0 END)
    ON CONFLICT (subject_kind, subject_id, listing_id) DO UPDATE
        SET favorited       = NEW.action = 'add',
            last_changed_at = NEW.occurred_at,
            add_count       = st.add_count + CASE WHEN NEW.action = 'add' THEN 1 ELSE 0 END;
    RETURN NEW;
END;
$$;

CREATE TRIGGER fav_event_apply
    AFTER INSERT ON fav_event
    FOR EACH ROW EXECUTE FUNCTION fav_apply_event();
