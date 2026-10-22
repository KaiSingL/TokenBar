const $ = (id) => document.getElementById(id);
const cardsEl = $("cards");
const metaEl = $("meta");
const liveEl = $("live");
const UI_POLL_MS = 10_000;
let pollTimer = null;
let tickTimer = null;
let intervalSecs = 60;
let snapshot = null;
const expandedCards = new Set();
// Credential form state, keyed by account name. Survives re-renders so the
// 10s status poll never eats a pasted key.
const openCredCards = new Set();
const credNotes = new Map(); // name -> { text, bad }
const pendingCred = new Set();
// Local key stores (labels only — the server never sends values), cached
// briefly so the 10s poll does not re-read the stores on every paint.
let keySourcesCache = null;
let keySourcesFetchedAt = 0;
const KEY_SOURCES_TTL_MS = 60_000;
// Mirrors AppConfig::show_credentials: when false the dashboard renders the
// plain usage cards only, exactly as it did before credential entry existed.
let showCredentials = false;

function hiddenMask(meters) {
  if (!meters || !meters.length) return [];
  const mask = new Array(meters.length).fill(false);
  for (let i = meters.length - 1; i >= 1; i--) {
    if (Number(meters[i].usage_percent) >= 100) {
      for (let j = 0; j < i; j++) mask[j] = true;
      break;
    }
  }
  return mask;
}

function cardKey(a) { return a.name + "|" + (a.provider || ""); }

function level(pct) {
  if (pct >= 85) return "bad";
  if (pct >= 60) return "warn";
  return "ok";
}

function fmtTime(iso) {
  if (!iso) return "not yet";
  try {
    return new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  } catch {
    return iso;
  }
}

function escapeHtml(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function statusLabel(status) {
  if (status === "ready") return "synced";
  return String(status).replaceAll("_", " ");
}

function providerLabel(provider) {
  const p = String(provider || "");
  if (p === "opencode_go" || p === "open_code_go") return "opencode go";
  return p.replaceAll("_", " ");
}

function formatReset(secs) {
  secs = Math.max(0, Math.floor(Number(secs) || 0));
  if (secs === 0) return "now";
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (days > 0) return hours > 0 ? `${days}d ${hours}h` : `${days}d`;
  if (hours > 0) return mins > 0 ? `${hours}h ${mins}m` : `${hours}h`;
  if (mins > 0) return `${mins}m`;
  return `${s}s`;
}

function remainingReset(baseSec, fetchedAt) {
  const elapsed = Math.floor((Date.now() - fetchedAt) / 1000);
  return Math.max(0, (Number(baseSec) || 0) - elapsed);
}

function isGoProvider(p) {
  return p === "opencode_go" || p === "open_code_go";
}

function cssEscape(s) {
  return window.CSS && CSS.escape ? CSS.escape(s) : String(s).replace(/"/g, '\\"');
}

function credRowHtml(a) {
  const c = a.credential;
  const kindTxt = !c
    ? "none"
    : c.kind === "api_key"
      ? "api key"
      : c.kind === "key_source"
        ? "key source"
        : "cookie";
  // A store-resolved key shows where it comes from: "hermes:lks00 · sk-3YL…".
  let maskTxt;
  if (!c) {
    maskTxt = "no key or cookie stored";
  } else if (c.kind === "api_key" && c.source) {
    maskTxt = `${c.source} · ${c.masked || ""}`;
  } else {
    maskTxt = String(c.masked || "");
  }
  const btn = isGoProvider(a.provider)
    ? `<button type="button" class="cred-btn" data-cred-open="${escapeHtml(a.name)}">${c ? "update key" : "set key"}</button>`
    : "";
  return `<div class="cred-row">
    <span class="cred-kind${c ? "" : " none"}">${kindTxt}</span>
    <span class="cred-mask">${escapeHtml(maskTxt)}</span>
    ${btn}
  </div>`;
}

function credNoteHtml(a) {
  const note = credNotes.get(a.name);
  if (!note) return "";
  return `<div class="note${note.bad ? " bad" : " dim"}">${escapeHtml(note.text)}</div>`;
}

function srcListHtml(a) {
  if (!keySourcesCache) {
    return `<div class="src-hint">checking local key stores…</div>`;
  }
  if (!keySourcesCache.length) {
    return `<div class="src-hint">no keys found in the local stores (hermes / opencode)</div>`;
  }
  return `<div class="src-list">${keySourcesCache
    .map((s) => {
      const ref = s.source_ref || (s.store + (s.label ? `:${s.label}` : ""));
      const dis = !s.usable || pendingCred.has(a.name) ? " disabled" : "";
      const note = s.usable
        ? s.status
          ? `${s.store} · ${s.status}`
          : s.store
        : s.reason || s.store;
      return `<button type="button" class="src-row" data-cred-src="${escapeHtml(ref)}" title="${escapeHtml(note)}"${dis}>
        <span class="src-ref">${escapeHtml(ref)}</span>
        <span class="src-note">${escapeHtml(note)}</span>
      </button>`;
    })
    .join("")}</div>`;
}

function credFormHtml(a) {
  const busy = pendingCred.has(a.name);
  const dis = busy ? " disabled" : "";
  return `<div class="cred-form" data-cred-name="${escapeHtml(a.name)}">
    <div class="cred-hint">Create a key in opencode.ai under your workspace's API Keys page, then paste it here.</div>
    <div class="cred-input-row">
      <input class="cred-input" type="password" autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false" placeholder="paste opencode api key" aria-label="API key for ${escapeHtml(a.name)}" inputmode="text" enterkeyhint="done"${dis} />
      <button type="button" class="cred-eye" aria-label="show key"${dis}>show</button>
    </div>
    <div class="cred-actions">
      <button type="button" class="cred-save" data-cred-save="${escapeHtml(a.name)}"${dis}>${busy ? "checking…" : "save key"}</button>
      <button type="button" class="cred-cancel" data-cred-cancel="${escapeHtml(a.name)}"${dis}>cancel</button>
    </div>
    <div class="src-section">
      <div class="src-hint">or adopt a key already on this machine (it stays in its store; tokenbar reads it at fetch time)</div>
      ${srcListHtml(a)}
    </div>
  </div>`;
}

function findCredForm(name) {
  return cardsEl.querySelector(`.cred-form[data-cred-name="${cssEscape(name)}"]`);
}

// Preserve typed text / visibility / focus across innerHTML repaints.
function captureCredForms() {
  const state = {};
  cardsEl.querySelectorAll(".cred-form[data-cred-name]").forEach((form) => {
    const input = form.querySelector(".cred-input");
    if (!input) return;
    state[form.dataset.credName] = {
      value: input.value,
      shown: input.type === "text",
      focus: document.activeElement === input,
    };
  });
  return state;
}

function restoreCredForms(state) {
  Object.entries(state).forEach(([name, s]) => {
    const form = findCredForm(name);
    if (!form) return;
    const input = form.querySelector(".cred-input");
    if (!input) return;
    input.value = s.value;
    input.type = s.shown ? "text" : "password";
    const eye = form.querySelector(".cred-eye");
    if (eye) eye.textContent = s.shown ? "hide" : "show";
    if (s.focus) input.focus();
  });
}

// Fetch the local key-source listing (labels only) unless a fresh copy is
// cached. Repaints so the list appears as soon as it lands.
async function ensureKeySources(force = false) {
  const fresh = Date.now() - keySourcesFetchedAt < KEY_SOURCES_TTL_MS;
  if (!force && keySourcesCache && fresh) return;
  try {
    const res = await fetch("/api/key-sources", { cache: "no-store" });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    keySourcesCache = Array.isArray(data.sources) ? data.sources : [];
    keySourcesFetchedAt = Date.now();
  } catch {
    // Keep whatever we had; an empty list still renders a hint.
    if (!keySourcesCache) keySourcesCache = [];
    keySourcesFetchedAt = Date.now();
  }
  paintCards();
}

// Shared POST + response handling for both credential kinds. `body` is
// {kind:"api_key",value} or {kind:"key_source",value:"hermes:lks00"}.
async function postCredential(name, body) {
  if (!name || pendingCred.has(name)) return;
  pendingCred.add(name);
  credNotes.set(name, { text: "checking…", bad: false });
  paintCards();
  try {
    const res = await fetch(`/api/accounts/${encodeURIComponent(name)}/credential`, {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-TokenBar": "1" },
      body: JSON.stringify(body),
    });
    let data = {};
    try {
      data = await res.json();
    } catch {
      /* non-JSON error body */
    }
    pendingCred.delete(name);
    if (res.ok && data.ok) {
      openCredCards.delete(name);
      credNotes.set(name, { text: String(data.note || "saved"), bad: false });
      await fetchStatus();
    } else {
      credNotes.set(name, { text: String(data.error || `HTTP ${res.status}`), bad: true });
      paintCards();
    }
  } catch (e) {
    pendingCred.delete(name);
    credNotes.set(name, { text: (e && e.message) || "network error", bad: true });
    paintCards();
  }
}

async function submitCred(name) {
  if (!name || pendingCred.has(name)) return;
  const form = findCredForm(name);
  const input = form ? form.querySelector(".cred-input") : null;
  if (!form || !input) return;
  const value = (input.value || "").trim();
  if (!value) {
    credNotes.set(name, { text: "paste an api key first", bad: true });
    paintCards();
    return;
  }
  postCredential(name, { kind: "api_key", value });
}

function submitKeySource(name, ref) {
  postCredential(name, { kind: "key_source", value: ref });
}

function render(data, fetchedAt) {
  intervalSecs = data.refresh_interval_secs || 60;
  showCredentials = !!data.show_credentials;
  const refreshing = !!data.is_refreshing;
  liveEl.className = "live " + (refreshing ? "sync" : data.last_refresh ? "" : "idle");
  liveEl.textContent = refreshing ? "sync" : data.last_refresh ? "live" : "idle";
  metaEl.textContent = `last ${fmtTime(data.last_refresh)} · poll ${intervalSecs}s`;

  const accounts = data.accounts || [];
  snapshot = { fetchedAt: fetchedAt || Date.now(), accounts };

  if (!accounts.length) {
    cardsEl.innerHTML = `<div class="empty">No accounts configured.<br/>Add accounts with <code>tokenbar account login</code>.</div>`;
    return;
  }

  paintCards();
}

function paintCards() {
  if (!snapshot) return;
  const { fetchedAt, accounts } = snapshot;
  const savedForms = captureCredForms();

  cardsEl.innerHTML = accounts.map((a) => {
    const status = a.status || "error";
    const key = cardKey(a);
    const isExpanded = expandedCards.has(key);
    let body = "";
    let hasHidden = false;

    if (a.meters && a.meters.length) {
      const mask = hiddenMask(a.meters);
      const totalHidden = mask.filter(Boolean).length;

      body = a.meters.map((m, i) => {
        const pct = Math.max(0, Math.min(100, Number(m.usage_percent) || 0));
        const lv = level(pct);
        const left = remainingReset(m.reset_in_sec, fetchedAt);
        const hiddenAttr = mask[i] ? ' data-hidden=""' : "";
        return `<div class="meter"${hiddenAttr} data-meter="${i}">
          <div class="meter-row">
            <span class="meter-label">${escapeHtml(m.label)}</span>
            <span class="meter-pct ${lv}">${pct.toFixed(0)}%</span>
          </div>
          <div class="bar"><i class="${lv}" style="width:${pct}%"></i></div>
          <div class="reset" data-meter="${i}">resets ${escapeHtml(formatReset(left))}</div>
        </div>`;
      }).join("");

      hasHidden = totalHidden > 0;
    } else if (status === "loading") {
      body = `<div class="note dim">Fetching usage…</div>`;
    } else if (status === "no_session") {
      body = `<div class="note dim">${escapeHtml(a.error || "No session")}</div>`;
    } else if (status === "not_subscribed") {
      body = `<div class="note dim">${escapeHtml(a.error || "No OpenCode Go subscription")}</div>`;
    } else if (status === "error") {
      body = `<div class="note bad">${escapeHtml(a.error || "Error")}</div>`;
    }

    if (status === "stale" && a.error) {
      body += `<div class="note">stale · ${escapeHtml(a.error)}</div>`;
    }

    const credHtml = showCredentials
      ? `${credRowHtml(a)}${credNoteHtml(a)}${openCredCards.has(a.name) ? credFormHtml(a) : ""}`
      : "";

    const cardCls = hasHidden && isExpanded ? "card expanded" : "card";
    return `<article class="${cardCls} ${escapeHtml(status)}">
      <div class="card-top">
        <div>
          <span class="card-title">${escapeHtml(a.name)}</span>
          <span class="provider"> · ${escapeHtml(providerLabel(a.provider))}</span>
        </div>
        <div class="card-top-actions">
          <span class="badge ${escapeHtml(status)}">${escapeHtml(statusLabel(status))}</span>
          ${hasHidden ? `<span class="chevron-btn${isExpanded ? " expanded" : ""}" data-key="${escapeHtml(key)}" role="button" tabindex="0"><svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg></span>` : ""}
        </div>
      </div>
      ${body}
      ${credHtml}
    </article>`;
  }).join("");

  restoreCredForms(savedForms);
}

cardsEl.addEventListener("click", async (e) => {
  const openBtn = e.target.closest("[data-cred-open]");
  if (openBtn) {
    const name = openBtn.dataset.credOpen;
    openCredCards.add(name);
    credNotes.delete(name);
    paintCards();
    const form = findCredForm(name);
    if (form) {
      const input = form.querySelector(".cred-input");
      if (input) input.focus();
    }
    ensureKeySources();
    return;
  }
  const srcBtn = e.target.closest("[data-cred-src]");
  if (srcBtn) {
    const form = srcBtn.closest(".cred-form");
    const name = form ? form.dataset.credName : null;
    if (name && !srcBtn.disabled) submitKeySource(name, srcBtn.dataset.credSrc);
    return;
  }
  const cancelBtn = e.target.closest("[data-cred-cancel]");
  if (cancelBtn) {
    const name = cancelBtn.dataset.credCancel;
    openCredCards.delete(name);
    credNotes.delete(name);
    paintCards();
    return;
  }
  const eyeBtn = e.target.closest(".cred-eye");
  if (eyeBtn) {
    const input = eyeBtn.closest(".cred-form")?.querySelector(".cred-input");
    if (input) {
      const show = input.type === "password";
      input.type = show ? "text" : "password";
      eyeBtn.textContent = show ? "hide" : "show";
      eyeBtn.setAttribute("aria-label", show ? "hide key" : "show key");
    }
    return;
  }
  const saveBtn = e.target.closest("[data-cred-save]");
  if (saveBtn) {
    submitCred(saveBtn.dataset.credSave);
    return;
  }
  const chevron = e.target.closest(".chevron-btn");
  if (!chevron) return;
  const key = chevron.dataset.key;
  const card = chevron.closest(".card");
  chevron.classList.toggle("expanded");
  card.classList.toggle("expanded");
  if (expandedCards.has(key)) {
    expandedCards.delete(key);
  } else {
    expandedCards.add(key);
  }
});

cardsEl.addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;
  const input = e.target.closest(".cred-input");
  if (!input) return;
  e.preventDefault();
  const form = input.closest(".cred-form");
  if (form) submitCred(form.dataset.credName);
});

function tickResets() {
  if (!snapshot) return;
  const articles = cardsEl.querySelectorAll(".card");
  snapshot.accounts.forEach((a, ai) => {
    if (!a.meters || !a.meters.length) return;
    const article = articles[ai];
    if (!article) return;
    a.meters.forEach((m, mi) => {
      const el = article.querySelector(`.reset[data-meter="${mi}"]`);
      if (!el) return;
      const left = remainingReset(m.reset_in_sec, snapshot.fetchedAt);
      el.textContent = "resets " + formatReset(left);
    });
  });
}

async function fetchStatus() {
  try {
    const res = await fetch("/api/status", { cache: "no-store" });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const data = await res.json();
    render(data, Date.now());
    schedulePoll();
  } catch (e) {
    snapshot = null;
    cardsEl.innerHTML = `<div class="err-page">Could not load status.<br/>${escapeHtml(e.message || e)}</div>`;
    liveEl.className = "live idle";
    liveEl.textContent = "offline";
    metaEl.textContent = "connection error";
    schedulePoll(5000);
  }
}

function schedulePoll(ms) {
  if (pollTimer) clearTimeout(pollTimer);
  const wait = ms != null ? ms : UI_POLL_MS;
  pollTimer = setTimeout(fetchStatus, wait);
}

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") fetchStatus();
});
tickTimer = setInterval(tickResets, 1000);
fetchStatus();
