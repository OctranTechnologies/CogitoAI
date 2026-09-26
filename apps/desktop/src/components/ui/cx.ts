/**
 * Class-name joiner.
 *
 * Deliberately tiny and dependency-free: falsy entries are dropped so callers
 * can pass conditionals inline without producing `"false"` in the output.
 */

export function cx(...values: Array<string | false | null | undefined>): string {
  return values.filter(Boolean).join(" ");
}
