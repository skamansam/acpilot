export function 日本語の関数(value: number): number {
  return value + 1;
}

export function caller(): number {
  const banner = "😀 日本語 é"; return 日本語の関数(41 + banner);
}
