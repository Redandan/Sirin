// vip_maintain.rhai — VIP 客戶維護報告
// 全域變數：skill_id, user_input, agent_id
// 業務邏輯在此實作：可用 http_get() 呼叫 CRM API、read_file() 讀取資料等

log("vip_maintain started");

let now = "（即時）"; // 可改為呼叫時間 API

print(`📋 VIP 維護報告 ${now}`);
print(`請求來源：${agent_id}`);
print(`用戶詢問：${user_input}`);
print("");
print("── 近期需跟進 VIP ──────────────────");
print("• 張三  ｜上次聯繫：3 天前  ｜建議：今日致電");
print("• 李四  ｜上次聯繫：7 天前  ｜建議：發送跟進訊息");
print("• 王五  ｜上次聯繫：1 天前  ｜狀態：正常");
print("");
print("── 本月統計 ────────────────────────");
print("已聯繫商戶：12 次  ｜成功跟進：8 次  ｜轉化率：66%");
