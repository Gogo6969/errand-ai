// The browser the Errand agent drives.
//
// Playwright is Node and the backend is Rust, so the browser lives here, in a
// process the Rust core owns and supervises. The important property is that the
// model never talks to this: it emits symbolic actions naming refs, Rust checks
// each one against the run's domain allowlist and safe-action policy, and only
// then does a message arrive on this pipe.
//
// One secret path exists, `secure.fill`, and it is one-way. The value arrives,
// goes into the page, and is dropped. It is stripped from every error and echo
// path, because the single most likely way a password escapes is an exception
// object that happened to carry the arguments that caused it.

import { existsSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { SNAPSHOT_FN, SECURE_BOXES_FN } from './snapshot.js';

let context = null;
let page = null;
let allowedDomains = [];
let strictNetwork = false;
const secureRefs = new Set();

// ---------------------------------------------------------------- plumbing --

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function emit(event, params) {
  send({ event, params });
}

/** Something we wrote ourselves, for a person to read. */
class UserFacingError extends Error {
  constructor(message) {
    super(message);
    this.name = 'UserFacingError';
    this.userFacing = true;
  }
}

/** Never let a thrown value carry a secret out of this process. */
function safeError(e) {
  const msg = (e && e.message) || String(e);
  // Our own sentences are already the length they need to be, and have no call
  // log to strip. The clipping below is for Playwright's messages.
  if (e && e.userFacing) return msg;
  // Playwright puts the full call, including arguments, in its log section.
  return msg.split('\nCall log:')[0].slice(0, 400);
}

// A sidecar that dies on its first breath is indistinguishable, from the Rust
// side, from a slow one: the caller waits out its whole timeout and then says
// the browser took too long, with the real reason nowhere. So every way this
// process can end early says so first.
//
// Both pipes, deliberately. The NDJSON line is the shape a caller can act on,
// but the reader in runner/src/browser.rs logs an event's name and drops its
// params, so on stdout alone the sentence would vanish. Sidecar stderr is
// logged whole, scrubbed, which is what actually puts the reason in front of
// whoever is reading the log.
function reportFatal(kind, message) {
  try {
    send({ event: 'fatal', params: { kind, message } });
  } catch {
    // stdout has gone; stderr may still be there.
  }
  try {
    process.stderr.write(`fatal (${kind}): ${message}\n`);
  } catch {
    // Both pipes are gone. Nothing left to report with.
  }
}

process.on('uncaughtException', (e) => {
  reportFatal('uncaught_exception', safeError(e));
  process.exit(1);
});

process.on('unhandledRejection', (e) => {
  reportFatal('unhandled_rejection', safeError(e));
  process.exit(1);
});

// playwright-core is installed with npm rather than vendored, so it is exactly
// the piece a mispackaged build leaves out. A plain top-level import would fail
// during module resolution, before any line of this file ran, and the process
// would disappear without a word. Loading it by hand keeps the reason sayable.
let chromium;
try {
  ({ chromium } = await import('playwright-core'));
} catch (e) {
  reportFatal(
    'startup',
    'Errand could not start its browser helper, so no web page can be opened. The helper is ' +
      'missing part of its installation. Reinstalling Errand should put it back. Nothing was ' +
      'opened and nothing about your tasks was changed. The underlying error was: ' +
      String((e && e.message) || e).split('\n')[0]
  );
  process.exit(1);
}

function apexOf(hostname) {
  const parts = String(hostname || '').toLowerCase().split('.').filter(Boolean);
  if (parts.length <= 2) return parts.join('.');
  // Good enough for the second enforcement layer; Rust holds the authoritative
  // check using a real public-suffix list.
  const twoLevelTlds = ['co.uk', 'org.uk', 'ac.uk', 'com.au', 'co.jp', 'com.br', 'co.nz'];
  const lastTwo = parts.slice(-2).join('.');
  return twoLevelTlds.includes(lastTwo) ? parts.slice(-3).join('.') : lastTwo;
}

/**
 * Entries arrive from a database row a person typed into, so " EXAMPLE.COM "
 * and "example.com" are the same site. Rust normalises the same way before it
 * compares; when only one of the two layers did, an allowed site was waved
 * through by Rust and then refused here, which looks to the user like the site
 * being broken.
 */
function normaliseDomains(list) {
  return (list || []).map((d) => String(d).trim().toLowerCase()).filter(Boolean);
}

// Which hosts this page asked for files from and did not get them.
//
// A blocked subresource is silent by design: the request never happens, and a
// site whose scripts live on a second domain draws its own "something went
// wrong" instead of its content. So the page looks broken, the fence looks
// innocent, and the agent retries something that can never load. Learned from
// x.com, whose every script comes from abs.twimg.com: a task allowed x.com and
// nothing else gets a shell that cannot start, and no line anywhere said why.
const blockedHosts = new Map();

// The two kinds of file whose absence stops a page working rather than merely
// making it plainer. A missing image is a gap on the screen; a missing script
// is a page that never starts. Only these are worth widening a task's list of
// sites for, which is why the kind is remembered and not just the count.
const RUNS_THE_PAGE = new Set(['script', 'stylesheet']);

function noteBlocked(request) {
  let host;
  try {
    host = new URL(request.url()).hostname.toLowerCase();
  } catch {
    return;
  }
  const before = blockedHosts.get(host);
  const scripts = RUNS_THE_PAGE.has(request.resourceType());
  blockedHosts.set(host, {
    count: (before?.count || 0) + 1,
    scripts: !!before?.scripts || scripts,
  });
  // One event per host per page. A page shell can ask a single CDN for a
  // hundred files, and a hundred identical lines bury the one that matters.
  if (!before) {
    emit('blocked', { url: request.url(), host, reason: 'off_allowlist_resource' });
  }
}

function domainAllowed(url) {
  // An empty list permits nothing, which is what the Rust layer does too. A
  // task that has not said where it may go has not been taught yet, and
  // reading that as "anywhere" is the wrong direction to fail.
  if (!allowedDomains.length) return false;
  try {
    const h = new URL(url).hostname.toLowerCase();
    const apex = apexOf(h);
    return allowedDomains.some((d) => apex === d || h === d || h.endsWith('.' + d));
  } catch {
    return false;
  }
}

function requirePage() {
  if (!page) throw new Error('No browser session is open.');
  return page;
}

async function resolveRef(ref) {
  const p = requirePage();
  const handle = await p.evaluateHandle(
    (r) => {
      const i = parseInt(String(r).replace(/^e/, ''), 10) - 1;
      const el = (window.__errandRefs || [])[i];
      return el || null;
    },
    ref
  );
  const el = handle.asElement();
  if (!el) {
    throw new Error(
      `There is no element ${ref} on this page any more. Take a fresh snapshot; the page may have changed.`
    );
  }
  return el;
}

// ----------------------------------------------------------------- browser --
//
// playwright-core deliberately ships no browser of its own. Left to itself it
// looks for one under ~/Library/Caches/ms-playwright, and on a Mac that has
// never run Playwright it fails with a path into a directory that does not
// exist and an instruction to run npx. Nobody who installed a scheduling app
// is going to do that, so instead we drive a Chrome-family browser the person
// already has, always in a profile of Errand's own.
//
// Where Playwright knows a browser by name we hand it the channel rather than a
// path, because the channel carries that flavour's launch quirks. Its channel
// lookup only ever consults /Applications, so a copy kept in a home folder has
// to be named by path instead.

const CHROME_FAMILY = [
  {
    name: 'Google Chrome',
    rel: 'Google Chrome.app/Contents/MacOS/Google Chrome',
    channel: 'chrome',
  },
  {
    name: 'Microsoft Edge',
    rel: 'Microsoft Edge.app/Contents/MacOS/Microsoft Edge',
    channel: 'msedge',
  },
  { name: 'Chromium', rel: 'Chromium.app/Contents/MacOS/Chromium', channel: null },
  { name: 'Brave', rel: 'Brave Browser.app/Contents/MacOS/Brave Browser', channel: null },
];

const NO_BROWSER_MESSAGE =
  'Errand needs a Chrome-family browser to open web pages, and this Mac does not have one. It ' +
  'looked for Google Chrome, Microsoft Edge, Brave and Chromium in /Applications and ' +
  '~/Applications. Install Google Chrome from https://www.google.com/chrome and run this task ' +
  'again. Errand drives it in a separate profile of its own, so your windows, tabs, history and ' +
  'saved logins are never touched. If your browser is somewhere unusual, set ERRAND_BROWSER to ' +
  'the executable inside it, for example /Applications/Google Chrome.app/Contents/MacOS/Google ' +
  'Chrome.';

/**
 * The first Chrome-family browser on this machine, or null.
 * Returns the name to report and the launch options to merge in.
 */
function findBrowser() {
  const override = process.env.ERRAND_BROWSER;
  if (override && existsSync(override)) {
    return { name: override, launch: { executablePath: override } };
  }

  for (const b of CHROME_FAMILY) {
    const path = join('/Applications', b.rel);
    if (!existsSync(path)) continue;
    return b.channel
      ? { name: b.name, launch: { channel: b.channel } }
      : { name: b.name, launch: { executablePath: path } };
  }

  for (const b of CHROME_FAMILY) {
    const path = join(homedir(), 'Applications', b.rel);
    if (existsSync(path)) return { name: b.name, launch: { executablePath: path } };
  }

  // Last resort: a Chromium some other tool asked Playwright to download. Go
  // through the channel rather than launching the default, because a default
  // headless launch wants chromium_headless_shell, which is a different
  // directory from the one just checked and is often not there.
  try {
    const path = chromium.executablePath();
    if (path && existsSync(path)) {
      return { name: 'Chromium (Playwright)', launch: { channel: 'chromium' } };
    }
  } catch {
    // Nothing registered, which is the ordinary case. Fall through.
  }

  return null;
}

// ----------------------------------------------------------------- methods --

const methods = {
  async 'session.open'({ profile_dir, headless = true, allowed_domains = [], strict_network = false }) {
    if (context) await methods['session.close']({ save_state: true });
    blockedHosts.clear();
    allowedDomains = normaliseDomains(allowed_domains);
    strictNetwork = !!strict_network;

    const browser = findBrowser();
    if (!browser) throw new UserFacingError(NO_BROWSER_MESSAGE);

    context = await chromium.launchPersistentContext(profile_dir, {
      headless,
      viewport: { width: 1280, height: 900 },
      args: ['--disable-blink-features=AutomationControlled'],
      ...browser.launch,
    });
    page = context.pages()[0] || (await context.newPage());

    // Third enforcement layer. Rust is authoritative, the model is told the
    // rules, and this is the belt that catches a redirect neither saw coming.
    await context.route('**/*', (route, request) => {
      const isMain = request.isNavigationRequest() && request.frame() === page.mainFrame();
      if (isMain && !domainAllowed(request.url())) {
        emit('blocked', { url: request.url(), reason: 'off_allowlist_navigation' });
        return route.abort('blockedbyclient');
      }
      if (strictNetwork && !isMain && !domainAllowed(request.url())) {
        noteBlocked(request);
        return route.abort('blockedbyclient');
      }
      return route.continue();
    });

    page.on('framenavigated', (f) => {
      if (f !== page.mainFrame()) return;
      // Each page is refused its own files. Carrying the last one's tally over
      // would have a snapshot blame a host this page never asked for.
      blockedHosts.clear();
      emit('nav', { url: f.url() });
    });
    page.on('dialog', async (d) => {
      emit('dialog', { type: d.type(), message: d.message().slice(0, 300) });
      await d.dismiss().catch(() => {});
    });
    page.on('download', (d) => {
      emit('download', { suggested: d.suggestedFilename() });
      d.cancel().catch(() => {});
    });

    // Which browser this was, so that the day a Chrome update changes something
    // the run log says what was actually driven rather than leaving it a
    // mystery.
    return { ok: true, url: page.url(), browser: browser.name };
  },

  // Widen the list this session enforces, without closing the page that is
  // open on it.
  //
  // Only ever called after the same widening has been written to the task, so
  // this is the fence being told what it already agreed to rather than a way
  // round it. The tally is cleared with it: the hosts in it have just stopped
  // being refused, and a snapshot still reporting them would have the agent
  // announce a problem that was fixed a moment ago.
  async 'session.allow'({ domains = [] }) {
    allowedDomains = normaliseDomains(domains);
    blockedHosts.clear();
    return { ok: true, allowed: allowedDomains };
  },

  // The same ladder, without launching anything, so a health check can say
  // whether this Mac can browse at all before a task depends on it.
  async 'browser.probe'() {
    const browser = findBrowser();
    if (!browser) return { found: false, message: NO_BROWSER_MESSAGE };
    return { found: true, name: browser.name };
  },

  async 'session.close'({ save_state = true } = {}) {
    try {
      if (context) {
        if (save_state) await context.storageState().catch(() => {});
        await context.close();
      }
    } catch { /* closing is best effort */ }
    context = null;
    page = null;
    secureRefs.clear();
    return { ok: true };
  },

  async 'page.goto'({ url, timeout_ms = 30000 }) {
    const p = requirePage();
    if (!domainAllowed(url)) {
      throw new Error(`${url} is outside the sites this task is allowed to visit.`);
    }
    await p.goto(url, { timeout: timeout_ms, waitUntil: 'domcontentloaded' });
    return { url: p.url(), title: await p.title() };
  },

  async 'page.snapshot'() {
    const p = requirePage();
    secureRefs.clear();
    const snap = await p.evaluate(SNAPSHOT_FN);
    // Travelling with the page rather than sitting in a log nobody reads is
    // what turns "this site is broken" into "this site wanted a domain the
    // task does not allow".
    snap.blocked = [...blockedHosts].map(([host, seen]) => ({ host, ...seen }));
    return snap;
  },

  async 'page.act'({ kind, ref, text, value, key, timeout_ms = 15000 }) {
    const p = requirePage();
    switch (kind) {
      case 'click': {
        const el = await resolveRef(ref);
        await el.click({ timeout: timeout_ms });
        return { ok: true };
      }
      case 'type': {
        const el = await resolveRef(ref);
        await el.fill(String(text ?? ''), { timeout: timeout_ms });
        return { ok: true };
      }
      case 'select': {
        const el = await resolveRef(ref);
        await el.selectOption(String(value ?? ''), { timeout: timeout_ms });
        return { ok: true };
      }
      case 'check': {
        const el = await resolveRef(ref);
        await el.check({ timeout: timeout_ms });
        return { ok: true };
      }
      case 'press': {
        await p.keyboard.press(String(key ?? 'Enter'));
        return { ok: true };
      }
      case 'scroll': {
        await p.mouse.wheel(0, value === 'up' ? -600 : 600);
        return { ok: true };
      }
      default:
        throw new Error(`Unknown action '${kind}'.`);
    }
  },

  async 'page.wait'({ for: what, value, timeout_ms = 15000 }) {
    const p = requirePage();
    if (what === 'text') {
      await p.getByText(String(value), { exact: false }).first().waitFor({ timeout: timeout_ms });
    } else if (what === 'url') {
      await p.waitForURL((u) => String(u).includes(String(value)), { timeout: timeout_ms });
    } else {
      await p.waitForLoadState('networkidle', { timeout: timeout_ms }).catch(() => {});
    }
    return { ok: true, url: p.url() };
  },

  // The only path a secret takes. Everything about this function exists to make
  // sure the value it receives cannot come back out.
  async 'secure.fill'({ ref, value, label }) {
    const el = await resolveRef(ref);
    try {
      await el.fill(String(value ?? ''), { timeout: 15000 });
    } catch (e) {
      // Rethrow without the original, whose message and call log may quote the
      // value that was being typed.
      throw new Error(`Could not fill the ${label || 'credential'} field.`);
    } finally {
      value = null;
    }
    secureRefs.add(ref);
    return { ok: true, filled: label || 'credential' };
  },

  async 'page.screenshot'({ mask_secure = true, path }) {
    const p = requirePage();
    let masks = [];
    if (mask_secure) {
      // Mask before the pixels exist rather than blurring afterwards.
      const boxes = await p.evaluate(SECURE_BOXES_FN);
      masks = boxes.map((b) =>
        p.locator('body').locator('xpath=.').first()
      );
      // Playwright's mask takes locators; simpler and more reliable is to hide
      // the values outright for the duration of the capture.
      await p.addStyleTag({
        content:
          'input[type="password"],input[autocomplete*="password"],input[autocomplete*="cc-"]' +
          '{ -webkit-text-security: disc !important; color: transparent !important;' +
          ' background-image: repeating-linear-gradient(45deg,#333,#333 6px,#555 6px,#555 12px) !important; }',
      });
    }
    const buf = await p.screenshot({ path, fullPage: false, type: 'png' });
    return { bytes: path ? undefined : buf.toString('base64'), path, masked: mask_secure };
  },

  async 'captcha.detect'() {
    const p = requirePage();
    const found = await p.evaluate(`(() => {
      const sel = [
        'iframe[src*="recaptcha"]','iframe[src*="hcaptcha"]','iframe[src*="turnstile"]',
        '.g-recaptcha','.h-captcha','#cf-challenge-running','[data-sitekey]'
      ];
      for (const s of sel) if (document.querySelector(s)) return s;
      if (/verify you are human|are you a robot|complete the security check/i.test(document.body.innerText || '')) {
        return 'text-challenge';
      }
      return null;
    })()`);
    return { captcha: found };
  },

  async ping() {
    return { pong: true, hasSession: !!context };
  },
};

// -------------------------------------------------------------------- loop --

let buffer = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', async (chunk) => {
  buffer += chunk;
  let idx;
  while ((idx = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;

    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    const { id, method, params } = msg;
    const fn = methods[method];
    if (!fn) {
      send({ id, error: { message: `Unknown method '${method}'.` } });
      continue;
    }
    try {
      const result = await fn(params || {});
      send({ id, result });
    } catch (e) {
      send({ id, error: { message: safeError(e) } });
    }
  }
});

process.stdin.on('end', async () => {
  await methods['session.close']({ save_state: true }).catch(() => {});
  process.exit(0);
});

emit('ready', { pid: process.pid });
