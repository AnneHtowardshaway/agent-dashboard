// Multi-host DOM surface adapter for Codex Desktop (ChatGPT.app).
// Keeps host-specific DOM discovery and React Fiber conversation ID resolution
// isolated from the shared Antigravity widget in inline-widget.cjs.
module.exports = function createMultiHostSurface(options) {
  const opts = options && typeof options === 'object' ? options : {};
  const hostKind = opts.host || 'codex';
  const ID_RE = /^(?:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|ses_[a-zA-Z0-9_-]{6,64}|[a-z0-9_-]{8,64})$/i;
  const visible = e => Boolean(e && e.isConnected && !e.closest('[hidden],[inert]') && e.getClientRects().length > 0 && getComputedStyle(e).visibility !== 'hidden');

  function readFiberProp(startNode, keys, maxElementHops = 12, maxFiberHops = 50) {
    let cur = startNode;
    for (let d = 0; cur && d < maxElementHops; d++, cur = cur.parentElement) {
      const fk = Object.keys(cur).find(k => k.startsWith('__reactFiber$'));
      if (!fk) continue;
      for (let f = cur[fk], h = 0; f && h < maxFiberHops; f = f.return, h++) {
        const p = f.memoizedProps;
        if (!p || typeof p !== 'object') continue;
        for (const key of keys) {
          if (Object.hasOwn(p, key) && typeof p[key] === 'string' && ID_RE.test(p[key].trim())) {
            return p[key].trim();
          }
        }
      }
      break;
    }
    return null;
  }

  function findCodexComposers() {
    const editors = Array.from(document.querySelectorAll('.ProseMirror[role="textbox"], [role="textbox"][contenteditable="true"]')).filter(visible);
    const pairs = [];
    const claimed = new Set();
    for (const editor of editors) {
      if (claimed.has(editor)) continue;
      let box = editor.closest('[class*="ComposerLayoutFooter"]') || editor.parentElement;
      for (let i = 0; i < 6 && box && !box.querySelector('button'); i++) box = box.parentElement;
      if (!box || !visible(box)) continue;

      // Prefer the right-side flex-1 justify-end container next to the model selector,
      // or fall back to the widest visible horizontal flex row inside the composer footer.
      const candidates = Array.from(box.querySelectorAll('div')).filter(el => {
        if (!visible(el)) return false;
        const s = getComputedStyle(el);
        return (s.display === 'flex' || s.display === 'inline-flex') && s.flexDirection === 'row' && el.querySelector('button');
      });
      if (!candidates.length) continue;

      let row = candidates.find(el => {
        const cls = String(el.className || '');
        return cls.includes('flex-1') && cls.includes('justify-end') && el.getBoundingClientRect().width >= 120;
      });
      if (!row) {
        candidates.sort((a, b) => b.getBoundingClientRect().width - a.getBoundingClientRect().width);
        row = candidates[0];
      }
      if (!row) continue;

      const btns = Array.from(row.querySelectorAll('button, [role="button"], [role="combobox"]')).filter(visible);
      if (!btns.length) continue;
      const model = btns.find(b => /GPT|o[1-9]|codex|model|推理|模型/i.test(b.textContent || b.getAttribute('aria-label') || '')) || btns[0];
      let branch = model;
      while (branch && branch.parentElement !== row) branch = branch.parentElement;
      if (!branch) branch = row.firstElementChild;
      if (!branch) continue;

      claimed.add(editor);
      if (claimed.has(row)) continue;
      pairs.push({ editor, model, row, branch, box, kind: 'main' });
    }
    return pairs;
  }

  function findComposers() {
    if (hostKind === 'codex') return findCodexComposers();
    return [];
  }

  function findComposer(previous) {
    const pairs = findComposers();
    if (pairs.length === 1) return pairs[0];
    return pairs.find(p => p.editor === document.activeElement) || pairs.find(p => p.editor === previous?.editor) || pairs[0] || null;
  }

  function conversationState(composer) {
    if (!composer || !composer.editor || !composer.editor.isConnected) return { id: null, status: 'unavailable' };
    if (hostKind === 'codex') {
      const id = readFiberProp(composer.editor, ['conversationId', 'threadId', 'sessionId'], 12, 50);
      if (id) return { id: id.toLowerCase(), status: 'active' };
      const urlMatch = location.pathname.match(/\/(?:t|c|threads?)\/([0-9a-f-]{8,64})/i);
      if (urlMatch) return { id: urlMatch[1].toLowerCase(), status: 'active' };
      return { id: 'latest', status: 'active' };
    }
    return { id: 'latest', status: 'active' };
  }

  return {
    useSharedComposer: false,
    showEmpty: true,
    usageHostLabel: hostKind === 'codex' ? 'Codex' : 'Antigravity',
    findComposers,
    findComposer,
    conversationState,
    conversationId: composer => conversationState(composer).id,
    nativeReserve: () => 0,
  };
};
