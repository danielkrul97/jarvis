/**
 * Doctrine: favorites (srdíčko / oblíbené) — UX layer contract.
 *
 * Type-only contract for the two surfaces that make up the doctrine's UX layer:
 *   1. FavoriteButton — the heart toggle (card / detail / list / compare)
 *   2. FavoritesList  — the "výpis" of favorited listings
 *
 * The UX layer is decoupled from marketing: toggling emits a FavoriteDomainEvent
 * (see signal.ts). The button never talks to Meta/GA4 directly.
 *
 * Erasable-types syntax only (no enum / namespace / parameter properties) so it
 * runs under `node file.ts` type-stripping for a parse-level smoke check.
 */

/** Where the interaction happened. Kept in sync with fav_event.surface. */
export type FavoriteSurface = 'card' | 'detail' | 'list' | 'compare';

/** Actor identity resolved at interaction time. Guests use a stable anon id. */
export type SubjectKind = 'user' | 'anon';

export interface Subject {
  kind: SubjectKind;
  /** user id when logged in, else the first-party anon id (cookie/localStorage). */
  id: string;
}

/** Visual/interaction states the button must implement. */
export type FavoriteButtonState =
  | 'empty' // not favorited
  | 'filled' // favorited
  | 'pending' // optimistic toggle in flight
  | 'error'; // server rejected → rolled back, show retry affordance

/** The domain event emitted on every toggle. Consumed by the signal layer + persisted as fav_event. */
export interface FavoriteDomainEvent {
  eventId: string; // uuid; SHARED with the client Pixel for Meta dedup
  action: 'add' | 'remove';
  subject: Subject;
  listingId: string;
  surface: FavoriteSurface;
  occurredAt: number; // unix ms
  sessionId?: string;
}

export interface FavoriteButtonProps {
  listingId: string;
  /** SSR/hydration seed; the hook reconciles against the source of truth. */
  initialFavorited: boolean;
  surface: FavoriteSurface;
  size?: 'sm' | 'md' | 'lg';
  /** Fired after the optimistic state flips; the host wires this to the signal layer. */
  onToggle?: (event: FavoriteDomainEvent) => void;
  disabled?: boolean;
}

/**
 * Client hook contract. Guest favorites live in localStorage under a stable
 * anon id; on login, mergeGuestInto() replays them for the user (deduped) and
 * emits 'merge'-surface events so the value/affinity layers see them.
 */
export interface UseFavorites {
  isFavorited(listingId: string): boolean;
  toggle(listingId: string, surface: FavoriteSurface): Promise<FavoriteDomainEvent>;
  list(): string[]; // listing ids, newest first
  count(): number;
  /** Called once after authentication to fold guest state into the account. */
  mergeGuestInto(userId: string): Promise<FavoriteDomainEvent[]>;
}

export interface FavoritesListProps {
  subject: Subject;
  sort?: 'recent' | 'price_asc' | 'price_desc' | 'popularity';
  /** Rendered when the list is empty. */
  emptyState?: unknown;
}

/**
 * Accessibility + i18n contract. User-facing strings are Czech (data, not comments).
 * The button is a toggle: aria-pressed reflects `filled`, and the label changes
 * with state. Heart fill animation must respect prefers-reduced-motion.
 */
export interface FavoriteA11y {
  role: 'button';
  ariaPressed: boolean; // === (state === 'filled')
  ariaLabel: string; // localized, state-dependent
  respectsReducedMotion: true;
}

export const FAVORITE_LABELS = {
  add: 'Přidat do oblíbených',
  remove: 'Odebrat z oblíbených',
  listTitle: 'Oblíbené',
  empty: 'Zatím nemáte žádná oblíbená vozidla.',
  retry: 'Zkusit znovu',
} as const;

/** State machine the FavoriteButton implementation must honor. */
export const FAVORITE_TRANSITIONS: ReadonlyArray<{
  from: FavoriteButtonState;
  on: 'click' | 'server_ok' | 'server_err';
  to: FavoriteButtonState;
}> = [
  { from: 'empty', on: 'click', to: 'pending' },
  { from: 'filled', on: 'click', to: 'pending' },
  { from: 'pending', on: 'server_ok', to: 'filled' }, // resolved value depends on action; see note
  { from: 'pending', on: 'server_err', to: 'error' },
  { from: 'error', on: 'click', to: 'pending' },
];
