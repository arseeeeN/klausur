// The in-page half of locators, evaluated as `(<this file>)(chain, op, arg)`.
//
// A chain is a list of steps: {css}, {nth}, {text}, {visible}. CSS pierces open shadow roots
// like Playwright's: a descendant combinator can cross into a shadow tree, so `my-widget input`
// finds the input inside `<my-widget>`'s shadow DOM.
//
// Results: {invalid} for a bad selector, {error} for a mistake retrying can't fix (e.g. filling a
// checkbox), {count, ...} for everything else; the caller decides what to wait for.
(chain, op, arg) => {
  // --- Selector parsing -----------------------------------------------------------------------

  // Split `sel` where `isSep(ch, prev, next)` says so, outside brackets, parentheses and quotes.
  const split = (sel, isSep) => {
    const parts = [];
    let depth = 0, quote = null, start = 0;
    for (let i = 0; i < sel.length; i++) {
      const c = sel[i];
      if (c === '\\') { i++; continue; }
      if (quote) { if (c === quote) quote = null; continue; }
      if (c === '"' || c === "'") quote = c;
      else if (c === '(' || c === '[') depth++;
      else if (c === ')' || c === ']') depth--;
      else if (depth === 0 && isSep(sel, i)) {
        parts.push(sel.slice(start, i));
        start = i + 1;
      }
    }
    parts.push(sel.slice(start));
    return parts.map(p => p.trim()).filter(p => p);
  };
  // A space is a descendant combinator unless a `>`, `+` or `~` combinator is next to it.
  const descendant = (s, i) => {
    if (!/\s/.test(s[i])) return false;
    let p = i - 1; while (p >= 0 && /\s/.test(s[p])) p--;
    let n = i + 1; while (n < s.length && /\s/.test(s[n])) n++;
    return p >= 0 && n < s.length && !'>+~'.includes(s[p]) && !'>+~'.includes(s[n]);
  };

  // Every element matching `simple` (no descendant combinators) inside `scope`, including in
  // the open shadow trees below it.
  const deepQuery = (scope, simple, out) => {
    const visit = root => {
      for (const el of root.querySelectorAll(simple)) out.add(el);
      for (const el of root.querySelectorAll('*')) if (el.shadowRoot) visit(el.shadowRoot);
    };
    if (scope.shadowRoot) visit(scope.shadowRoot);
    visit(scope);
  };
  const css = (scopes, sel) => {
    const out = new Set();
    for (const alternative of split(sel, (s, i) => s[i] === ',')) {
      let current = scopes;
      for (const simple of split(alternative, descendant)) {
        const next = new Set();
        for (const scope of current) deepQuery(scope, simple, next);
        current = [...next];
      }
      current.forEach(el => out.add(el));
    }
    return [...out];
  };

  // --- Element state --------------------------------------------------------------------------

  const isVisible = el => {
    if (!el.isConnected || !el.checkVisibility({ visibilityProperty: true })) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const isEnabled = el => !(
    (['BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'OPTION', 'OPTGROUP'].includes(el.tagName) &&
      (el.disabled || el.closest('fieldset[disabled]'))) ||
    el.closest('[aria-disabled="true"]'));
  // Text inside an element, shadow trees included, with whitespace collapsed.
  const deepText = el => {
    let text = '';
    const walk = node => {
      if (node.nodeType === Node.TEXT_NODE) text += node.data;
      if (node.shadowRoot) walk(node.shadowRoot);
      for (const child of node.childNodes) walk(child);
    };
    walk(el);
    return text.replace(/\s+/g, ' ').trim();
  };
  const describe = el => {
    let s = el.tagName.toLowerCase();
    if (el.id) s += '#' + el.id;
    if (typeof el.className === 'string' && el.className.trim())
      s += '.' + el.className.trim().split(/\s+/).slice(0, 3).join('.');
    const text = deepText(el);
    return '<' + s + '>' + (text ? ' "' + (text.length > 40 ? text.slice(0, 40) + '…' : text) + '"' : '');
  };
  const checkable = el =>
    (el.tagName === 'INPUT' && ['checkbox', 'radio'].includes(el.type)) ||
    ['checkbox', 'radio', 'switch', 'menuitemcheckbox'].includes(el.getAttribute('role'));
  const isChecked = el => el.tagName === 'INPUT' ? el.checked : el.getAttribute('aria-checked') === 'true';
  // What the mouse would hit at a point, looking into shadow trees.
  const hitAt = (x, y) => {
    let hit = document.elementFromPoint(x, y);
    while (hit && hit.shadowRoot) {
      const inner = hit.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === hit) break;
      hit = inner;
    }
    return hit;
  };
  const inside = (el, node) => {
    for (let n = node; n; n = n.parentNode || n.host) if (n === el) return true;
    return false;
  };

  // --- Resolving the chain --------------------------------------------------------------------

  let els = [document];
  try {
    for (const step of chain) {
      if ('css' in step) els = css(els, step.css);
      else if ('nth' in step) {
        const e = els.at(step.nth);
        els = e ? [e] : [];
      } else if ('text' in step) {
        const want = step.text.replace(/\s+/g, ' ').trim().toLowerCase();
        els = els.filter(e => deepText(e).toLowerCase().includes(want) !== !!step.not);
      } else if ('visible' in step) els = els.filter(e => isVisible(e) === step.visible);
    }
  } catch (e) {
    return { invalid: String(e.message) };
  }
  const count = els.length;
  if (op === 'count') return { count };
  if (op === 'texts') return { count, texts: els.map(e => e.innerText.trim()) };
  if (op === 'any-visible') return { count, visible: els.some(isVisible) };
  if (count !== 1) return { count, matches: els.slice(0, 5).map(describe) };

  // --- One element ----------------------------------------------------------------------------

  const el = els[0];
  const out = {
    count, element: describe(el), visible: isVisible(el), enabled: isEnabled(el),
  };
  const kind = el.tagName === 'INPUT' ? (el.getAttribute('type') || 'text').toLowerCase() : '';
  switch (op) {
    case 'state':
      break;
    case 'text':
      out.text = el.innerText.trim();
      break;
    case 'value':
      if (!['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName))
        return { error: `${describe(el)} isn't an <input>, <textarea> or <select>` };
      out.value = String(el.value);
      break;
    case 'checked':
      if (!checkable(el)) return { error: `${describe(el)} isn't a checkbox or radio button` };
      out.checked = isChecked(el);
      break;
    case 'point': {
      // Where a click would land, and whether it would reach the element.
      if (arg === 'check' && !checkable(el))
        return { error: `${describe(el)} isn't a checkbox or radio button` };
      if (arg === 'check') out.checked = isChecked(el);
      if (!out.visible) break;
      el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      const r = el.getBoundingClientRect();
      const x = r.left + r.width / 2, y = r.top + r.height / 2;
      const hit = hitAt(x, y);
      if (hit && inside(el, hit)) { out.x = x; out.y = y; }
      else out.covered_by = hit ? describe(hit) : 'nothing: it is off screen';
      break;
    }
    case 'focus':
      el.focus();
      break;
    case 'fill': {
      if (kind && ['checkbox', 'radio', 'button', 'submit', 'reset', 'file', 'image', 'range',
        'color', 'hidden'].includes(kind))
        return { error: `an input of type "${kind}" can't be filled` };
      if (!['INPUT', 'TEXTAREA'].includes(el.tagName) && !el.isContentEditable)
        return { error: `${describe(el)} isn't an <input>, <textarea> or [contenteditable] element` };
      out.editable = !el.readOnly;
      if (!out.visible || !out.enabled || !out.editable) break;
      // Select what's there, so the typing that follows replaces it.
      el.focus();
      if (el.select) el.select();
      else {
        const range = document.createRange();
        range.selectNodeContents(el);
        const s = getSelection(); s.removeAllRanges(); s.addRange(range);
      }
      out.ready = true;
      break;
    }
    case 'select': {
      if (el.tagName !== 'SELECT') return { error: `${describe(el)} isn't a <select>` };
      if (!out.visible || !out.enabled) break;
      const options = [...el.options];
      const picked = arg.map(v => options.find(o => o.value === v || o.label === v));
      const missing = arg.filter((_, i) => !picked[i]);
      if (missing.length) { out.missing = missing; break; }
      if (picked.length > 1 && !el.multiple)
        return { error: `${describe(el)} takes one option, not ${picked.length}` };
      options.forEach(o => { o.selected = picked.includes(o); });
      el.dispatchEvent(new Event('input', { bubbles: true, composed: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      out.selected = picked.map(o => o.value);
      break;
    }
    case 'file':
      if (kind !== 'file') return { error: `${describe(el)} isn't an <input type="file">` };
      if (arg > 1 && !el.multiple) return { error: `${describe(el)} takes one file, not ${arg}` };
      break;
    case 'node':
      // The element itself, as a handle for protocol calls that take one (setting files).
      return el;
  }
  return out;
}
