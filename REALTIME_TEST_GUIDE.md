# 🚀 實時測試指導

應用已在運行中，您可以立即驗證自我開發功能。

## 快速開始

### 步驟 1: 打開應用
```
應用已運行在: http://localhost:3001
```

### 步驟 2: 模擬用戶交互與反饋

在開發工具控制台 (Browser Console) 中執行以下命令：

#### 記錄一個交互
```javascript
// 記錄用戶與AI的對話
invoke('record_interaction', {
  source: 'manual_test',
  input: '如何學習 Rust？',
  output: 'Rust 很難。',
  latency_ms: 800,
  success: true,
  model: 'ollama/mistral'
})
```

#### 記錄負面反饋（觸發自我優化）
```javascript
// 當 record_interaction 返回 interaction_id 後執行
invoke('record_feedback', {
  interaction_id: 'YOUR_INTERACTION_ID_HERE',
  rating: -1,  // 負面反饋
  reason: '回覆太簡短，應提供學習路線圖',
  corrected_output: 'Rust 有陡峭的學習曲線，但提供卓越的性能。推薦：1.所有權系統 2.借用檢查 3.模式匹配'
})
```

#### 查看自我優化任務
```javascript
// 查看最近的任務
invoke('read_tasks').then(tasks => {
  const selfImprovement = tasks.filter(t => t.event === 'self_improvement_request');
  console.log('自我優化任務:', selfImprovement);
})
```

#### 監控自主派遣指標
```javascript
// 查看自主派遣系統的工作情況
invoke('read_autonomous_metrics').then(metrics => {
  console.log('自主派遣統計:', {
    running_research: metrics.running_research,
    pending_tasks: metrics.pending_tasks,
    followup_needed: metrics.followup_needed_tasks,
    success_rate: metrics.success_rate_last_hour
  });
})
```

---

## 觀察要點

### 📊 監控儀表板
在應用的 「Logs」標籤中查看實時監控：
- **Running Research**: 當前執行的研究任務數
- **Pending Tasks**: 待處理任務數
- **Followup Needed**: 需要跟進的任務數
- **Success Rate**: 最近一小時的成功率
- **策略參數**: 最大併發數、冷卻時間等

### 📝 日誌查看
1. 打開檔案瀏覽器
2. 導航到: `C:\Users\Redan\AppData\Local\Sirin\tracking`
3. 打開 `task.jsonl` 檢查事件序列：
   ```json
   {"timestamp":"...", "event":"self_improvement_request", ...}
   {"timestamp":"...", "event":"autonomous_scheduled", ...}
   {"timestamp":"...", "event":"autonomous_completed:research", ...}
   ```

### ⏱️ 時間線
- 記錄反饋 → `self_improvement_request` 任務立即生成（100ms）
- 自主派遣系統掃描 → 每 20 秒（可配置）
- 研究執行 → 取決於 LLM 響應（通常 3-10秒）
- 任務完成 → `autonomous_completed:research` 事件記錄

---

## 完整測試流程

```
1️⃣ 輸入提問
   invoke('record_interaction', {...})
   
2️⃣ 記錄反饋  
   invoke('record_feedback', {rating: -1, ...})
   
3️⃣ 觀察自我優化任務生成
   等待 100ms，檢查 task.jsonl
   
4️⃣ 自主派遣拾取
   等待 20 秒（下一個掃描周期）
   觀察 autonomous_scheduled 事件
   
5️⃣ 任務執行
   觀察 autonomous_completed:research 事件
   
6️⃣ 驗證改進
   檢查任務完成日誌
   觀察監控指標更新
```

---

## 環境變量配置（可選）

在 `.env` 中修改自主派遣行為：

```bash
# 工作線程掃描頻率（秒）
FOLLOWUP_INTERVAL_SECS=20

# 最大並發研究任務
AUTONOMOUS_MAX_CONCURRENT=2

# 每個週期最多派遣數
AUTONOMOUS_MAX_PER_CYCLE=2

# 冷卻鎖定期（秒）
AUTONOMOUS_COOLDOWN_SECS=300

# 最大重試次數
AUTONOMOUS_MAX_RETRIES=2

# LLM 後端配置
LLM_PROVIDER=ollama
OLLAMA_MODEL=llama3.2
```

修改後需要重新編譯：
```bash
cargo build && npm run build
```

---

## 故障排除

### 自我優化任務未生成
- 檢查 `record_feedback` 的 `rating` 是否 < 0
- 確認 feedback.jsonl 是否包含新記錄

### 自主派遣未拾取任務
- 檢查任務的 `message_preview` 是否包含「調研」或「研究」
- 查看任務狀態是否為 `PENDING`
- 檢查冷卻窗口（最近 300 秒內是否已派遣過）

### 任務執行失敗
- 檢查 LLM 後端是否運行（Ollama/LM Studio）
- 查看 Rust console 是否有錯誤日誌
- 驗證網絡連接（如使用遠程 LLM）

---

## 驗證清單

- [ ] 應用在 http://localhost:3001 運行
- [ ] 能夠打開 Browser Console
- [ ] 記錄交互命令執行成功
- [ ] 記錄反饋命令執行成功
- [ ] task.jsonl 中出現 `self_improvement_request` 事件
- [ ] 20 秒內出現 `autonomous_scheduled` 事件
- [ ] 研究完成後出現 `autonomous_completed:research` 事件
- [ ] 監控儀表板指標更新
- [ ] 所有日誌按時間順序出現

✅ 全部通過 = 自我開發功能完整驗證！

---

## 額外資源

- 完整驗證報告: [SELF_IMPROVEMENT_VERIFICATION.md](./SELF_IMPROVEMENT_VERIFICATION.md)
- 架構文檔: [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md)
- 快速開始: [docs/QUICKSTART.md](./docs/QUICKSTART.md)
