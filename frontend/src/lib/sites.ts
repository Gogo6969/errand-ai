/**
 * Pulling the websites out of what somebody wrote.
 *
 * Asking a person to describe a job in plain language and then demanding they
 * separately type a domain is asking them to do the same work twice. If the
 * description already names an address, offer it.
 *
 * Deliberately conservative: it only offers what is genuinely there. It will
 * not guess that "CDV Software" means cdv-software.com, because a guess that
 * lands on the wrong site is a task pointed at a stranger's server, possibly
 * carrying a login. Where there is nothing to offer, the interface asks rather
 * than inventing.
 */

/** Endings common enough to recognise a bare host by, without a scheme. */
const COMMON_ENDINGS =
  /\.(com|org|net|io|dev|app|co|ai|uk|de|at|ch|fr|es|it|nl|se|no|eu|info|biz|me|tv|shop|club|online|site|store)$/;

/**
 * Websites named in a piece of text, tidied the way the allowlist stores them.
 *
 * Order is preserved and repeats dropped, because the first site in a task's
 * list decides which browser profile it uses.
 */
export function suggestFromText(text: string): string[] {
  if (!text) return [];
  const out: string[] = [];
  const seen = new Set<string>();

  const add = (raw: string) => {
    const host = tidy(raw);
    if (!host || seen.has(host)) return;
    seen.add(host);
    out.push(host);
  };

  // Anything written as a proper address first: those are unambiguous.
  for (const m of text.matchAll(/https?:\/\/[^\s<>"')\]]+/gi)) add(m[0]);

  // Then bare hosts, which need a recognisable ending to be worth offering.
  // Requiring one keeps ordinary prose out: "e.g. book it" is not a website.
  for (const m of text.matchAll(/\b[a-z0-9][a-z0-9-]*(?:\.[a-z0-9][a-z0-9-]*)+\b/gi)) {
    const t = m[0].toLowerCase().replace(/\.$/, "");
    if (COMMON_ENDINGS.test(t) || /^(localhost|\d{1,3}(\.\d{1,3}){3})$/.test(t)) add(t);
  }

  // A bare loopback address with a port, which is how a local service is named.
  for (const m of text.matchAll(/\b(?:localhost|\d{1,3}(?:\.\d{1,3}){3})(?::\d+)?\b/gi)) add(m[0]);

  return out;
}

/**
 * The bare lowercase host, which is the only form the allowlist matches on.
 *
 * The daemon does this properly with a real URL parser and has the final say;
 * this is only good enough to decide what to offer.
 */
function tidy(raw: string): string | null {
  let s = raw.trim().toLowerCase();
  s = s.replace(/^https?:\/\//, "");
  s = s.replace(/^[^@/]*@/, ""); // userinfo
  s = s.split(/[/?#]/)[0]; // path, query, fragment
  s = s.replace(/:\d+$/, ""); // port
  s = s.replace(/\.$/, ""); // a trailing root dot
  if (!s || s.includes("*") || s.includes(",")) return null;
  // A single label is either a whole top-level domain or not a site at all.
  if (!s.includes(".") && s !== "localhost") return null;
  if (!/^[a-z0-9.[\]:-]+$/.test(s)) return null;
  return s;
}
