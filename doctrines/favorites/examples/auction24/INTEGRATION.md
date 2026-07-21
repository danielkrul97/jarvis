# Zapojení doktríny „favorites" do garaaage-auction (Nuxt 4 / Vue 3 / Kysely)

## Nejdůležitější zjištění: srdíčko už existuje — kompletně

garaaage-auction **není zelená louka**. Oblíbené jsou hotové od DB po Meta Pixel:
heart na kartě, toggle přes composable, POST/GET routy, atomický repo, stránka,
i18n, `AddToWishlist` přes klientský Pixel, i cron na „končící oblíbené". Doktrína se
tedy nemá stavět znovu — má stávající vrstvu **obohatit**, ne duplikovat.

## Co už tam je vs. co doktrína přidává

| Vrstva doktríny | Už v garaaage? | Skutečná cesta | Co doktrína přidává |
|---|---|---|---|
| UX heart + výpis | ✅ | `features/supply/auction-items/ui/ItemCard.vue` (`.fav-btn`), `pages/favorites.vue` | — (reuse) |
| Aktuální stav | ✅ `users.favorite_ids text[]` | `server/db/schema.ts`, `server/repos/userRepo.ts:177` `toggleFavorite` | — (zůstává jako rychlý read-model) |
| Toggle API | ✅ | `server/api/favorites/toggle.post.ts` | **+ zápis eventu, + serverový signál** |
| Meta signál | ✅ klientský Pixel | `features/demand/favorites/logic/useFavorites.ts:31` (`content_type:'vehicle'`, bez eventID) | **+ serverová CAPI s eventID dedup** |
| GA4 | ✅ Consent Mode v2 (`nuxt-gtag`) | `nuxt.config.ts` | + explicitní `add_to_wishlist` (volitelně) |
| Souhlas | ✅ granulární + server záznamy | `features/platform/consent-tracking/logic/useCookieConsent.ts`, `server/repos/consentRepo.ts:13` `getLatestMarketingConsent` | — (reuse) |
| **Event substrát / hodnota 0–1 / afinita** | ❌ **chybí** | — | **`fav_events` + `fav_signal_value` → reco** ← *tohle je jádro přínosu* |
| Guest oblíbené | ❌ jen po přihlášení | `useFavorites` otevře auth dialog | guest-merge z doktríny je tu N/A (ledaže bys je chtěl) |

**Pointa:** `users.favorite_ids` je jen množina „co mám teď rád" — zahazuje signál
(kdy, odkud, jak často, jakou to má hodnotu). Doktrína přidá **append-only substrát**,
nad kterým běží hodnota 0–1 a afinita, aniž by sáhla na fungující read-model.

---

## Přidání 1 — event substrát + hodnotová funkce (migrace 063)

Nová migrace ve stylu garaaage (raw `sql`, `up`/`down`). Vytvoří `fav_events` (append-only)
a `fav_signal_value()` z `doctrines/favorites/src/schema.sql`.

```ts
// server/migrations/063-create-fav-events.ts   (příští volné číslo — nejvyšší je 062)
import { sql, type Kysely } from 'kysely'

export const up = async (db: Kysely<unknown>): Promise<void> => {
  await sql`
    CREATE TABLE fav_events (
      id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
      event_id          uuid        NOT NULL,                 -- sdílené s Pixelem pro Meta dedup
      user_id           text        NOT NULL REFERENCES users(id),
      item_id           text        NOT NULL,                 -- bez FK: oblíbené smazané položky přežívá (viz userRepo)
      action            text        NOT NULL CHECK (action IN ('add','remove')),
      surface           text        NOT NULL,                 -- 'card' | 'detail' | 'list' | 'compare'
      marketing_consent boolean     NOT NULL DEFAULT false,   -- stav souhlasu v okamžiku eventu
      occurred_at       timestamptz NOT NULL DEFAULT now()
    )`.execute(db)
  await sql`CREATE INDEX fav_events_user_idx ON fav_events (user_id, occurred_at DESC)`.execute(db)
  await sql`CREATE INDEX fav_events_item_idx ON fav_events (item_id, occurred_at DESC)`.execute(db)
  await sql`CREATE UNIQUE INDEX fav_events_event_id_uidx ON fav_events (event_id)`.execute(db)

  // Hodnota 0..1 (linear decay, shodná s fav_signal_value v schema.sql doktríny).
  await sql`
    CREATE FUNCTION fav_signal_value(is_known boolean, age_days numeric, converted boolean, add_count integer)
    RETURNS numeric LANGUAGE sql IMMUTABLE AS $$
      SELECT LEAST(1.0, GREATEST(0.0,
            0.35 * (CASE WHEN is_known THEN 1.0 ELSE 0.4 END)
          + 0.35 * GREATEST(0.0, 1.0 - age_days / 60.0)
          + 0.30 * (CASE WHEN converted THEN 1.0 ELSE 0.0 END)
          - 0.25 * LEAST(1.0, (add_count - 1) * 0.2)))
    $$`.execute(db)
}

export const down = async (db: Kysely<unknown>): Promise<void> => {
  await sql`DROP FUNCTION IF EXISTS fav_signal_value(boolean, numeric, boolean, integer)`.execute(db)
  await sql`DROP TABLE IF EXISTS fav_events`.execute(db)
}
```

A registrace typu do `server/db/schema.ts` (CamelCasePlugin → camelCase v TS, snake_case v SQL):

```ts
// server/db/schema.ts  — přidat interface a zaregistrovat na Database
export interface FavEventsTable {
  id: Generated<number>
  eventId: string
  userId: string
  itemId: string
  action: string
  surface: string
  marketingConsent: Generated<boolean>
  occurredAt: Generated<Date>
}
// export interface Database { … ; favEvents: FavEventsTable }
export type FavEventRow = Selectable<FavEventsTable>
```

## Přidání 2 — repo, který event zapíše

```ts
// server/repos/favEventRepo.ts
import { db } from '../utils/db'

export const appendFavEvent = async (e: {
  eventId: string; userId: string; itemId: string
  action: 'add' | 'remove'; surface: string; marketingConsent: boolean
}): Promise<void> => {
  await db.insertInto('favEvents').values(e).execute()
}
```

## Přidání 3 — toggle routa: zapsat event + poslat serverový signál

Klíčové: `requireSession(event)` už vrací usera **včetně `favoriteIds`**, takže before-stav
mám zadarmo a poznám add vs. remove bez zásahu do atomického `toggleFavorite`.

```ts
// server/api/favorites/toggle.post.ts  — přírůstek (existující řádky ponechány)
import { toggleFavorite } from '~/server/repos/userRepo'
import { appendFavEvent } from '~/server/repos/favEventRepo'
import { emitFavoriteCapi } from '~/server/utils/favoriteCapi'

export default defineEventHandler(async event => {
  const user = await requireSession(event)
  enforceRateLimit(event, { bucket: 'favorites', limit: 60, windowMs: 60_000, key: user.id })
  const body = await readBody(event).catch(() => null)
  const id = typeof body?.id === 'string' ? body.id.trim() : ''
  if (!id || id.length > 64) throw createError({ statusCode: 400, statusMessage: 'Invalid item id' })

  const wasFav = user.favoriteIds?.includes(id) ?? false          // ← before-stav z requireSession
  const favoriteIds = await toggleFavorite(user.id, id)           // beze změny
  const nowFav = favoriteIds.includes(id)

  if (nowFav !== wasFav) {                                        // skutečná změna (ne blokovaný no-op)
    const action = nowFav ? 'add' : 'remove'
    const eventId = typeof body?.eventId === 'string' ? body.eventId : crypto.randomUUID()
    const marketingConsent = (await getLatestMarketingConsent(user.id)) ?? false
    await appendFavEvent({ eventId, userId: user.id, itemId: id, action,
                           surface: body?.surface ?? 'card', marketingConsent })
    if (action === 'add' && marketingConsent) await emitFavoriteCapi(event, user, id, eventId)
  }
  return { favoriteIds }
})
```

## Přidání 4 — serverová Meta CAPI (znovupoužije `signal.ts` doktríny)

Doplněk ke klientskému Pixelu: odolné proti blokovačům, deduplikováno přes sdílený
`event_id`. Používá `buildMetaAddToWishlist` z `doctrines/favorites/src/signal.ts`.

```ts
// server/utils/favoriteCapi.ts
import { buildMetaAddToWishlist } from '../../doctrines/favorites/src/signal'  // nebo zkopírovat do repa

export const emitFavoriteCapi = async (event, user, itemId: string, eventId: string) => {
  const item = await getItem(itemId)                      // cena/značka pro custom_data + afinitu
  const vid = getCookie(event, 'vid')                     // guest id → external_id pro lepší match
  const payload = buildMetaAddToWishlist(
    { eventId, action: 'add', subject: { kind: 'user', id: user.id },
      listingId: itemId, surface: 'card', occurredAt: Date.now() },
    { email: user.email, externalId: user.id ?? vid, fbc: getCookie(event, '_fbc'), fbp: getCookie(event, '_fbp') },
    { marketing: true },                                  // routa už souhlas ověřila
    { price: item?.priceFrom?.amount, currency: 'CZK', make: item?.specs?.manufacturer, body: item?.bodyType },
  )
  if (payload) await sendToMetaCapi(payload)              // POST na graph.facebook.com/<PIXEL>/events
}
```

## Přidání 5 — klient: sdílet eventID pro dedup (jednořádkovka)

Aby serverová CAPI a klientský Pixel byly jeden a týž event, pošli stejné `eventId` do obou:

```ts
// features/demand/favorites/logic/useFavorites.ts  (kolem řádku 31)
const eventId = crypto.randomUUID()
await $fetch('/api/favorites/toggle', { method: 'POST', body: { id, eventId, surface } })
if (res.favoriteIds.includes(id))
  pixelTrack('AddToWishlist', { content_ids: [id], content_type: 'vehicle', eventID: eventId })
//                                                                          ^ dedup s CAPI
```

---

## Poctivé poznámky (fable-mode: neplácat na oko)

- **Nespuštěno proti garaaage** — cesty a signatury odpovídají reálným souborům (citováno výše),
  ale tento diff jsem v garaaage nebuildil. Ověřit: `pnpm db:migrate` + `pnpm test` + `vue-tsc`.
- **`content_type` nesedí**: garaaage posílá `'vehicle'`, Meta standard pro katalogový matching
  (DPA/Advantage+) je `'product'` s katalogovými ID. Pro plný wishlist-retargeting sjednotit na `'product'`.
- **Read-model se nemění**: `users.favorite_ids` + GIN index + `toggleFavorite` zůstávají beze změny
  jako rychlá cesta pro `isFav`, stránku a počet „sledujících". `fav_events` je paralelní substrát.
- **Guest oblíbené**: garaaage je má jen po přihlášení (heart → auth dialog). Guest-merge z doktríny
  je tu tedy N/A; kdybys chtěl guest wishlisty, je to samostatné rozhodnutí (klíč `vid` už existuje).
- **Intel = existující reco**: hodnotu 0–1 a afinitu (`item.specs.manufacturer`, `item.bodyType`)
  konzumuje stávající doporučovací systém (`RecommendationEventsTable`, `itemSignalMeta`), ne nová tabulka.

## Minimální PR (co se dotkne)

```
NOVÉ:    server/migrations/063-create-fav-events.ts
         server/repos/favEventRepo.ts
         server/utils/favoriteCapi.ts
UPRAVIT: server/db/schema.ts                              (+ FavEventsTable, + Database.favEvents)
         server/api/favorites/toggle.post.ts              (+ ~8 řádků: before/after, event, CAPI)
         features/demand/favorites/logic/useFavorites.ts  (+ sdílené eventID)
```
