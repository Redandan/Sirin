import { TaskBoard } from "@/components/task-board";
import { TelegramAuthCard } from "@/components/telegram-auth-card";

export default function Home() {
  return (
    <main className="min-h-screen p-6 space-y-4">
      <TelegramAuthCard />
      <TaskBoard />
    </main>
  );
}
