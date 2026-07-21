/**
 * Doctrine: favorites (srdíčko / oblíbené) — emitted-signal layer.
 *
 * This is the doctrine's reason to exist: a favorite is a marketing/data signal,
 * not a button. One FavoriteDomainEvent fans out to three sinks:
 *
 *   1. Meta Conversions API  — standard event "AddToWishlist" (server-side, deduped)
 *   2. GA4 Measurement Proto — recommended event "add_to_wishlist"
 *   3. Internal Intel bus    — the raw 0..1 signal value + affinity deltas
 *
 * CONSENT: sinks 1 and 2 are marketing and MUST be gated on marketing consent
 * (GDPR / ePrivacy). Sink 3 is first-party functional data (the favorites feature
 * itself) and is NOT gated. buildMeta/buildGa4 return null without consent.
 *
 * VALUE — two different "values", never conflate them:
 *   - signalValue ∈ [0,1]   internal usefulness score (fav_signal_value); Intel only.
 *   - custom_data.value      Meta monetary worth, in `currency`; estimated lead value.
 *
 * Erasable-types syntax (runs under `node file.ts`). Server-side hashing via node:crypto.
 */

import { createHash } from 'node:crypto';

// Re-declared here so the signal layer has no import cycle with the UX contract.
export interface FavoriteDomainEvent {
  eventId: string;
  action: 'add' | 'remove';
  subject: { kind: 'user' | 'anon'; id: string };
  listingId: string;
  surface: 'card' | 'detail' | 'list' | 'compare' | 'merge';
  occurredAt: number; // unix ms
  sessionId?: string;
}

/** Identity signals available for match quality. Provide as many as consent allows. */
export interface SubjectIdentity {
  email?: string;
  phone?: string;
  externalId?: string; // your user/anon id
  fbp?: string; // _fbp cookie (plaintext)
  fbc?: string; // _fbc cookie (plaintext)
  clientIpAddress?: string;
  clientUserAgent?: string;
}

export interface ConsentState {
  marketing: boolean; // Meta + GA4 gate
}

/** Listing facts needed to value/attribute the signal. */
export interface ListingContext {
  price?: number; // listing price in `currency`
  currency?: string; // default 'CZK'
  make?: string;
  body?: string;
}

export interface MetaCapiEvent {
  event_name: 'AddToWishlist';
  event_time: number; // unix seconds
  event_id: string; // dedup with Pixel
  action_source: 'website';
  user_data: Record<string, string>;
  custom_data: {
    content_ids: string[];
    content_type: 'product';
    contents: Array<{ id: string; quantity: number }>;
    value: number;
    currency: string;
  };
}

export interface Ga4Event {
  name: 'add_to_wishlist';
  params: { currency: string; value: number; items: Array<{ item_id: string }> };
}

export interface IntelSignal {
  kind: 'favorite';
  eventId: string;
  subjectKind: 'user' | 'anon';
  subjectId: string;
  listingId: string;
  signalValue: number; // 0..1
  affinityDeltas: Array<{ attributeKey: string; attributeValue: string; weight: number }>;
  occurredAt: number;
}

const CURRENCY_DEFAULT = 'CZK';
/** Fraction of listing price used as the estimated marketing worth of a wishlist add. */
const WISHLIST_VALUE_FRACTION = 0.02;

/** Meta normalization: trim + lowercase, then SHA-256 hex. */
export function normalizeAndHash(value: string): string {
  return createHash('sha256').update(value.trim().toLowerCase()).digest('hex');
}

/**
 * Build user_data honoring Meta's hashing rules. Returns null when no identifier
 * is available — Meta requires at least one, so a null here means "do not send".
 */
export function buildUserData(id: SubjectIdentity): Record<string, string> | null {
  const ud: Record<string, string> = {};
  if (id.email) ud.em = normalizeAndHash(id.email);
  if (id.phone) ud.ph = normalizeAndHash(id.phone.replace(/[^\d]/g, ''));
  if (id.externalId) ud.external_id = normalizeAndHash(id.externalId);
  if (id.fbp) ud.fbp = id.fbp; // plaintext
  if (id.fbc) ud.fbc = id.fbc; // plaintext
  if (id.clientIpAddress) ud.client_ip_address = id.clientIpAddress;
  if (id.clientUserAgent) ud.client_user_agent = id.clientUserAgent;
  return Object.keys(ud).length > 0 ? ud : null;
}

/** Sink 1 — Meta CAPI "AddToWishlist". Null when no consent or no usable identity. */
export function buildMetaAddToWishlist(
  event: FavoriteDomainEvent,
  identity: SubjectIdentity,
  consent: ConsentState,
  listing: ListingContext = {},
): MetaCapiEvent | null {
  if (!consent.marketing) return null;
  if (event.action !== 'add') return null; // AddToWishlist has no removal counterpart
  const user_data = buildUserData(identity);
  if (!user_data) return null;

  const currency = listing.currency ?? CURRENCY_DEFAULT;
  const value = listing.price != null ? round2(listing.price * WISHLIST_VALUE_FRACTION) : 0;

  return {
    event_name: 'AddToWishlist',
    event_time: Math.floor(event.occurredAt / 1000),
    event_id: event.eventId,
    action_source: 'website',
    user_data,
    custom_data: {
      content_ids: [event.listingId],
      content_type: 'product',
      contents: [{ id: event.listingId, quantity: 1 }],
      value,
      currency,
    },
  };
}

/** Sink 2 — GA4 recommended "add_to_wishlist". Null without consent. */
export function buildGa4AddToWishlist(
  event: FavoriteDomainEvent,
  consent: ConsentState,
  listing: ListingContext = {},
): Ga4Event | null {
  if (!consent.marketing || event.action !== 'add') return null;
  return {
    name: 'add_to_wishlist',
    params: {
      currency: listing.currency ?? CURRENCY_DEFAULT,
      value: listing.price != null ? round2(listing.price * WISHLIST_VALUE_FRACTION) : 0,
      items: [{ item_id: event.listingId }],
    },
  };
}

/**
 * Sink 3 — internal Intel signal. NOT consent-gated (first-party functional data).
 * Carries the 0..1 signal value and the affinity deltas derived from the listing.
 */
export function buildIntelSignal(
  event: FavoriteDomainEvent,
  signalValue: number,
  listing: ListingContext = {},
): IntelSignal {
  const affinityDeltas: IntelSignal['affinityDeltas'] = [];
  if (listing.make) affinityDeltas.push({ attributeKey: 'make', attributeValue: listing.make, weight: signalValue });
  if (listing.body) affinityDeltas.push({ attributeKey: 'body', attributeValue: listing.body, weight: signalValue });
  return {
    kind: 'favorite',
    eventId: event.eventId,
    subjectKind: event.subject.kind,
    subjectId: event.subject.id,
    listingId: event.listingId,
    signalValue: clamp01(signalValue),
    affinityDeltas,
    occurredAt: event.occurredAt,
  };
}

function clamp01(v: number): number {
  return Math.min(1, Math.max(0, v));
}
function round2(v: number): number {
  return Math.round(v * 100) / 100;
}
