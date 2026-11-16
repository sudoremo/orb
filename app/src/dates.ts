/**
 * Civil dates: `YYYY-MM-DD` strings handled as calendar arithmetic, never
 * as instants.
 *
 * A civil date is not a point in time. Turning one into a `Date` and back
 * crosses the local offset twice, and across a DST change a day's worth of
 * arithmetic lands at 23:00 the day before. Nothing here converts between a
 * civil date and an instant; what has to — a reminder — does it alone.
 */

export const pad = (n: number): string => String(n).padStart(2, "0");

/** `YYYY-MM-DD` from a year/month/day triple, month being 1-based. */
export function civil(year: number, month: number, day: number): string {
  return `${String(year).padStart(4, "0")}-${pad(month)}-${pad(day)}`;
}

/** The parts of a `YYYY-MM-DD`, month 1-based. */
export function parts(date: string): { year: number; month: number; day: number } {
  const [year, month, day] = date.split("-").map(Number);
  return { year, month, day };
}

/**
 * Today as a civil date in the machine's timezone.
 *
 * Built from the local calendar fields rather than from `toISOString()`, which
 * is UTC: east of Greenwich that spells tonight as tomorrow, which is the exact
 * off-by-one §4.3 exists to prevent.
 */
export function localToday(now: Date = new Date()): string {
  return civil(now.getFullYear(), now.getMonth() + 1, now.getDate());
}

/** A civil date as a UTC timestamp, for arithmetic only. */
export function stamp(date: string): number {
  const { year, month, day } = parts(date);
  return Date.UTC(year, month - 1, day);
}

/** The civil date a UTC timestamp stands for. */
export function unstamp(ms: number): string {
  const d = new Date(ms);
  return civil(d.getUTCFullYear(), d.getUTCMonth() + 1, d.getUTCDate());
}

export function addDays(date: string, days: number): string {
  return unstamp(stamp(date) + days * 86_400_000);
}

/**
 * Days from `from` to `to`. Negative means in the past; an unreadable date
 * counts as today.
 *
 * Both ends are UTC midnight, so the difference is a whole number of days
 * whatever the local offset did in between — which is what makes this survive
 * a DST boundary.
 */
export function daysBetween(from: string, to: string): number {
  const a = stamp(from);
  const b = stamp(to);
  if (Number.isNaN(a) || Number.isNaN(b)) return 0;
  return Math.round((b - a) / 86_400_000);
}

/**
 * Add months, clamping the day.
 *
 * January 31 plus a month is February 28, not March 3. The browser's own
 * `setMonth` overflows instead, which in a calendar means paging forward from
 * the end of a long month silently skips the short one entirely.
 */
export function addMonths(date: string, months: number): string {
  const { year, month, day } = parts(date);
  const zeroBased = year * 12 + (month - 1) + months;
  const targetYear = Math.floor(zeroBased / 12);
  const targetMonth = zeroBased - targetYear * 12 + 1;
  return civil(targetYear, targetMonth, Math.min(day, daysInMonth(targetYear, targetMonth)));
}

export function daysInMonth(year: number, month: number): number {
  return new Date(Date.UTC(year, month, 0)).getUTCDate();
}

/** 0 Sunday … 6 Saturday, for a civil date. */
export function weekday(date: string): number {
  return new Date(stamp(date)).getUTCDay();
}

/** Whether a string is a civil date worth working with. */
export function isCivilDate(value: string | null | undefined): value is string {
  return !!value && /^\d{4}-\d{2}-\d{2}$/.test(value) && !Number.isNaN(stamp(value));
}
