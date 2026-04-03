"use client";

/**
 * Live Task Board with Pagination & Virtual Scrolling
 *
 * Three sections:
 *  1. Research Tasks — polls `list_research_tasks` and shows per-phase progress
 *  2. Activity Feed   — polls `read_tasks_paginated` with pagination & virtual scroll
 *  3. System Logs     — displays structured logs with filtering in a separate tab
 *
 * Poll interval adapts: 2 s when research tasks are running, else 5 s.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FixedSizeList as List } from "react-window";
import {
  Activity,
  AlertCircle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Clock,
  FileText,
  Loader2,
  LogsIcon,
  Microscope,
  Moon,
  RefreshCw,
  Sun,
  Zap,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { LogsViewer } from "@/components/logs-viewer";
import { TelegramAuthCard } from "@/components/telegram-auth-card";

// ── Types ─────────────────────────────────────────────────────────────────────

interface TaskEntry {
  timestamp: string;
  event: string;
  persona: string;
  message_preview?: string;
  trigger_remote_ai?: boolean;
  estimated_profit_usd?: number;
  status?: string;
}

interface PaginatedTasksResponse {
  items: TaskEntry[];
  total: number;
  offset: number;
  limit: number;
  has_more: boolean;
}

interface ResearchStep {
  phase: string;
  output: string;
}

interface ResearchTask {
  id: string;
  topic: string;
  url?: string;
  /** snake_case from Rust serde */
  status: "running" | "done" | "failed";
  steps: ResearchStep[];
  final_report?: string;
  started_at: string;
  finished_at?: string;
}

// ── Research phase ordering ───────────────────────────────────────────────────

const PIPELINE_PHASES: { key: string; label: string }[] = [
  { key: "fetch",       label: "擷取頁面" },
  { key: "overview",    label: "概覽分析" },
  { key: "questions",   label: "生成問題" },
  { key: "research_q1", label: "Q1 調研" },
  { key: "research_q2", label: "Q2 調研" },
  { key: "research_q3", label: "Q3 調研" },
  { key: "research_q4", label: "Q4 調研" },
  { key: "synthesis",   label: "合成報告" },
];

// ── Helpers ───────────────────────────────────────────────────────────────────

function formatTs(ts: string): string {
  try {
    return new Intl.DateTimeFormat(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    }).format(new Date(ts));
  } catch {
    return ts;
  }
}

function elapsed(start: string, end?: string): string {
  try {
    const ms = (end ? new Date(end) : new Date()).getTime() - new Date(start).getTime();
    if (ms < 60_000) return `${Math.round(ms / 1000)}s`;
    return `${Math.round(ms / 60_000)}m`;
  } catch {
    return "";
  }
}

type StatusVariant = "pending" | "following" | "followup" | "done" | "running" | "failed" | "default";

function taskStatusVariant(status?: string): StatusVariant {
  switch (status) {
    case "PENDING":        return "pending";
    case "FOLLOWING":      return "following";
    case "FOLLOWUP_NEEDED": return "followup";
    case "DONE":           return "done";
    default:               return "default";
  }
}

function taskStatusLabel(status?: string): string {
  switch (status) {
    case "PENDING":        return "待處理";
    case "FOLLOWING":      return "跟進中";
    case "FOLLOWUP_NEEDED": return "需要跟進";
    case "DONE":           return "已完成";
    default:               return status ?? "—";
  }
}

function eventLabel(event: string): string {
  if (event === "ai_decision")    return "AI 決策";
  if (event === "user_request")   return "使用者請求";
  if (event.startsWith("skill_executed:"))
    return `已執行：${event.replace("skill_executed:", "")}`;
  return event;
}

function isActionable(status?: string) {
  return status === "PENDING" || status === "FOLLOWING" || status === "FOLLOWUP_NEEDED";
}

function StatusIcon({ status }: { status?: string }) {
  const cls = "h-4 w-4 shrink-0";
  switch (status) {
    case "PENDING":        return <Clock className={`${cls} text-yellow-500`} />;
    case "FOLLOWING":      return <Activity className={`${cls} text-blue-500`} />;
    case "FOLLOWUP_NEEDED": return <AlertCircle className={`${cls} text-red-500`} />;
    case "DONE":           return <CheckCircle2 className={`${cls} text-green-500`} />;
    default:               return null;
  }
}

// ── Research: phase timeline ──────────────────────────────────────────────────

function PhaseTimeline({ task }: { task: ResearchTask }) {
  const completedKeys = new Set(task.steps.map((s) => s.phase));
  const isRunning = task.status === "running";

  // Determine which phases actually matter (skip fetch if no URL)
  const phases = task.url
    ? PIPELINE_PHASES
    : PIPELINE_PHASES.filter((p) => p.key !== "fetch");

  // The "current" phase is the first not yet completed (only meaningful when running)
  const currentIdx = phases.findIndex((p) => !completedKeys.has(p.key));

  return (
    <div className="flex flex-wrap gap-1.5 mt-3">
      {phases.map((phase, idx) => {
        const done = completedKeys.has(phase.key);
        const active = isRunning && idx === currentIdx;
        return (
          <span
            key={phase.key}
            className={[
              "inline-flex items-center gap-1 rounded-full px-2.5 py-0.5 text-[11px] font-medium transition-colors",
              done
                ? "bg-emerald-100 text-emerald-700 dark:bg-emerald-500/15 dark:text-emerald-300"
                : active
                ? "bg-violet-100 text-violet-700 dark:bg-violet-500/20 dark:text-violet-300 ring-1 ring-violet-400/40"
                : "bg-slate-100 text-slate-400 dark:bg-slate-800 dark:text-slate-500",
            ].join(" ")}
          >
            {done && <CheckCircle2 className="h-2.5 w-2.5" />}
            {active && <Loader2 className="h-2.5 w-2.5 animate-spin" />}
            {phase.label}
          </span>
        );
      })}
    </div>
  );
}

// ── Research card ─────────────────────────────────────────────────────────────

function ResearchCard({ task }: { task: ResearchTask }) {
  const [expanded, setExpanded] = useState(task.status === "running");

  const statusBadge: StatusVariant =
    task.status === "running" ? "running" :
    task.status === "done"    ? "done" :
    "failed";

  const statusText =
    task.status === "running" ? "調研中" :
    task.status === "done"    ? "已完成" :
    "失敗";

  return (
    <Card className={[
      "transition-shadow",
      task.status === "running"
        ? "ring-1 ring-violet-400/30 shadow-md dark:shadow-violet-950/30"
        : "hover:shadow-md dark:hover:shadow-slate-950/40",
    ].join(" ")}>
      <CardHeader className="flex-row items-start justify-between gap-3 space-y-0 pb-2">
        <div className="flex items-center gap-2 min-w-0">
          {task.status === "running"
            ? <Loader2 className="h-4 w-4 shrink-0 text-violet-500 animate-spin" />
            : task.status === "done"
            ? <Microscope className="h-4 w-4 shrink-0 text-emerald-500" />
            : <AlertCircle className="h-4 w-4 shrink-0 text-red-500" />}
          <CardTitle className="text-sm font-semibold truncate">{task.topic}</CardTitle>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <Badge variant={statusBadge}>{statusText}</Badge>
          <span className="text-[11px] text-slate-400 dark:text-slate-500 tabular-nums">
            {elapsed(task.started_at, task.finished_at)}
          </span>
        </div>
      </CardHeader>

      <CardContent>
        {task.url && (
          <p className="text-xs text-slate-400 dark:text-slate-500 truncate mb-1">{task.url}</p>
        )}

        <PhaseTimeline task={task} />

        {/* Latest step output preview */}
        {task.steps.length > 0 && task.status === "running" && (
          <div className="mt-2 rounded-md bg-slate-50 dark:bg-slate-900/60 border border-slate-200 dark:border-slate-800 px-3 py-2 text-xs text-slate-600 dark:text-slate-300 line-clamp-2">
            <span className="text-slate-400 dark:text-slate-500 font-medium mr-1">
              {task.steps[task.steps.length - 1].phase}:
            </span>
            {task.steps[task.steps.length - 1].output}
          </div>
        )}

        {/* Final report (expandable) */}
        {task.final_report && (
          <div className="mt-3">
            <button
              onClick={() => setExpanded((v) => !v)}
              className="flex items-center gap-1 text-xs font-medium text-slate-500 dark:text-slate-400 hover:text-slate-700 dark:hover:text-slate-200 transition-colors"
            >
              <FileText className="h-3.5 w-3.5" />
              查看完整報告
              {expanded
                ? <ChevronDown className="h-3 w-3" />
                : <ChevronRight className="h-3 w-3" />}
            </button>

            {expanded && (
              <div className="mt-2 max-h-72 overflow-y-auto rounded-lg border border-slate-200 dark:border-slate-700 bg-slate-50 dark:bg-slate-900/60 px-4 py-3 text-xs leading-6 text-slate-700 dark:text-slate-200 whitespace-pre-wrap">
                {task.final_report}
              </div>
            )}
          </div>
        )}

        {/* Started time */}
        <p className="mt-2 text-[10px] text-slate-300 dark:text-slate-600">
          開始 {formatTs(task.started_at)}
          {task.finished_at && ` · 完成 ${formatTs(task.finished_at)}`}
        </p>
      </CardContent>
    </Card>
  );
}

// ── Activity task card ────────────────────────────────────────────────────────

interface TaskCardProps {
  entry: TaskEntry;
  onApprove: (timestamp: string) => Promise<void>;
  approving: boolean;
}

function TaskCard({ entry, onApprove, approving }: TaskCardProps) {
  const variant = taskStatusVariant(entry.status);

  return (
    <Card className="transition-shadow hover:shadow-md dark:hover:shadow-slate-950/40">
      <CardHeader className="flex-row items-start justify-between gap-3 space-y-0">
        <div className="flex items-center gap-2 min-w-0">
          <StatusIcon status={entry.status} />
          <CardTitle className="truncate text-sm font-semibold">
            {eventLabel(entry.event)}
          </CardTitle>
        </div>
        <Badge variant={variant}>{taskStatusLabel(entry.status)}</Badge>
      </CardHeader>

      <CardContent>
        {entry.message_preview && (
          <div className="mb-3 rounded-lg border border-slate-200/80 bg-slate-50 px-3 py-2 text-sm leading-6 text-slate-700 dark:border-slate-800 dark:bg-slate-900/70 dark:text-slate-200">
            {entry.message_preview}
          </div>
        )}

        <div className="flex items-center justify-between text-[11px] text-slate-400 dark:text-slate-500">
          <span>{entry.persona}</span>
          <span className="tabular-nums">{formatTs(entry.timestamp)}</span>
        </div>

        {isActionable(entry.status) && (
          <div className="mt-3 flex justify-end">
            <Button
              size="sm"
              onClick={() => onApprove(entry.timestamp)}
              disabled={approving}
              className="gap-1.5"
            >
              {approving
                ? <Loader2 className="h-3.5 w-3.5 animate-spin" />
                : <CheckCircle2 className="h-3.5 w-3.5" />}
              快速核准
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

// ── Live status bar ───────────────────────────────────────────────────────────

function StatusBar({ activeResearch, taskCount }: { activeResearch: number; taskCount: number }) {
  return (
    <div className="flex items-center gap-3 text-xs text-slate-500 dark:text-slate-400">
      <span className="flex items-center gap-1.5">
        <span className="relative flex h-2 w-2">
          <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-emerald-400 opacity-75" />
          <span className="relative inline-flex rounded-full h-2 w-2 bg-emerald-500" />
        </span>
        在線
      </span>
      {activeResearch > 0 && (
        <span className="flex items-center gap-1 text-violet-500 dark:text-violet-400 font-medium">
          <Loader2 className="h-3 w-3 animate-spin" />
          {activeResearch} 個調研任務進行中
        </span>
      )}
      {taskCount > 0 && (
        <span className="flex items-center gap-1">
          <Zap className="h-3 w-3 text-amber-400" />
          {taskCount} 待處理
        </span>
      )}
    </div>
  );
}

// ── Main board ────────────────────────────────────────────────────────────────

const FAST_POLL_MS = 2000;
const SLOW_POLL_MS = 5000;
const PAGE_SIZE = 25;

export function TaskBoard() {
  const [tasks, setTasks]               = useState<TaskEntry[]>([]);
  const [totalTasks, setTotalTasks]     = useState(0);
  const [taskOffset, setTaskOffset]     = useState(0);
  
  const [research, setResearch]         = useState<ResearchTask[]>([]);
  const [loading, setLoading]           = useState(true);
  const [error, setError]               = useState<string | null>(null);
  const [approvingId, setApprovingId]   = useState<string | null>(null);
  const [lastRefresh, setLastRefresh]   = useState<Date | null>(null);
  const [theme, setTheme]               = useState<"light" | "dark">("light");
  const [showAllResearch, setShowAllResearch] = useState(false);
  const [activeTab, setActiveTab]       = useState<"board" | "logs">("board");
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  // ── Theme ──────────────────────────────────────────────────────────────────
  useEffect(() => {
    const saved = typeof window !== "undefined" ? localStorage.getItem("sirin-theme") : null;
    const resolved =
      saved === "dark" || saved === "light"
        ? saved
        : window.matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light";
    document.documentElement.classList.toggle("dark", resolved === "dark");
    setTheme(resolved);
  }, []);

  const toggleTheme = useCallback(() => {
    const next = theme === "dark" ? "light" : "dark";
    document.documentElement.classList.toggle("dark", next === "dark");
    localStorage.setItem("sirin-theme", next);
    setTheme(next);
  }, [theme]);

  // ── Data fetching ──────────────────────────────────────────────────────────
  const fetchAll = useCallback(async () => {
    try {
      const [tasksResp, researchList] = await Promise.all([
        invoke<PaginatedTasksResponse>("read_tasks_paginated", {
          offset: taskOffset,
          limit: PAGE_SIZE,
        }),
        invoke<ResearchTask[]>("list_research_tasks").catch(() => [] as ResearchTask[]),
      ]);
      
      setTasks(tasksResp.items.reverse());
      setTotalTasks(tasksResp.total);
      setResearch(researchList);
      setLastRefresh(new Date());
      setError(null);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [taskOffset]);

  // Adaptive polling: faster when research tasks are running
  useEffect(() => {
    const hasRunning = research.some((r) => r.status === "running");
    const interval = hasRunning ? FAST_POLL_MS : SLOW_POLL_MS;

    if (intervalRef.current) clearInterval(intervalRef.current);
    intervalRef.current = setInterval(fetchAll, interval);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [fetchAll, research]);

  // Initial load
  useEffect(() => {
    fetchAll();
  }, [fetchAll]);

  const handleApprove = useCallback(
    async (timestamp: string) => {
      setApprovingId(timestamp);
      try {
        await invoke("approve_task", { timestamp, skill: "send_tg_reply" });
        await fetchAll();
      } catch (err) {
        setError(String(err));
      } finally {
        setApprovingId(null);
      }
    },
    [fetchAll]
  );

  // ── Derived data ───────────────────────────────────────────────────────────
  const actionable       = tasks.filter((t) => isActionable(t.status));
  const activityFeed     = tasks.filter((t) => !isActionable(t.status));
  const activeResearch   = research.filter((r) => r.status === "running");
  const completedResearch = research.filter((r) => r.status !== "running");
  const visibleCompleted = showAllResearch ? completedResearch : completedResearch.slice(0, 3);

  return (
    <div className="max-w-4xl mx-auto space-y-6">

      {/* ── Header with tabs ───────────────────────────────────────────────── */}
      <div className="space-y-4">
        <div className="flex items-center justify-between">
          <div>
            <h1 className="text-2xl font-bold tracking-tight flex items-center gap-2">
              Sirin
              <span className="text-base font-normal text-slate-400 dark:text-slate-500">任務板</span>
            </h1>
            <StatusBar
              activeResearch={research.filter((r) => r.status === "running").length}
              taskCount={tasks.filter((t) => isActionable(t.status)).length}
            />
          </div>
          <div className="flex items-center gap-2">
            {lastRefresh && (
              <span className="text-[11px] text-slate-400 dark:text-slate-500 tabular-nums hidden sm:block">
                {formatTs(lastRefresh.toISOString())} 更新
              </span>
            )}
            <Button variant="outline" size="sm" onClick={toggleTheme} className="gap-1.5">
              {theme === "dark" ? <Sun className="h-3.5 w-3.5" /> : <Moon className="h-3.5 w-3.5" />}
            </Button>
            <Button
              variant="outline"
              size="sm"
              onClick={fetchAll}
              disabled={loading}
              className="gap-1.5"
            >
              <RefreshCw className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`} />
              重新整理
            </Button>
          </div>
        </div>

        {/* ── Tab buttons ────────────────────────────────────────────────── */}
        <div className="flex items-center gap-2 border-b border-slate-200 dark:border-slate-800">
          <button
            onClick={() => setActiveTab("board")}
            className={[
              "px-4 py-2 text-sm font-medium transition-colors border-b-2 -mb-px",
              activeTab === "board"
                ? "text-violet-600 dark:text-violet-400 border-violet-600 dark:border-violet-400"
                : "text-slate-600 dark:text-slate-400 border-transparent hover:text-slate-900 dark:hover:text-slate-200",
            ].join(" ")}
          >
            <Activity className="h-4 w-4 inline-block mr-1" />
            任務看板
          </button>
          <button
            onClick={() => setActiveTab("logs")}
            className={[
              "px-4 py-2 text-sm font-medium transition-colors border-b-2 -mb-px",
              activeTab === "logs"
                ? "text-violet-600 dark:text-violet-400 border-violet-600 dark:border-violet-400"
                : "text-slate-600 dark:text-slate-400 border-transparent hover:text-slate-900 dark:hover:text-slate-200",
            ].join(" ")}
          >
            <LogsIcon className="h-4 w-4 inline-block mr-1" />
            系統日誌
          </button>
        </div>
      </div>

      {/* ── Error banner ───────────────────────────────────────────────────── */}
      {error && (
        <div className="rounded-lg border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700 dark:border-red-500/40 dark:bg-red-500/10 dark:text-red-300">
          {error}
        </div>
      )}

      {/* ── Telegram auth/status card (hidden when connected) ──────────── */}
      <TelegramAuthCard />

      {/* ── Loading skeleton ────────────────────────────────────────────────── */}
      {loading && tasks.length === 0 && activeTab === "board" && (
        <div className="space-y-3">
          {[1, 2, 3].map((n) => (
            <div
              key={n}
              className="h-24 rounded-xl border border-slate-200 bg-white animate-pulse dark:border-slate-800 dark:bg-slate-900"
            />
          ))}
        </div>
      )}

      {/* ── Board Tab Content ───────────────────────────────────────────────── */}
      {activeTab === "board" && (
        <div className="space-y-6">
          {/* ── Running research tasks ────────────────────────────────────── */}
          {activeResearch.length > 0 && (
            <section className="space-y-3">
              <h2 className="text-xs font-semibold uppercase tracking-widest text-violet-500 dark:text-violet-400 flex items-center gap-1.5">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                正在調研 ({activeResearch.length})
              </h2>
              {activeResearch.map((r) => (
                <ResearchCard key={r.id} task={r} />
              ))}
            </section>
          )}

          {/* ── Actionable tasks ──────────────────────────────────────────── */}
          {actionable.length > 0 && (
            <section className="space-y-3">
              <h2 className="text-xs font-semibold uppercase tracking-widest text-amber-500 dark:text-amber-400 flex items-center gap-1.5">
                <Zap className="h-3.5 w-3.5" />
                待處理 ({actionable.length})
              </h2>
              {actionable.map((t) => (
                <TaskCard
                  key={t.timestamp}
                  entry={t}
                  onApprove={handleApprove}
                  approving={approvingId === t.timestamp}
                />
              ))}
            </section>
          )}

          {/* ── Completed research tasks ──────────────────────────────────── */}
          {completedResearch.length > 0 && (
            <section className="space-y-3">
              <h2 className="text-xs font-semibold uppercase tracking-widest text-slate-400 dark:text-slate-500 flex items-center gap-1.5">
                <Microscope className="h-3.5 w-3.5" />
                調研紀錄 ({completedResearch.length})
              </h2>
              {visibleCompleted.map((r) => (
                <ResearchCard key={r.id} task={r} />
              ))}
              {completedResearch.length > 3 && (
                <button
                  onClick={() => setShowAllResearch((v) => !v)}
                  className="w-full text-center text-xs text-slate-400 hover:text-slate-600 dark:text-slate-500 dark:hover:text-slate-300 py-1 transition-colors"
                >
                  {showAllResearch
                    ? "收起"
                    : `顯示全部 ${completedResearch.length} 筆紀錄`}
                </button>
              )}
            </section>
          )}

          {/* ── Activity feed with pagination ─────────────────────────────── */}
          {activityFeed.length > 0 && (
            <section className="space-y-3">
              <div className="flex items-center justify-between">
                <h2 className="text-xs font-semibold uppercase tracking-widest text-slate-400 dark:text-slate-500 flex items-center gap-1.5">
                  <Activity className="h-3.5 w-3.5" />
                  最近活動 ({totalTasks})
                </h2>
                <div className="text-xs text-slate-500 dark:text-slate-400">
                  第 {Math.floor(taskOffset / PAGE_SIZE) + 1} 頁，共 {Math.ceil(totalTasks / PAGE_SIZE)} 頁
                </div>
              </div>

              {activityFeed.map((t) => (
                <TaskCard
                  key={t.timestamp}
                  entry={t}
                  onApprove={handleApprove}
                  approving={approvingId === t.timestamp}
                />
              ))}

              {/* Pagination controls */}
              {totalTasks > PAGE_SIZE && (
                <div className="flex items-center justify-between pt-2 border-t border-slate-200 dark:border-slate-800">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setTaskOffset(Math.max(0, taskOffset - PAGE_SIZE))}
                    disabled={taskOffset === 0}
                    className="gap-1"
                  >
                    <ChevronRight className="h-3.5 w-3.5 rotate-180" />
                    上一頁
                  </Button>

                  <span className="text-xs text-slate-600 dark:text-slate-400">
                    {taskOffset + 1}–{Math.min(taskOffset + PAGE_SIZE, totalTasks)} / {totalTasks}
                  </span>

                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setTaskOffset(taskOffset + PAGE_SIZE)}
                    disabled={taskOffset + PAGE_SIZE >= totalTasks}
                    className="gap-1"
                  >
                    下一頁
                    <ChevronRight className="h-3.5 w-3.5" />
                  </Button>
                </div>
              )}
            </section>
          )}

          {/* ── Empty state ────────────────────────────────────────────────── */}
          {!loading && tasks.length === 0 && research.length === 0 && (
            <div className="flex flex-col items-center justify-center rounded-xl border border-dashed border-slate-300 py-16 text-slate-400 dark:border-slate-700 dark:text-slate-500">
              <Activity className="h-8 w-8 mb-3 opacity-40" />
              <p className="text-sm font-medium">目前沒有任務</p>
              <p className="text-xs mt-1 text-center">
                Sirin 處理 Telegram 訊息後會顯示在這裡。<br />
                傳送「調研 &lt;網址&gt;」啟動背景調研任務。
              </p>
            </div>
          )}
        </div>
      )}

      {/* ── Logs Tab Content ───────────────────────────────────────────────── */}
      {activeTab === "logs" && <LogsViewer />}
    </div>
  );
}
