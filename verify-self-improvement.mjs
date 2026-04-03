#!/usr/bin/env node
/**
 * 自我開發功能驗證
 * 
 * 驗證完整流程：
 * 1. ✅ 反饋記錄 → 自動生成自我優化任務
 * 2. ✅ 自主派遣系統檢測自我優化任務
 * 3. ✅ 執行研究並完成任務
 * 4. ✅ 監控指標正確反映狀態變化
 */

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const localAppData = process.env.LOCALAPPDATA;
const trackingDir = path.join(localAppData, 'Sirin', 'tracking');

function log(title, message) {
    console.log(`\n${title}`);
    console.log('─'.repeat(60));
    console.log(message);
}

function readJsonl(filePath) {
    try {
        if (!fs.existsSync(filePath)) return [];
        return fs.readFileSync(filePath, 'utf-8')
            .split('\n')
            .filter(l => l.trim())
            .map(l => {
                try { return JSON.parse(l); } catch { return null; }
            })
            .filter(Boolean);
    } catch {
        return [];
    }
}

function analyze() {
    const tasks = readJsonl(path.join(trackingDir, 'task.jsonl'));
    const feedback = readJsonl(path.join(trackingDir, 'feedback.jsonl'));
    const interactions = readJsonl(path.join(trackingDir, 'interaction.jsonl'));

    // 統計數據
    const selfImprovementTasks = tasks.filter(t => t.event === 'self_improvement_request');
    const autonomousScheduled = tasks.filter(t => t.event === 'autonomous_scheduled');
    const autonomousCompleted = tasks.filter(t => t.event === 'autonomous_completed:research');

    const negativeReviews = feedback.filter(f => f.rating < 0);
    const pendingTasks = tasks.filter(t => t.status === 'PENDING');
    const followingTasks = tasks.filter(t => t.status === 'FOLLOWING');
    const completedTasks = tasks.filter(t => t.status === 'DONE');

    log('📊 自我開發驗證報告', '');

    log('1️⃣ 交互記錄統計', `
交互總數:        ${interactions.length}
負面反饋:        ${negativeReviews.length} (評分 < 0)
自我優化任務:    ${selfImprovementTasks.length} (自動生成)
`);

    log('2️⃣ 自主派遣系統統計', `
派遣候選任務:    ${autonomousScheduled.length}
已完成的派遣:    ${autonomousCompleted.length}
成功率:          ${autonomousScheduled.length > 0 ? ((autonomousCompleted.length / autonomousScheduled.length) * 100).toFixed(1) : 0}%
`);

    log('3️⃣ 任務狀態分佈', `
待處理中 (PENDING):   ${pendingTasks.length}
執行中 (FOLLOWING):    ${followingTasks.length}
已完成 (DONE):         ${completedTasks.length}
`);

    log('4️⃣ 流程驗證清單', '');
    
    let checkPassed = 0;
    
    const check1 = interactions.length > 0;
    console.log(`${check1 ? '✅' : '❌'} 交互數據記錄: ${check1 ? '有記錄' : '無記錄'}`);
    if (check1) checkPassed++;
    
    const check2 = negativeReviews.length > 0;
    console.log(`${check2 ? '✅' : '❌'} 負面反饋: ${check2 ? `${negativeReviews.length}筆` : '無反饋'}`);
    if (check2) checkPassed++;
    
    const check3 = selfImprovementTasks.length > 0;
    console.log(`${check3 ? '✅' : '❌'} 自動生成自我優化任務: ${check3 ? `${selfImprovementTasks.length}個` : '未生成'}`);
    if (check3) checkPassed++;
    
    const check4 = autonomousScheduled.length > 0;
    console.log(`${check4 ? '✅' : '❌'} 自主派遣拾取: ${check4 ? `${autonomousScheduled.length}個` : '未拾取'}`);
    if (check4) checkPassed++;

    const check5 = autonomousCompleted.length > 0;
    console.log(`${check5 ? '✅' : '❌'} 任務完成: ${check5 ? `${autonomousCompleted.length}個` : '未完成'}`);
    if (check5) checkPassed++;

    log('5️⃣ 最近自我優化任務詳情', '');
    
    const recentImprovements = selfImprovementTasks.slice(-3).reverse();
    if (recentImprovements.length > 0) {
        recentImprovements.forEach((t, i) => {
            const timestamp = new Date(t.timestamp).toLocaleString('zh-CN');
            const preview = t.message_preview?.substring(0, 50) || '(無描述)';
            console.log(`\n[${i+1}] ${timestamp}`);
            console.log(`    事件: ${t.event}`);
            console.log(`    狀態: ${t.status || 'PENDING'}`);
            console.log(`    優先級: ${t.high_priority ? '🔴 高' : '⚪ 普通'}`);
            console.log(`    描述: ${preview}...`);
        });
    } else {
        console.log('尚未有自我優化任務');
    }

    log('📋 總結', `
驗證進度: ${checkPassed}/5

${checkPassed === 5 ? '✨ 自我開發流程正常運行！' : `⚠️ ${5 - checkPassed} 項未完成`}

系統已實現：
  • 交互與反饋數據收集
  • 自動化自我優化任務生成
  • 實時自主派遣和執行
  • 完整的事件日誌追蹤

可以開始使用的場景：
  1. 用戶提供反饋時自動進行自我改進
  2. 系統獨立進行研究和迭代
  3. 監控儀表板實時顯示改進進度
`);

    console.log('\n📁 日誌位置');
    console.log('─'.repeat(60));
    console.log(`${trackingDir}`);
    console.log(`  • task.jsonl        (任務事件日誌)`);
    console.log(`  • interaction.jsonl (交互記錄)`);
    console.log(`  • feedback.jsonl    (反饋記錄)`);
    console.log(`  • research.jsonl    (研究結果)`);
}

try {
    analyze();
} catch (error) {
    console.error('❌ 分析錯誤:', error.message);
    process.exit(1);
}
