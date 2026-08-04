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

function render(data, fetchedAt) {
  intervalSecs = data.refresh_interval_secs || 60;
  const refreshing = !!data.is_refreshing;
  liveEl.className = "live " + (refreshing ? "sync" : data.last_refresh ? "" : "idle");
  liveEl.textContent = refreshing ? "sync" : data.last_refresh ? "live" : "idle";
  metaEl.textContent = `last ${fmtTime(data.last_refresh)} · poll ${intervalSecs}s`;

  const accounts = data.accounts || [];
  snapshot = { fetchedAt: fetchedAt || Date.now(), accounts };

  if (!accounts.length) {
    cardsEl.innerHTML = `<div class="empty">No accounts configured.<br/>Add accounts with <code>tokenbar login</code>.</div>`;
    return;
  }

  paintCards();
}

function paintCards() {
  if (!snapshot) return;
  const { fetchedAt, accounts } = snapshot;

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
    } else if (status === "error") {
      body = `<div class="note bad">${escapeHtml(a.error || "Error")}</div>`;
    }

    if (status === "stale" && a.error) {
      body += `<div class="note">stale · ${escapeHtml(a.error)}</div>`;
    }

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
    </article>`;
  }).join("");
}

cardsEl.addEventListener("click", (e) => {
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
