// Intentional diagnostic: typescript-language-server (via tsserver) reports the
// type mismatch — a string literal assigned to a number-typed const.
const answer: number = "not a number";

export function value(): number {
  return answer;
}
