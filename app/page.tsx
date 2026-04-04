"use client";

import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ErrorBoundary } from "@/components/error-boundary";
import { TaskBoard } from "@/components/task-board";

interface TestResult {
  name: string;
  status: "pending" | "success" | "error";
  message: string;
}

function TestPanel() {
  const [tests, setTests] = useState<TestResult[]>([
    { name: "Tauri IPC Available", status: "pending", message: "" },
    { name: "read_tasks_paginated", status: "pending", message: "" },
    { name: "list_research_tasks", status: "pending", message: "" },
    { name: "telegram_get_auth_status", status: "pending", message: "" },
  ]);

  useEffect(() => {
    const runTests = async () => {
      const results: TestResult[] = [];

      // Test 1: Tauri IPC
      try {
        // @ts-ignore
        if (window.__TAURI__) {
          results.push({ name: "Tauri IPC Available", status: "success", message: "✓" });
        } else {
          results.push({ name: "Tauri IPC Available", status: "error", message: "window.__TAURI__ not found" });
        }
      } catch (e) {
        results.push({ name: "Tauri IPC Available", status: "error", message: String(e) });
      }

      // Test 2: read_tasks_paginated
      try {
        const tasksResp = await invoke("read_tasks_paginated", { offset: 0, limit: 10 });
        results.push({
          name: "read_tasks_paginated",
          status: "success",
          message: `✓ Got ${(tasksResp as any).total} total tasks`,
        });
      } catch (e) {
        results.push({ name: "read_tasks_paginated", status: "error", message: String(e) });
      }

      // Test 3: list_research_tasks
      try {
        const research = await invoke("list_research_tasks");
        results.push({
          name: "list_research_tasks",
          status: "success",
          message: `✓ Got ${(research as any[]).length} research tasks`,
        });
      } catch (e) {
        results.push({ name: "list_research_tasks", status: "error", message: String(e) });
      }

      // Test 4: telegram_get_auth_status
      try {
        const status = await invoke("telegram_get_auth_status");
        results.push({
          name: "telegram_get_auth_status",
          status: "success",
          message: `✓ Status: ${(status as any).state}`,
        });
      } catch (e) {
        results.push({ name: "telegram_get_auth_status", status: "error", message: String(e) });
      }

      setTests(results);
    };

    const timer = setTimeout(runTests, 500);
    return () => clearTimeout(timer);
  }, []);

  const allPassed = tests.every((t) => t.status === "success");

  return (
    <div className="max-w-4xl mx-auto mb-8">
      <div className={`rounded-lg border-2 p-6 ${allPassed ? "border-emerald-400 bg-emerald-50 dark:bg-emerald-950/30" : "border-amber-400 bg-amber-50 dark:bg-amber-950/30"}`}>
        <h2 className={`text-lg font-bold mb-4 ${allPassed ? "text-emerald-900 dark:text-emerald-200" : "text-amber-900 dark:text-amber-200"}`}>
          {allPassed ? "✓ 系統狀態: 正常" : "⚠ 系統診斷"}
        </h2>

        <div className="space-y-2">
          {tests.map((test) => (
            <div key={test.name} className="flex items-center gap-3 text-sm">
              <span className="w-6 text-center">
                {test.status === "pending" && "⏳"}
                {test.status === "success" && "✓"}
                {test.status === "error" && "✗"}
              </span>
              <span className="font-medium min-w-40">{test.name}</span>
              <span
                className={`text-xs ${
                  test.status === "success"
                    ? "text-emerald-700 dark:text-emerald-300"
                    : test.status === "error"
                    ? "text-red-700 dark:text-red-300"
                    : "text-gray-500"
                }`}
              >
                {test.message}
              </span>
            </div>
          ))}
        </div>

        {allPassed && (
          <button
            onClick={() => window.location.reload()}
            className="mt-6 px-4 py-2 bg-emerald-600 hover:bg-emerald-700 text-white rounded font-medium"
          >
            Load Task Board
          </button>
        )}
      </div>
    </div>
  );
}

export default function Home() {
  const [showBoard, setShowBoard] = useState(false);
  const [mounted, setMounted] = useState(false);

  useEffect(() => {
    setMounted(true);
  }, []);

  if (!mounted) {
    return (
      <main className="min-h-screen p-6 bg-white dark:bg-slate-950">
        <div className="max-w-4xl mx-auto">
          <h1 className="text-3xl font-bold">Sirin</h1>
          <p className="text-gray-600 dark:text-gray-400 mt-2">Initializing...</p>
        </div>
      </main>
    );
  }

  return (
    <main className="min-h-screen p-6 bg-white dark:bg-slate-950">
      <div className="pt-4">
        <TestPanel />
      </div>

      {showBoard && (
        <ErrorBoundary
          fallback={(error) => (
            <div className="max-w-4xl mx-auto rounded-lg border border-red-300 bg-red-50 dark:border-red-800 dark:bg-red-900/30 p-6">
              <h2 className="text-xl font-bold text-red-900 dark:text-red-200">Component Error</h2>
              <p className="text-sm text-red-800 dark:text-red-300 mt-2 font-mono break-all">{error.message}</p>
              <button
                onClick={() => setShowBoard(false)}
                className="mt-4 px-4 py-2 bg-red-900 text-white rounded hover:bg-red-800"
              >
                Back to Tests
              </button>
            </div>
          )}
        >
          <TaskBoard />
        </ErrorBoundary>
      )}
    </main>
  );
}
