"use client";

/**
 * TelegramAuthCard
 *
 * Shows the current Telegram connection status.  When the backend is
 * waiting for a login code or 2-FA password it renders an inline form
 * so the user can supply the value without restarting the app.
 *
 * Polls `telegram_get_auth_status` every 3 seconds.  Only visible when
 * the status is anything other than "connected".
 */

import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AlertCircle, CheckCircle2, Loader2, MessageCircle, WifiOff } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";

// ── Types ─────────────────────────────────────────────────────────────────────

type TelegramStatus =
  | { state: "disconnected"; reason: string }
  | { state: "connected" }
  | { state: "code_required" }
  | { state: "password_required"; hint: string }
  | { state: "error"; message: string };

// ── Component ─────────────────────────────────────────────────────────────────

export function TelegramAuthCard() {
  const [status, setStatus] = useState<TelegramStatus | null>(null);
  const [code, setCode]   = useState("");
  const [pass, setPass]   = useState("");
  const [busy, setBusy]   = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  // ── Poll every 3 s ─────────────────────────────────────────────────────────

  const poll = async () => {
    try {
      const s = await invoke<TelegramStatus>("telegram_get_auth_status");
      setStatus(s);
    } catch {
      // Don't crash the board if TG commands aren't available yet
    }
  };

  useEffect(() => {
    poll();
    intervalRef.current = setInterval(poll, 3_000);
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, []);

  // ── Submit code ────────────────────────────────────────────────────────────

  const submitCode = async () => {
    if (!code.trim()) return;
    setBusy(true);
    setFeedback(null);
    try {
      const accepted = await invoke<boolean>("telegram_submit_auth_code", { code: code.trim() });
      setFeedback(accepted ? "驗證碼已送出，等待確認…" : "目前沒有等待驗證的流程");
      if (accepted) setCode("");
    } catch (e) {
      setFeedback(`錯誤：${e}`);
    } finally {
      setBusy(false);
    }
  };

  // ── Submit password ────────────────────────────────────────────────────────

  const submitPass = async () => {
    if (!pass.trim()) return;
    setBusy(true);
    setFeedback(null);
    try {
      const accepted = await invoke<boolean>("telegram_submit_auth_password", { password: pass.trim() });
      setFeedback(accepted ? "密碼已送出，等待確認…" : "目前沒有等待驗證的流程");
      if (accepted) setPass("");
    } catch (e) {
      setFeedback(`錯誤：${e}`);
    } finally {
      setBusy(false);
    }
  };

  // ── Don't render on connected ──────────────────────────────────────────────

  if (!status || status.state === "connected") return null;

  // ── Choose accent colour based on urgency ─────────────────────────────────

  const needsInput = status.state === "code_required" || status.state === "password_required";
  const isError    = status.state === "error";

  const borderClass = needsInput
    ? "border-amber-300 dark:border-amber-500/50"
    : isError
    ? "border-red-300 dark:border-red-500/50"
    : "border-slate-200 dark:border-slate-800";

  const headerClass = needsInput
    ? "text-amber-600 dark:text-amber-400"
    : isError
    ? "text-red-600 dark:text-red-400"
    : "text-slate-500 dark:text-slate-400";

  // ── Render ─────────────────────────────────────────────────────────────────

  return (
    <Card className={`${borderClass} overflow-hidden`}>
      <CardHeader className="pb-2">
        <CardTitle className={`flex items-center gap-2 text-sm ${headerClass}`}>
          {needsInput ? (
            <MessageCircle className="h-4 w-4" />
          ) : isError ? (
            <AlertCircle className="h-4 w-4" />
          ) : (
            <WifiOff className="h-4 w-4" />
          )}
          Telegram 連線狀態
          <Badge
            className={`ml-auto text-[10px] px-1.5 py-0 border ${
              needsInput
                ? "bg-amber-50 border-amber-300 text-amber-600 dark:bg-amber-500/10 dark:border-amber-500/60 dark:text-amber-400"
                : isError
                ? "bg-red-50 border-red-300 text-red-600 dark:bg-red-500/10 dark:border-red-500/60 dark:text-red-400"
                : "bg-slate-50 border-slate-200 text-slate-500 dark:bg-slate-800 dark:border-slate-700 dark:text-slate-400"
            }`}
          >
            {status.state === "disconnected"    && "離線"}
            {status.state === "code_required"   && "等待驗證碼"}
            {status.state === "password_required" && "等待密碼"}
            {status.state === "error"           && "錯誤"}
          </Badge>
        </CardTitle>
      </CardHeader>

      <CardContent className="space-y-3 text-sm">
        {/* ── Disconnected ───────────────────────────────────────────────── */}
        {status.state === "disconnected" && (
          <p className="text-slate-500 dark:text-slate-400">
            {status.reason || "Telegram 連線尚未啟動，將自動重試"}
          </p>
        )}

        {/* ── Error ──────────────────────────────────────────────────────── */}
        {status.state === "error" && (
          <p className="text-red-600 dark:text-red-400">{status.message}</p>
        )}

        {/* ── Code required ──────────────────────────────────────────────── */}
        {status.state === "code_required" && (
          <div className="space-y-2">
            <p className="text-slate-600 dark:text-slate-300">
              Telegram 已向您的手機或 App 發送驗證碼，請在下方輸入：
            </p>
            <div className="flex gap-2">
              <input
                type="text"
                inputMode="numeric"
                placeholder="12345"
                value={code}
                maxLength={10}
                onChange={(e) => setCode(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && submitCode()}
                className="flex-1 rounded-md border border-slate-200 bg-white px-3 py-1.5 text-sm outline-none focus:border-violet-400 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100 dark:focus:border-violet-500"
              />
              <Button size="sm" onClick={submitCode} disabled={busy || !code.trim()}>
                {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "送出"}
              </Button>
            </div>
          </div>
        )}

        {/* ── 2-FA password required ─────────────────────────────────────── */}
        {status.state === "password_required" && (
          <div className="space-y-2">
            <p className="text-slate-600 dark:text-slate-300">
              {status.hint
                ? `需要兩步驟驗證密碼（提示：${status.hint}）：`
                : "需要兩步驟驗證密碼："}
            </p>
            <div className="flex gap-2">
              <input
                type="password"
                placeholder="密碼"
                value={pass}
                onChange={(e) => setPass(e.target.value)}
                onKeyDown={(e) => e.key === "Enter" && submitPass()}
                className="flex-1 rounded-md border border-slate-200 bg-white px-3 py-1.5 text-sm outline-none focus:border-violet-400 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100 dark:focus:border-violet-500"
              />
              <Button size="sm" onClick={submitPass} disabled={busy || !pass.trim()}>
                {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : "送出"}
              </Button>
            </div>
          </div>
        )}

        {/* ── Feedback message ───────────────────────────────────────────── */}
        {feedback && (
          <p className="flex items-center gap-1.5 text-xs text-slate-500 dark:text-slate-400">
            <CheckCircle2 className="h-3.5 w-3.5 shrink-0 text-emerald-500" />
            {feedback}
          </p>
        )}
      </CardContent>
    </Card>
  );
}
