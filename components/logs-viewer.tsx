"use client";

/**
 * Logs Viewer
 *
 * Displays system logs in real-time with pagination and filtering.
 * Supports virtual scrolling for large log volumes.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { FixedSizeList as List } from "react-window";
import {
  AlertCircle,
  ChevronLeft,
  ChevronRight,
  LogsIcon,
  Search,
  Trash2,
  Filter,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

interface LogEntry {
  timestamp: string;
  level: string;
  target: string;
  message: string;
  context?: string;
}

interface LogsResponse {
  items: LogEntry[];
  total: number;
  offset: number;
  limit: number;
  has_more: boolean;
}

interface AutonomousMetrics {
  generated_at: string;
  pending_tasks: number;
  followup_needed_tasks: number;
  running_research: number;
  scheduled_last_hour: number;
  completed_success_last_hour: number;
  completed_failed_last_hour: number;
  success_rate_last_hour: number;
  max_concurrent: number;
  max_per_cycle: number;
  cooldown_secs: number;
  max_retries: number;
}

const LOG_LEVELS = ["DEBUG", "INFO", "WARN", "ERROR"];

function getLevelColor(level: string): string {
  switch (level) {
    case "ERROR":
      return "text-red-600 dark:text-red-400";
    case "WARN":
      return "text-amber-600 dark:text-amber-400";
    case "INFO":
      return "text-blue-600 dark:text-blue-400";
    case "DEBUG":
      return "text-slate-600 dark:text-slate-400";
    default:
      return "text-slate-600 dark:text-slate-400";
  }
}

function getLevelBgColor(level: string): string {
  switch (level) {
    case "ERROR":
      return "bg-red-100 dark:bg-red-500/10";
    case "WARN":
      return "bg-amber-100 dark:bg-amber-500/10";
    case "INFO":
      return "bg-blue-100 dark:bg-blue-500/10";
    case "DEBUG":
      return "bg-slate-100 dark:bg-slate-500/10";
    default:
      return "bg-slate-100 dark:bg-slate-500/10";
  }
}

function formatTimestamp(ts: string): string {
  try {
    return new Intl.DateTimeFormat(undefined, {
      timeStyle: "medium",
      hour12: false,
    }).format(new Date(ts));
  } catch {
    return ts;
  }
}

// Virtual list row component
function LogRow({
  index,
  style,
  data,
}: {
  index: number;
  style: React.CSSProperties;
  data: LogEntry[];
}) {
  const entry = data[index];

  return (
    <div
      style={style}
      className="px-4 py-2 border-b border-slate-200 dark:border-slate-800 text-xs"
    >
      <div className="flex items-start gap-2">
        <span className={`font-mono font-semibold ${getLevelColor(entry.level)}`}>
          {entry.level}
        </span>
        <span className="text-slate-400 dark:text-slate-500 min-w-[12ch]">
          {formatTimestamp(entry.timestamp)}
        </span>
        <span className="text-slate-500 dark:text-slate-400 flex-1">{entry.target}</span>
        <span className="text-slate-600 dark:text-slate-300 flex-1 line-clamp-2">
          {entry.message}
        </span>
      </div>
      {entry.context && (
        <div className="mt-1 ml-24 text-slate-500 dark:text-slate-400 line-clamp-1">
          Context: {entry.context}
        </div>
      )}
    </div>
  );
}

export function LogsViewer() {
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [metrics, setMetrics] = useState<AutonomousMetrics | null>(null);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [limit, setLimit] = useState(50);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  
  const [filterTarget, setFilterTarget] = useState("");
  const [filterLevel, setFilterLevel] = useState("");
  const [searchQuery, setSearchQuery] = useState("");
  
  const listRef = useRef<List>(null);

  const fetchLogs = useCallback(async () => {
    setLoading(true);
    try {
      const response = await invoke<LogsResponse>("get_logs", {
        limit,
        offset,
        target: filterTarget || null,
        level: filterLevel || null,
      });
      
      setLogs(response.items);
      setTotal(response.total);
      setError(null);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, [offset, limit, filterTarget, filterLevel]);

  const fetchMetrics = useCallback(async () => {
    try {
      const data = await invoke<AutonomousMetrics>("read_autonomous_metrics");
      setMetrics(data);
    } catch {
      // keep UI usable even if metrics are temporarily unavailable
    }
  }, []);

  useEffect(() => {
    fetchLogs();
    fetchMetrics();
  }, [fetchLogs, fetchMetrics]);

  useEffect(() => {
    const t = setInterval(fetchMetrics, 5000);
    return () => clearInterval(t);
  }, [fetchMetrics]);

  // Filter logs by search query
  const filteredLogs = useMemo(() => {
    if (!searchQuery) return logs;
    return logs.filter(
      (log) =>
        log.message.toLowerCase().includes(searchQuery.toLowerCase()) ||
        log.target.toLowerCase().includes(searchQuery.toLowerCase())
    );
  }, [logs, searchQuery]);

  const currentPage = Math.floor(offset / limit) + 1;
  const totalPages = Math.ceil(total / limit);

  const canPrevious = offset > 0;
  const canNext = offset + limit < total;

  return (
    <div className="max-w-6xl mx-auto space-y-4">
      {/* ── Header with title ─────────────────────────────────────────────── */}
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold flex items-center gap-2">
          <LogsIcon className="h-5 w-5" />
          系統日誌
        </h2>
        <div className="text-xs text-slate-500 dark:text-slate-400">
          {total} 筆紀錄 · 第 {currentPage} / {totalPages} 頁
        </div>
      </div>

      {/* ── Autonomous metrics ────────────────────────────────────────────── */}
      {metrics && (
        <Card className="border-slate-200 dark:border-slate-800">
          <CardHeader className="pb-2">
            <CardTitle className="text-sm">自主任務監控</CardTitle>
          </CardHeader>
          <CardContent className="grid grid-cols-2 md:grid-cols-4 gap-3 text-xs">
            <div className="rounded-md bg-slate-50 dark:bg-slate-900/50 p-2">
              <div className="text-slate-400">進行中研究</div>
              <div className="text-lg font-semibold">{metrics.running_research}</div>
            </div>
            <div className="rounded-md bg-slate-50 dark:bg-slate-900/50 p-2">
              <div className="text-slate-400">待處理</div>
              <div className="text-lg font-semibold">{metrics.pending_tasks}</div>
            </div>
            <div className="rounded-md bg-slate-50 dark:bg-slate-900/50 p-2">
              <div className="text-slate-400">需跟進</div>
              <div className="text-lg font-semibold">{metrics.followup_needed_tasks}</div>
            </div>
            <div className="rounded-md bg-slate-50 dark:bg-slate-900/50 p-2">
              <div className="text-slate-400">近一小時成功率</div>
              <div className="text-lg font-semibold">
                {(metrics.success_rate_last_hour * 100).toFixed(0)}%
              </div>
            </div>
            <div className="rounded-md bg-slate-50 dark:bg-slate-900/50 p-2 col-span-2 md:col-span-4">
              <div className="text-slate-400 mb-1">策略參數</div>
              <div className="flex flex-wrap gap-2">
                <Badge variant="default">max_concurrent={metrics.max_concurrent}</Badge>
                <Badge variant="default">max_per_cycle={metrics.max_per_cycle}</Badge>
                <Badge variant="default">cooldown={metrics.cooldown_secs}s</Badge>
                <Badge variant="default">max_retries={metrics.max_retries}</Badge>
                <Badge variant="default">近一小時派工={metrics.scheduled_last_hour}</Badge>
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      {/* ── Filters ───────────────────────────────────────────────────────── */}
      <Card className="bg-slate-50 dark:bg-slate-900/30 border-slate-200 dark:border-slate-800">
        <CardContent className="pt-4 space-y-3">
          {/* Search bar */}
          <div className="flex items-center gap-2 bg-white dark:bg-slate-900 rounded-lg border border-slate-200 dark:border-slate-800 px-3 py-2">
            <Search className="h-4 w-4 text-slate-400" />
            <input
              type="text"
              placeholder="搜尋日誌訊息..."
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              className="flex-1 bg-transparent text-sm outline-none"
            />
          </div>

          {/* Filter buttons */}
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-[11px] font-medium text-slate-500 dark:text-slate-400 uppercase tracking-wider flex items-center gap-1">
              <Filter className="h-3 w-3" />
              日誌等級
            </span>
            {["", "DEBUG", "INFO", "WARN", "ERROR"].map((level) => (
              <button
                key={level}
                onClick={() => setFilterLevel(level)}
                className={[
                  "px-2.5 py-1 rounded-full text-xs font-medium transition-colors",
                  filterLevel === level
                    ? "bg-violet-500 text-white"
                    : "bg-slate-200 dark:bg-slate-800 text-slate-700 dark:text-slate-300 hover:bg-slate-300 dark:hover:bg-slate-700",
                ].join(" ")}
              >
                {level || "全部"}
              </button>
            ))}
          </div>

          {/* Target filter */}
          <div className="flex items-center gap-2">
            <span className="text-[11px] font-medium text-slate-500 dark:text-slate-400 uppercase tracking-wider">
              來源
            </span>
            <input
              type="text"
              placeholder="e.g. telegram, researcher"
              value={filterTarget}
              onChange={(e) => setFilterTarget(e.target.value)}
              className="flex-1 max-w-sm px-2 py-1 text-xs bg-white dark:bg-slate-900 border border-slate-200 dark:border-slate-800 rounded outline-none focus:ring-1 focus:ring-violet-400"
            />
          </div>
        </CardContent>
      </Card>

      {/* ── Error banner ──────────────────────────────────────────────────── */}
      {error && (
        <div className="rounded-lg border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700 dark:border-red-500/40 dark:bg-red-500/10 dark:text-red-300 flex items-start gap-2">
          <AlertCircle className="h-4 w-4 shrink-0 mt-0.5" />
          {error}
        </div>
      )}

      {/* ── Virtual list container ────────────────────────────────────────── */}
      <Card className="overflow-hidden">
        {filteredLogs.length > 0 ? (
          <List
            ref={listRef}
            height={400}
            itemCount={filteredLogs.length}
            itemSize={56}
            width="100%"
            itemData={filteredLogs}
          >
            {LogRow}
          </List>
        ) : (
          <CardContent className="py-16 text-center text-slate-500 dark:text-slate-400">
            {loading ? (
              <div className="flex items-center justify-center gap-2">
                <div className="inline-block animate-spin rounded-full h-4 w-4 border-b-2 border-current" />
                載入中...
              </div>
            ) : (
              <div>
                <LogsIcon className="h-8 w-8 mx-auto mb-2 opacity-30" />
                <p className="text-sm">沒有匹配的日誌</p>
              </div>
            )}
          </CardContent>
        )}
      </Card>

      {/* ── Pagination ────────────────────────────────────────────────────── */}
      <div className="flex items-center justify-between">
        <div className="text-xs text-slate-500 dark:text-slate-400">
          顯示 {offset + 1}–{Math.min(offset + limit, total)} 筆，共 {total} 筆
        </div>

        <div className="flex items-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setOffset(Math.max(0, offset - limit))}
            disabled={!canPrevious || loading}
            className="gap-1"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
            上一頁
          </Button>

          <span className="text-sm text-slate-600 dark:text-slate-400 min-w-[3ch] text-center">
            {currentPage}
          </span>

          <Button
            variant="outline"
            size="sm"
            onClick={() => setOffset(offset + limit)}
            disabled={!canNext || loading}
            className="gap-1"
          >
            下一頁
            <ChevronRight className="h-3.5 w-3.5" />
          </Button>

          <select
            value={limit}
            onChange={(e) => {
              setLimit(parseInt(e.target.value));
              setOffset(0);
            }}
            className="text-xs px-2 py-1 rounded border border-slate-200 dark:border-slate-800 bg-white dark:bg-slate-900 outline-none"
          >
            <option value={25}>25 筆</option>
            <option value={50}>50 筆</option>
            <option value={100}>100 筆</option>
            <option value={200}>200 筆</option>
          </select>
        </div>
      </div>
    </div>
  );
}
