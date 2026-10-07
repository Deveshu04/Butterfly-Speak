<script lang="ts">
  import { formatWindow } from "$lib/dictationData";
  import { stats } from "$lib/stats.svelte";
  import EmptyState from "$lib/components/EmptyState.svelte";

  const POSTCARD_WORDS = 50;
  const TYPING_WPM = 40; // average typing speed, for the gauge caption
  const GAUGE_MAX_WPM = 220; // top of the gauge sweep
  const HEAT_WEEKS = 18; // ~4 months of squares in the half-width card

  let postcards = $derived(Math.max(1, Math.round(stats.totalWords / POSTCARD_WORDS)));

  function dayKeyOf(d: Date): string {
    const m = `${d.getMonth() + 1}`.padStart(2, "0");
    const day = `${d.getDate()}`.padStart(2, "0");
    return `${d.getFullYear()}-${m}-${day}`;
  }

  /** Sum of words over the 7 days ending (today − endOffset days), inclusive. */
  function weekSum(days: Record<string, number>, endOffset: number): number {
    let sum = 0;
    const d = new Date();
    d.setDate(d.getDate() - endOffset);
    for (let i = 0; i < 7; i++) {
      sum += days[dayKeyOf(d)] ?? 0;
      d.setDate(d.getDate() - 1);
    }
    return sum;
  }

  let thisWeekWords = $derived(weekSum(stats.days, 0));
  let lastWeekWords = $derived(weekSum(stats.days, 7));

  let wordsTrend = $derived.by(() => {
    if (lastWeekWords <= 0) return null;
    const delta = thisWeekWords - lastWeekWords;
    return {
      up: delta > 0,
      arrow: delta >= 0 ? "↑" : "↓",
      pct: Math.round((Math.abs(delta) / lastWeekWords) * 100),
    };
  });

  let interp = $derived.by((): { headline: string; sub: string } | null => {
    if (lastWeekWords > 0 && thisWeekWords > lastWeekWords * 1.1) {
      const pct = Math.round(((thisWeekWords - lastWeekWords) / lastWeekWords) * 100);
      return {
        headline: "You're picking up speed.",
        sub: `You dictated ${pct}% more this week than last.`,
      };
    }
    if (stats.totalWords === 0) return null;
    if (stats.streak >= 3) {
      return {
        headline: "You're on a roll.",
        sub: `${stats.streak} days in a row of dictating.`,
      };
    }
    return {
      headline: "Building the habit.",
      sub: "Every dictation is one you didn't have to type.",
    };
  });

  /** Start hour (even, 0–22) of the most common 2-hour dictation window,
   * from counts kept per window (no dictation text is kept for this). */
  let mostActive = $derived(stats.mostActive);

  function fmtDay(key: string): string {
    const [y, m, d] = key.split("-").map(Number);
    return new Date(y, m - 1, d).toLocaleDateString([], {
      month: "short",
      day: "numeric",
    });
  }

  /* ---- WPM gauge (semicircle) ---- */
  const ARC_LEN = Math.PI * 82; // radius 82 semicircle
  let gaugePct = $derived(Math.min(stats.avgWpm / GAUGE_MAX_WPM, 1));

  /* ---- App usage ---- */
  let usage = $derived.by(() => {
    const entries = Object.entries(stats.apps).sort((a, b) => b[1] - a[1]);
    const total = entries.reduce((n, [, w]) => n + w, 0);
    if (total === 0) return null;
    const top = entries.slice(0, 5).map(([app, words]) => ({
      app,
      words,
      pct: Math.max(1, Math.round((words / total) * 100)),
    }));
    const otherWords = total - top.reduce((n, r) => n + r.words, 0);
    return { top, otherWords, total, count: entries.length };
  });

  /* ---- Contribution heatmap ---- */
  interface HeatDay {
    key: string;
    words: number;
    level: number;
    future: boolean;
  }

  function level(words: number): number {
    if (words <= 0) return 0;
    if (words < 50) return 1;
    if (words < 200) return 2;
    if (words < 500) return 3;
    return 4;
  }

  let heatWeeks = $derived.by(() => {
    const today = new Date();
    today.setHours(0, 0, 0, 0);
    const start = new Date(today);
    start.setDate(start.getDate() - start.getDay() - (HEAT_WEEKS - 1) * 7);
    const weeks: { month: string | null; days: HeatDay[] }[] = [];
    let prevMonth = -1;
    const d = new Date(start);
    for (let w = 0; w < HEAT_WEEKS; w++) {
      const days: HeatDay[] = [];
      let month: string | null = null;
      for (let i = 0; i < 7; i++) {
        const key = dayKeyOf(d);
        const words = stats.days[key] ?? 0;
        days.push({
          key,
          words,
          level: level(words),
          future: d.getTime() > today.getTime(),
        });
        if (i === 0 && d.getMonth() !== prevMonth) {
          month = d.toLocaleDateString([], { month: "short" });
          prevMonth = d.getMonth();
        }
        d.setDate(d.getDate() + 1);
      }
      weeks.push({ month, days });
    }
    return weeks;
  });

  const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
</script>

<div class="page">
  <h1 class="page-title">Insights</h1>
  <p class="page-desc">What your dictation looks like — speed, volume, and momentum.</p>

  <div class="stat-row">
    <div class="stat-card">
      <span class="stat-label">Words / minute</span>
      <span class="stat-num">{stats.avgWpm > 0 ? stats.avgWpm : "—"}</span>
      {#if stats.avgWpm > 0}
        <div class="gauge" role="img" aria-label="Speaking speed gauge">
          <svg viewBox="0 0 200 108">
            <path class="track" d="M 18 100 A 82 82 0 0 1 182 100" />
            <path
              class="fill"
              d="M 18 100 A 82 82 0 0 1 182 100"
              stroke-dasharray={ARC_LEN}
              stroke-dashoffset={ARC_LEN * (1 - gaugePct)}
            />
          </svg>
          <div class="gauge-caption">
            <span class="gauge-x">{(stats.avgWpm / TYPING_WPM).toFixed(1)}×</span>
            <span class="gauge-sub">typing speed</span>
          </div>
        </div>
      {:else}
        <span class="stat-context">Dictate a little to measure your speed.</span>
      {/if}
    </div>

    <div class="stat-card">
      <span class="stat-label">Fixes made by Butterfly</span>
      <span class="stat-num">
        {(stats.wordsCorrected + stats.dictFixes).toLocaleString()}
      </span>
      <div class="fixes-rows">
        <div class="fixes-row">
          <span class="fixes-num">{stats.wordsCorrected.toLocaleString()}</span>
          <span>words corrected</span>
        </div>
        <div class="fixes-row">
          <span class="fixes-num">{stats.dictFixes.toLocaleString()}</span>
          <span>dictionary fixes</span>
        </div>
      </div>
    </div>

    <div class="stat-card">
      <span class="stat-label">Total words</span>
      <span class="stat-num">{stats.totalWords.toLocaleString()}</span>
      <span class="stat-dictations">
        across {stats.totalDictations.toLocaleString()}
        {stats.totalDictations === 1 ? "dictation" : "dictations"}
      </span>
      {#if wordsTrend}
        <span class="stat-context" class:up={wordsTrend.up}>
          {wordsTrend.arrow} {wordsTrend.pct}% vs last week
        </span>
      {:else if stats.totalWords > 0}
        <span class="stat-context fun">
          You've written {postcards.toLocaleString()}
          {postcards === 1 ? "postcard" : "postcards"}!
        </span>
      {/if}
    </div>
  </div>

  {#if interp}
    <div class="interp-card">
      <p class="interp-headline">{interp.headline}</p>
      <p class="interp-sub">{interp.sub}</p>
    </div>
  {/if}

  {#if stats.totalWords === 0}
    <EmptyState
      icon="bars"
      title="No activity yet"
      body="Your usage and streaks will show up here once you start dictating."
    />
  {:else}
    <div class="lower">
      <div class="usage-card">
        <div class="usage-head">
          <span class="usage-title">App usage</span>
          {#if usage}
            <span class="usage-total">Total apps used | {usage.count}</span>
          {/if}
        </div>
        {#if usage}
          <div class="usage-rows">
            {#each usage.top as row (row.app)}
              <div class="usage-row" title="{row.words.toLocaleString()} words">
                <div class="usage-bar" style="width: {Math.max(row.pct, 12)}%">
                  <span>{row.pct}%</span>
                </div>
                <span class="usage-label">{row.app}</span>
              </div>
            {/each}
            {#if usage.otherWords > 0}
              <p class="usage-other">
                + {usage.otherWords.toLocaleString()} words in other apps
              </p>
            {/if}
          </div>
          {#if mostActive !== null}
            <p class="most-active">Most active: {formatWindow(mostActive)}</p>
          {/if}
        {:else}
          <p class="usage-empty">
            Which apps you dictate into will show up here — every dictation is
            counted toward the app it lands in.
          </p>
        {/if}
      </div>

      <div class="heat-card">
        <div class="heat-head">
          <span class="heat-title">
            {stats.streak}
            {stats.streak === 1 ? "day" : "days"} streak
          </span>
          <span class="heat-longest">
            Longest streak | {stats.longestStreak}
            {stats.longestStreak === 1 ? "day" : "days"}
          </span>
        </div>
        <div class="heat-body">
          <div class="weekday-col" aria-hidden="true">
            {#each WEEKDAYS as wd}
              <span>{wd}</span>
            {/each}
          </div>
          <div class="weeks">
            <div class="months" aria-hidden="true">
              {#each heatWeeks as w, i (i)}
                <span class="month-slot">{w.month ?? ""}</span>
              {/each}
            </div>
            <div class="grid" role="img" aria-label="Daily dictation activity heatmap">
              {#each heatWeeks as w, i (i)}
                <div class="week">
                  {#each w.days as day (day.key)}
                    <span
                      class="cell lv{day.level}"
                      class:future={day.future}
                      title="{fmtDay(day.key)} — {day.words.toLocaleString()} words"
                    ></span>
                  {/each}
                </div>
              {/each}
            </div>
          </div>
        </div>
        <div class="heat-legend">
          <span>More</span>
          <span class="cell lv4"></span>
          <span class="cell lv3"></span>
          <span class="cell lv2"></span>
          <span class="cell lv1"></span>
          <span>Less</span>
        </div>
      </div>
    </div>
  {/if}

  {#if stats.todayDictations > 0}
    <p class="today-line">
      Today: {stats.todayWords.toLocaleString()} words across {stats.todayDictations}
      {stats.todayDictations === 1 ? "dictation" : "dictations"}.
    </p>
  {/if}
</div>

<style>
  .stat-row {
    display: flex;
    gap: 14px;
    margin-bottom: 14px;
    align-items: stretch;
  }

  .stat-card {
    flex: 1 1 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 18px 20px 16px;
  }

  .stat-label {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--fg-faint);
  }

  .stat-num {
    font-size: 38px;
    font-weight: 650;
    letter-spacing: -0.02em;
    line-height: 1.15;
    margin-top: 8px;
    font-variant-numeric: tabular-nums;
  }

  .stat-context {
    margin-top: auto;
    padding-top: 10px;
    font-size: 12.5px;
    color: var(--fg-muted);
  }

  .stat-context.up,
  .stat-context.fun {
    color: var(--teal);
    font-weight: 550;
  }

  .stat-dictations {
    margin-top: 2px;
    font-size: 12px;
    color: var(--fg-faint);
  }

  /* Fixes breakdown: divider + labeled counts. */
  .fixes-rows {
    margin-top: 12px;
    padding-top: 12px;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: column;
    gap: 7px;
  }

  .fixes-row {
    display: flex;
    align-items: baseline;
    gap: 6px;
    font-size: 13px;
    color: var(--fg-muted);
  }

  .fixes-num {
    font-weight: 650;
    color: var(--fg);
    font-variant-numeric: tabular-nums;
  }

  /* ---- WPM gauge ---- */
  .gauge {
    position: relative;
    margin: 10px auto 0;
    width: 150px;
  }

  .gauge svg {
    width: 100%;
    display: block;
  }

  .gauge .track,
  .gauge .fill {
    fill: none;
    stroke-width: 15;
    stroke-linecap: round;
  }

  .gauge .track {
    stroke: var(--track);
  }

  .gauge .fill {
    stroke: var(--teal);
    transition: stroke-dashoffset 600ms ease;
  }

  .gauge-caption {
    position: absolute;
    left: 0;
    right: 0;
    bottom: 4px;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 1px;
  }

  .gauge-x {
    font-size: 19px;
    font-weight: 650;
    color: var(--teal);
    letter-spacing: -0.01em;
  }

  .gauge-sub {
    font-size: 11px;
    color: var(--fg-muted);
  }

  .interp-card {
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 18px 22px;
    margin-bottom: 14px;
  }

  .interp-headline {
    margin: 0 0 3px;
    font-size: 17px;
    font-weight: 650;
    letter-spacing: -0.01em;
  }

  .interp-sub {
    margin: 0;
    font-size: 13px;
    line-height: 1.5;
    color: var(--fg-muted);
  }

  /* ---- Bottom row: usage (left) + streak heatmap (right) ---- */
  .lower {
    display: flex;
    gap: 14px;
    align-items: stretch;
  }

  .usage-card {
    flex: 1.1 1 0;
    min-width: 0;
    display: flex;
    flex-direction: column;
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 20px 22px 16px;
  }

  .usage-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 18px;
  }

  .usage-title {
    font-size: 22px;
    font-weight: 650;
    letter-spacing: -0.015em;
  }

  .usage-total {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--fg-muted);
    white-space: nowrap;
  }

  .usage-rows {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .usage-row {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .usage-bar {
    flex: none;
    display: flex;
    align-items: center;
    justify-content: center;
    height: 28px;
    min-width: 48px;
    max-width: 70%;
    background: var(--teal);
    border-radius: 7px;
    transition: width var(--motion);
  }

  .usage-bar span {
    font-size: 12px;
    font-weight: 650;
    color: var(--teal-fg);
    padding: 0 8px;
  }

  .usage-label {
    font-size: 12px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--fg-muted);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .usage-other {
    margin: 2px 0 0;
    font-size: 12px;
    color: var(--fg-faint);
  }

  .usage-empty {
    margin: 0;
    font-size: 13px;
    line-height: 1.55;
    color: var(--fg-muted);
    max-width: 36ch;
  }

  .most-active {
    margin: auto 0 0;
    padding-top: 14px;
    font-size: 12px;
    color: var(--fg-faint);
  }

  /* ---- Streak heatmap ---- */
  .heat-card {
    flex: 1 1 0;
    min-width: 0;
    background: var(--bg-elevated);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-card);
    padding: 20px 22px 16px;
  }

  .heat-head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 16px;
  }

  .heat-title {
    font-size: 22px;
    font-weight: 650;
    letter-spacing: -0.015em;
    white-space: nowrap;
  }

  .heat-longest {
    font-size: 11px;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--fg-muted);
    white-space: nowrap;
  }

  .heat-body {
    display: flex;
    gap: 8px;
    overflow-x: auto;
    padding-bottom: 2px;
  }

  .weekday-col {
    flex: none;
    display: flex;
    flex-direction: column;
    gap: 4px;
    /* Aligns with the grid below the months row (14px + 6px margin). */
    padding-top: 20px;
  }

  .weekday-col span {
    height: 14px;
    line-height: 14px;
    font-size: 10px;
    color: var(--fg-faint);
    padding-right: 4px;
  }

  .weeks {
    min-width: 0;
  }

  .months {
    display: flex;
    gap: 4px;
    height: 14px;
    margin-bottom: 6px;
  }

  .month-slot {
    flex: none;
    width: 14px;
    font-size: 10px;
    color: var(--fg-faint);
    overflow: visible;
    white-space: nowrap;
  }

  .grid {
    display: flex;
    gap: 4px;
  }

  .week {
    flex: none;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .cell {
    width: 14px;
    height: 14px;
    border-radius: 3.5px;
    background: var(--heat-0);
  }

  .cell.lv1 {
    background: var(--heat-1);
  }

  .cell.lv2 {
    background: var(--heat-2);
  }

  .cell.lv3 {
    background: var(--heat-3);
  }

  .cell.lv4 {
    background: var(--teal);
  }

  .cell.future {
    visibility: hidden;
  }

  .heat-legend {
    display: flex;
    align-items: center;
    gap: 5px;
    margin-top: 12px;
    font-size: 11px;
    color: var(--fg-muted);
  }

  .today-line {
    margin: 24px 2px 0;
    font-size: 12.5px;
    color: var(--fg-faint);
  }
</style>
