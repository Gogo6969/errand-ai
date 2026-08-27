// Injected into the page to build the view the agent works from.
//
// This is deliberately our own rather than Playwright's aria snapshot: we need
// ref stability we control, and we need to mark secure fields ourselves so the
// value of a password box is never rendered into something the model reads.
//
// Runs in page context. Returns a compact text tree plus the ref table.

export const SNAPSHOT_FN = `(() => {
  const MAX = 500;
  const refs = [];
  window.__errandRefs = refs;

  // Would a click here actually reach this element?
  //
  // Drawn and reachable are different questions, and only the first one used to
  // be asked. A page that opens a modal leaves the form behind it fully
  // rendered, the right size, and completely unreachable, so a snapshot listing
  // both hands the agent two of every field and no way to tell which one is
  // live. It picks wrong about half the time and then cannot say why.
  //
  // Watched happen on x.com: the run filled the username box behind the login
  // dialog, pressed a Continue the overlay was swallowing, and reported that
  // the button would not respond to a click. It was right. Nothing it could
  // read said the thing was buried.
  //
  // The same test the browser itself would apply on the way to a click, so an
  // element that survives this is one an action can actually use.
  const reachable = (el) => {
    const r = el.getBoundingClientRect();
    const x = r.left + r.width / 2;
    const y = r.top + r.height / 2;
    // Off-screen is not covered. A control below the fold is reached by
    // scrolling to it, and a hit test cannot see outside the window, so
    // everything there would fail this and the page would come back empty.
    if (x < 0 || y < 0 || x > window.innerWidth || y > window.innerHeight) return true;
    const hit = document.elementFromPoint(x, y);
    if (!hit) return false;
    // Its own child is still it: a button's centre belongs to the span inside
    // it, and a click on that span is a click on the button.
    return hit === el || el.contains(hit) || hit.contains(el);
  };

  // Does this element hide everything inside it?
  //
  // Only these three do, and they are the only grounds for skipping a whole
  // subtree. A box of zero size is not one of them, however much it looks like
  // one: flex and grid layouts are full of zero-height wrappers whose children
  // are positioned out of them and painted perfectly. Treating those as hidden
  // is how Errand came to be blind to x.com's sign-in dialog, which lives
  // inside a 1280x0 div. Every control the agent was offered on that page was
  // the copy on the page underneath, buried by the dialog's own backdrop, and
  // "the Continue button will not respond" was the honest report of a run
  // clicking the only Continue it could see.
  const hidesSubtree = (el) => {
    const s = window.getComputedStyle(el);
    return s.display === 'none' || s.visibility === 'hidden' || s.opacity === '0';
  };

  /** Is the element itself drawn? Asked about the line, never about descending. */
  const isDrawn = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };

  // A field is secure if the browser treats it as one, or if it is plainly
  // named like one. Erring toward secure is the safe direction: the cost is a
  // hidden value, and the cost of the other mistake is a leaked password.
  const isSecure = (el) => {
    if (el.tagName !== 'INPUT') return false;
    const t = (el.type || '').toLowerCase();
    if (t === 'password') return true;
    const ac = (el.getAttribute('autocomplete') || '').toLowerCase();
    if (/password|cc-number|cc-csc|one-time-code/.test(ac)) return true;
    const hint = ((el.name || '') + ' ' + (el.id || '') + ' ' + (el.getAttribute('aria-label') || '')).toLowerCase();
    return /passw|passcode|secret|cvv|cvc|card.?number|otp|2fa|token|pin\\b/.test(hint);
  };

  const label = (el) => {
    const aria = el.getAttribute('aria-label');
    if (aria) return aria.trim();
    if (el.id) {
      const l = document.querySelector('label[for="' + CSS.escape(el.id) + '"]');
      if (l && l.innerText.trim()) return l.innerText.trim();
    }
    const wrap = el.closest('label');
    if (wrap && wrap.innerText.trim()) return wrap.innerText.trim();
    const ph = el.getAttribute('placeholder');
    if (ph) return ph.trim();
    const t = (el.innerText || el.value || el.getAttribute('title') || el.getAttribute('name') || '').trim();
    return t.slice(0, 120);
  };

  const roleOf = (el) => {
    const explicit = el.getAttribute('role');
    if (explicit) return explicit;
    const tag = el.tagName.toLowerCase();
    if (tag === 'a') return el.href ? 'link' : 'generic';
    if (tag === 'button') return 'button';
    if (tag === 'select') return 'combobox';
    if (tag === 'textarea') return 'textbox';
    if (tag === 'input') {
      const t = (el.type || 'text').toLowerCase();
      if (t === 'checkbox') return 'checkbox';
      if (t === 'radio') return 'radio';
      if (t === 'submit' || t === 'button') return 'button';
      return 'textbox';
    }
    if (/^h[1-6]$/.test(tag)) return 'heading';
    return 'generic';
  };

  const interactive = (el) => {
    const tag = el.tagName.toLowerCase();
    if (['a', 'button', 'input', 'select', 'textarea'].includes(tag)) return true;
    const r = el.getAttribute('role');
    if (r && ['button', 'link', 'checkbox', 'radio', 'tab', 'menuitem', 'option', 'switch'].includes(r)) return true;
    return el.hasAttribute('onclick') || (el.tabIndex >= 0 && el.getAttribute('tabindex') !== null);
  };

  const lines = [];
  const walk = (node, depth) => {
    if (lines.length >= MAX) return;
    for (const el of node.children) {
      if (!(el instanceof HTMLElement)) continue;
      const tag = el.tagName.toLowerCase();
      if (['script', 'style', 'noscript', 'svg', 'head'].includes(tag)) continue;
      if (hidesSubtree(el)) continue;

      const pad = '  '.repeat(Math.min(depth, 8));
      const drawn = isDrawn(el);
      if (drawn && interactive(el) && reachable(el)) {
        const ref = 'e' + (refs.length + 1);
        refs.push(el);
        const role = roleOf(el);
        const parts = [role, JSON.stringify(label(el) || '')];
        parts.push('[ref=' + ref + ']');
        if (isSecure(el)) {
          parts.push('[secure] value=[hidden]');
        } else if (el.value && el.type !== 'password') {
          parts.push('value=' + JSON.stringify(String(el.value).slice(0, 60)));
        }
        // A control that submits a form is committing something. This is a far
        // better signal than the words on it, because "Book a court" can be a
        // navigation link and "Continue" can be a purchase.
        const submits =
          (tag === 'button' && (el.type === 'submit' || !el.type || el.type === '') && el.closest('form')) ||
          (tag === 'input' && el.type === 'submit');
        if (submits) parts.push('[submit]');
        if (el.disabled) parts.push('[disabled]');
        if (el.checked) parts.push('[checked]');
        if (tag === 'a' && el.href) parts.push('-> ' + el.getAttribute('href'));
        lines.push(pad + '- ' + parts.join(' '));
      } else if (interactive(el)) {
        // Either not drawn, or drawn under something else. No ref either way:
        // a ref is an offer to act on a thing, and neither of these can be
        // acted on. Its children are still walked, because the element
        // covering something is often a child of the thing it covers.
      } else if (drawn && /^h[1-6]$/.test(tag)) {
        const t = (el.innerText || '').trim().slice(0, 160);
        if (t) lines.push(pad + '- heading ' + JSON.stringify(t));
      } else if (drawn) {
        const own = Array.from(el.childNodes)
          .filter((n) => n.nodeType === 3)
          .map((n) => n.textContent.trim())
          .join(' ')
          .trim();
        if (own && own.length > 1) {
          lines.push(pad + '- text ' + JSON.stringify(own.slice(0, 200)));
        }
      }
      walk(el, depth + 1);
    }
  };

  walk(document.body, 0);
  return {
    url: location.href,
    title: document.title,
    truncated: lines.length >= MAX,
    tree: lines.join('\\n'),
    refCount: refs.length,
  };
})()`;

/** Bounding boxes of every secure field, for masking a screenshot. */
export const SECURE_BOXES_FN = `(() => {
  const out = [];
  for (const el of document.querySelectorAll('input')) {
    const t = (el.type || '').toLowerCase();
    const ac = (el.getAttribute('autocomplete') || '').toLowerCase();
    const hint = ((el.name||'')+' '+(el.id||'')).toLowerCase();
    const secure = t === 'password'
      || /password|cc-number|cc-csc|one-time-code/.test(ac)
      || /passw|secret|cvv|cvc|card.?number|otp|pin\\b/.test(hint);
    if (!secure) continue;
    const r = el.getBoundingClientRect();
    if (r.width > 0 && r.height > 0) {
      out.push({ x: r.x, y: r.y, width: r.width, height: r.height });
    }
  }
  return out;
})()`;
