/**
 * Kill-gate 3: the emitted-signal layer honors the real Meta CAPI contract and
 * the consent gate. Runs the actual signal.ts under `node` type-stripping.
 */
import { createHash } from 'node:crypto';
import {
  buildMetaAddToWishlist,
  buildGa4AddToWishlist,
  buildIntelSignal,
  buildUserData,
  normalizeAndHash,
  type FavoriteDomainEvent,
} from '../src/signal.ts';

const fails: string[] = [];
function check(cond: boolean, msg: string) {
  if (!cond) fails.push(msg);
}

const baseEvent: FavoriteDomainEvent = {
  eventId: 'evt-123',
  action: 'add',
  subject: { kind: 'user', id: 'u-1' },
  listingId: 'veh-777',
  surface: 'detail',
  occurredAt: 1_700_000_000_000,
};
const identity = { email: 'Test@Example.com ', externalId: 'u-1', fbp: 'fb.1.2.3' };
const listing = { price: 500_000, currency: 'CZK', make: 'Mercedes-Benz', body: 'SUV' };

// 1. No consent → both marketing sinks are null.
check(buildMetaAddToWishlist(baseEvent, identity, { marketing: false }, listing) === null, 'no-consent Meta must be null');
check(buildGa4AddToWishlist(baseEvent, { marketing: false }, listing) === null, 'no-consent GA4 must be null');

// 2. Consent + identity → valid AddToWishlist matching the Meta contract.
const meta = buildMetaAddToWishlist(baseEvent, identity, { marketing: true }, listing);
check(meta !== null, 'consented Meta must be built');
if (meta) {
  check(meta.event_name === 'AddToWishlist', 'event_name must be AddToWishlist');
  check(meta.action_source === 'website', 'action_source must be website');
  check(meta.event_id === 'evt-123', 'event_id must reuse eventId for dedup');
  check(Number.isInteger(meta.event_time) && meta.event_time === 1_700_000_000, 'event_time must be unix seconds');
  check(Object.keys(meta.user_data).length >= 1, 'user_data needs >=1 identifier');
  check(/^[0-9a-f]{64}$/.test(meta.user_data.em ?? ''), 'em must be sha256 hex');
  // Independently recompute the expected hash: normalization = trim + lowercase, then sha256.
  const expectedEm = createHash('sha256').update('test@example.com').digest('hex');
  check(meta.user_data.em === expectedEm, 'em must be normalized (trim+lowercase) then sha256');
  check(normalizeAndHash('  Test@Example.com ') === expectedEm, 'normalizeAndHash trims + lowercases');
  check(meta.user_data.fbp === 'fb.1.2.3', 'fbp must be plaintext');
  check(meta.custom_data.content_type === 'product', 'content_type must be product');
  check(JSON.stringify(meta.custom_data.contents) === JSON.stringify([{ id: 'veh-777', quantity: 1 }]), 'contents shape');
  check(meta.custom_data.content_ids[0] === 'veh-777', 'content_ids');
  check(meta.custom_data.currency === 'CZK', 'currency');
  check(meta.custom_data.value === 10000, 'value must be price*0.02 = 10000'); // 500000*0.02
}

// 3. remove action → null (AddToWishlist has no removal counterpart).
check(buildMetaAddToWishlist({ ...baseEvent, action: 'remove' }, identity, { marketing: true }, listing) === null, 'remove must be null');

// 4. Consent but NO usable identity → null (Meta requires >=1 identifier).
check(buildMetaAddToWishlist(baseEvent, {}, { marketing: true }, listing) === null, 'no-identity must be null');
check(buildUserData({}) === null, 'empty identity → null user_data');

// 5. Intel signal is ungated and clamps the 0..1 value; affinity deltas present.
const intel = buildIntelSignal(baseEvent, 1.7, listing);
check(intel.signalValue === 1, 'intel signalValue must clamp to 1');
check(intel.affinityDeltas.some((d) => d.attributeKey === 'make' && d.attributeValue === 'Mercedes-Benz'), 'affinity make delta');

if (fails.length) {
  console.log('KILL-GATE 3 FAILED:');
  for (const f of fails) console.log('  -', f);
  process.exit(1);
}
console.log('KILL-GATE 3 PASS: Meta AddToWishlist contract + consent gate + intel signal all hold');
