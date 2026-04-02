"use client";

/**
 * Live Task Board
 *
 * Polls the Rust backend every 5 seconds via `invoke('read_tasks')` to fetch
 * the latest 50 task entries, then renders them as macOS-style cards.
 *
 * The "Quick Approve" button calls `invoke('approve_task', { timestamp,
 * skill: 'send_tg_reply' })` which:
 *   1. Updates the task status to "DONE" in the JSONL log.
 *   2. Records a `skill_executed:send_tg_reply` log entry.
 *   3. Emits a `skill:send_tg_reply` Tauri event that the TG module can
 *      subscribe to for sending the actual reply.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { RefreshCw, CheckCircle2, Clock, AlertCircle, Activity } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

// ── Types ─────────────────────────────────────────────────────────────────────

interface TaskEntry {
  timestamp: string;
  event: string;
  persona: string;
  trigger_remote_ai?: boolean;
  estimated_profit_usd?: number;
  /** PENDING | FOLLOWING | FOLLOWUP_NEEDED | DONE */
  status?: string;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

type StatusVariant = "pending" | "following" | "followup" | "done" | "default";

function statusVariant(status?: string): StatusVariant {
  switch (status) {
    case "PENDING":
      return "pending";
    case "FOLLOWING":
      return "following";
    case "FOLLOWUP_NEEDED":
      return "followup";
    case "DONE":
      return "done";
    default:
      return "default";
  }
}

function statusLabel(status?: string): string {
  switch (status) {
    case "PENDING":
      return "Pending";
    case "FOLLOWING":
      return "Following";
    case "FOLLOWUP_NEEDED":
      return "Follow-up Needed";
    case "DONE":
      return "Done";
    default:
      return status ?? "—";
  }
}

function StatusIcon({ status }: { status?: string }) {
  const cls = "h-4 w-4 shrink-0";
  switch (status) {
    case "PENDING":
      return <Clock className={`${cls} text-yellow-500`} />;
    case "FOLLOWING":
      return <Activity className={`${cls} text-blue-500`} />;
    case "FOLLOWUP_NEEDED":
      return <AlertCircle className={`${cls} text-red-500`} />;
    case "DONE":
      return <CheckCircle2 className={`${cls} text-green-500`} />;
    default:
      return null;
  }
}

function formatTimestamp(ts: string): string {
  try {
    return new Intl.DateTimeFormat(undefined, {
      dateStyle: "medium",
      timeStyle: "short",
    }).format(new Date(ts));
  } catch {
    return ts;
  }
}

/** Returns true for statuses that can be actioned by the user. */
function isActionable(status?: string): boolean {
  return status === "PENDING" || status === "FOLLOWING" || status === "FOLLOWUP_NEEDED";
}

// ── Task card ─────────────────────────────────────────────────────────────────

interface TaskCardProps {
  entry: TaskEntry;
  onApprove: (timestamp: string) => Promise<void>;
  approving: boolean;
}

function TaskCard({ entry, onApprove, approving }: TaskCardProps) {
  const variant = statusVariant(entry.status);

  return (
    <Card className="transition-shadow hover:shadow-md">
      <CardHeader className="flex-row items-start justify-between gap-3 space-y-0">
        <div className="flex items-center gap-2 min-w-0">
          <StatusIcon status={entry.status} />
          <CardTitle className="truncate text-sm font-semibold">
            {entry.event}
          </CardTitle>
        </div>
        <Badge variant={variant}>{statusLabel(entry.status)}</Badge>
      </CardHeader>

      <CardContent>
        <div className="grid grid-cols-2 gap-x-6 gap-y-1 text-sm text-gray-600">
          <span className="font-medium text-gray-400 uppercase tracking-wide text-[10px]">
            Persona
          </span>
          <span className="font-medium text-gray-400 uppercase tracking-wide text-[10px]">
            Time
          </span>
          <span className="truncate">{entry.persona}</span>
          <span className="truncate">{formatTimestamp(entry.timestamp)}</span>

          {entry.estimated_profit_usd !== undefined && (
            <>
              <span className="font-medium text-gray-400 uppercase tracking-wide text-[10px] mt-2">
                Est. Profit
              </span>
              <span className="font-medium text-gray-400 uppercase tracking-wide text-[10px] mt-2">
                ROI Triggered
              </span>
              <span className="font-semibold text-green-700">
                ${entry.estimated_profit_usd.toFixed(2)}
              </span>
              <span>{entry.trigger_remote_ai ? "Yes" : "No"}</span>
            </>
          )}
        </div>

        {isActionable(entry.status) && (
          <div className="mt-4 flex justify-end">
            <Button
              size="sm"
              onClick={() => onApprove(entry.timestamp)}
              disabled={approving}
              className="gap-1.5"
            >
              <CheckCircle2 className="h-3.5 w-3.5" />
              Quick Approve
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

// ── Task Board ────────────────────────────────────────────────────────────────

const POLL_INTERVAL_MS = 5000;

export function TaskBoard() {
  const [tasks, setTasks] = useState<TaskEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [approvingId, setApprovingId] = useState<string | null>(null);
  const [lastRefresh, setLastRefresh] = useState<Date | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchTasks = useCallback(async () => {
    try {
      const entries = await invoke<TaskEntry[]>("read_tasks");
      // Show newest entries first.
      setTasks([...entries].reverse());
      setLastRefresh(new Date());
      setError(null);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  // Initial load + auto-refresh every 5 seconds.
  useEffect(() => {
    fetchTasks();
    intervalRef.current = setInterval(fetchTasks, POLL_INTERVAL_MS);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [fetchTasks]);

  const handleApprove = useCallback(
    async (timestamp: string) => {
      setApprovingId(timestamp);
      try {
        await invoke("approve_task", { timestamp, skill: "send_tg_reply" });
        // Immediately refresh so the updated status is visible.
        await fetchTasks();
      } catch (err) {
        setError(String(err));
      } finally {
        setApprovingId(null);
      }
    },
    [fetchTasks]
  );

  // Split tasks into actionable and the rest.
  const actionable = tasks.filter((t) => isActionable(t.status));
  const rest = tasks.filter((t) => !isActionable(t.status));

  return (
    <div className="max-w-3xl mx-auto space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Live Task Board</h1>
          {lastRefresh && (
            <p className="text-sm text-gray-400 mt-0.5">
              Last refreshed {formatTimestamp(lastRefresh.toISOString())}
            </p>
          )}
        </div>
        <Button
          variant="outline"
          size="sm"
          onClick={fetchTasks}
          disabled={loading}
          className="gap-1.5"
        >
          <RefreshCw className={`h-3.5 w-3.5 ${loading ? "animate-spin" : ""}`} />
          Refresh
        </Button>
      </div>

      {/* Error banner */}
      {error && (
        <div className="rounded-lg border border-red-200 bg-red-50 px-4 py-3 text-sm text-red-700">
          {error}
        </div>
      )}

      {/* Loading skeleton */}
      {loading && tasks.length === 0 && (
        <div className="space-y-3">
          {[1, 2, 3].map((n) => (
            <div
              key={n}
              className="h-28 rounded-xl border border-gray-200 bg-white animate-pulse"
            />
          ))}
        </div>
      )}

      {/* Actionable tasks */}
      {actionable.length > 0 && (
        <section className="space-y-3">
          <h2 className="text-xs font-semibold uppercase tracking-widest text-gray-400">
            Needs Attention ({actionable.length})
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

      {/* Other tasks */}
      {rest.length > 0 && (
        <section className="space-y-3">
          <h2 className="text-xs font-semibold uppercase tracking-widest text-gray-400">
            Recent Activity ({rest.length})
          </h2>
          {rest.map((t) => (
            <TaskCard
              key={t.timestamp}
              entry={t}
              onApprove={handleApprove}
              approving={approvingId === t.timestamp}
            />
          ))}
        </section>
      )}

      {/* Empty state */}
      {!loading && tasks.length === 0 && (
        <div className="flex flex-col items-center justify-center rounded-xl border border-dashed border-gray-300 py-16 text-gray-400">
          <Activity className="h-8 w-8 mb-3 opacity-40" />
          <p className="text-sm font-medium">No tasks yet</p>
          <p className="text-xs mt-1">
            Tasks appear here when Sirin processes signals.
          </p>
        </div>
      )}
    </div>
  );
}
