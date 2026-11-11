// Test-only: let a mock speak the `fetch(url, init)` shape.
//
// `openapi-fetch` hands the underlying fetch a `Request`, while a test that says
// "this route answers that body" wants a path and an init it can read. Wrapping
// the mock keeps every call site in the tests about the route, not about which
// client made the call — and the client's real request still goes through the
// same fetch, so what is tested is still what ships.

/** What a test's mock has always looked like. */
export type PlainFetch = (input: string, init?: RequestInit) => Promise<Response>;

/** Adapt `target` so a `Request` arrives as its path, method, headers and body. */
export function plainFetch(target: PlainFetch): typeof fetch {
  return async (input: RequestInfo | URL, init?: RequestInit): Promise<Response> => {
    if (!(input instanceof Request)) {
      return target(String(input), init);
    }

    let route: string;
    try {
      const url = new URL(input.url);
      route = url.pathname + url.search;
    } catch {
      // A relative URL is legal here and not to `URL`; it is already the path.
      route = input.url;
    }

    const headers: Record<string, string> = {};
    input.headers.forEach((value, name) => {
      headers[name] = value;
    });

    const plain: RequestInit = {
      method: input.method,
      headers,
      signal: input.signal,
    };
    if (input.method !== 'GET' && input.method !== 'HEAD') {
      plain.body = await input.clone().text();
    }
    return target(route, plain);
  };
}
