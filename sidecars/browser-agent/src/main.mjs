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

import { chromium } from 'playwright-core';
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

/** Never let a thrown value carry a secret out of this process. */
function safeError(e) {
  const msg = (e && e.message) || String(e);
  // Playwright puts the full call, including arguments, in its log section.
  return msg.split('\nCall log:')[0].slice(0, 400);
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

function domainAllowed(url) {
  if (!allowedDomains.length) return true;
  try {
    const h = new URL(url).hostname;
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

// ----------------------------------------------------------------- methods --

const methods = {
  async 'session.open'({ profile_dir, headless = true, allowed_domains = [], strict_network = false }) {
    if (context) await methods['session.close']({ save_state: true });
    allowedDomains = allowed_domains || [];
    strictNetwork = !!strict_network;

    context = await chromium.launchPersistentContext(profile_dir, {
      headless,
      viewport: { width: 1280, height: 900 },
      args: ['--disable-blink-features=AutomationControlled'],
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
        return route.abort('blockedbyclient');
      }
      return route.continue();
    });

    page.on('framenavigated', (f) => {
      if (f === page.mainFrame()) emit('nav', { url: f.url() });
    });
    page.on('dialog', async (d) => {
      emit('dialog', { type: d.type(), message: d.message().slice(0, 300) });
      await d.dismiss().catch(() => {});
    });
    page.on('download', (d) => {
      emit('download', { suggested: d.suggestedFilename() });
      d.cancel().catch(() => {});
    });

    return { ok: true, url: page.url() };
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
