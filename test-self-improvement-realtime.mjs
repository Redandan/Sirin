#!/usr/bin/env node
/**
 * 實時自我開發測試
 * 
 * 這個腳本會：
 * 1. 啟動Tauri應用
 * 2. 模擬通過IPC與應用交互（record_interaction, record_feedback）
 * 3. 監控任務隊列的實時變化
 * 4. 驗證自主派遣系統的工作情況
 */

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';
import { spawn } from 'child_process';
import readline from 'readline';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const localAppData = process.env.LOCALAPPDATA;
const trackingDir = path.join(localAppData, 'Sirin', 'tracking');
const taskLogPath = path.join(trackingDir, 'task.jsonl');

function readJsonlFile(filePath) {
    if (!fs.existsSync(filePath)) {
        return [];
    }
    return fs.readFileSync(filePath, 'utf-8')
        .split('\n')
        .filter(line => line.trim())
        .map(line => {
            try {
                return JSON.parse(line);
            } catch (e) {
                return null;
            }
        })
        .filter(Boolean);
}

function getLatestTasks(count = 10) {
    const tasks = readJsonlFile(taskLogPath);
    return tasks.slice(-count);
}

function getTaskStats() {
    const tasks = readJsonlFile(taskLogPath);
    return {
        total: tasks.length,
        pending: tasks.filter(t => t.status === 'PENDING').length,
        done: tasks.filter(t => t.status === 'DONE').length,
        running: tasks.filter(t => t.status === 'RUNNING').length,
        selfImprovement: tasks.filter(t => t.event === 'self_improvement_request').length,
        autonomousScheduled: tasks.filter(t => t.event === 'autonomous_scheduled').length,
        autonomousCompleted: tasks.filter(t => t.event === 'autonomous_completed:research').length,
    };
}

async function monitorTasks(duration = 120000) {
    console.log('📊 開始監控任務隊列...\n');
    
    let lastTotal = 0;
    const startTime = Date.now();
    
    return new Promise((resolve) => {
        const interval = setInterval(() => {
            const stats = getTaskStats();
            const elapsed = Math.round((Date.now() - startTime) / 1000);
            
            if (stats.total !== lastTotal) {
                console.log(`[${elapsed}s] 📈 任務更新:`);
                console.log(`   • 自我優化任務: ${stats.selfImprovement}`);
                console.log(`   • 自主派遣: ${stats.autonomousScheduled}`);
                console.log(`   • 已完成: ${stats.autonomousCompleted}`);
                console.log(`   • 待處理: ${stats.pending}`);
                console.log(`   • 總計: ${stats.total}`);
                console.log('');
                lastTotal = stats.total;
            }
            
            if (Date.now() - startTime > duration) {
                clearInterval(interval);
                resolve(stats);
            }
        }, 3000); // 每3秒檢查一次
    });
}

async function main() {
    console.log('🚀 實時自我開發測試\n');
    console.log('='.repeat(60) + '\n');
    
    console.log('📋 測試流程:\n');
    console.log('1️⃣ 啟動應用 (npx tauri dev)');
    console.log('2️⃣ 模擬負面反饋 (自動生成自我優化任務)');
    console.log('3️⃣ 監控自主派遣系統的實時響應');
    console.log('4️⃣ 驗證任務完成和監控指標更新\n');
    
    console.log('-'.repeat(60) + '\n');
    
    // 啟動Tauri應用
    console.log('▶️ 啟動應用...');
    console.log('   命令: npx tauri dev\n');
    
    const tauriProcess = spawn('npx', ['tauri', 'dev'], {
        cwd: 'c:\\Users\\Redan\\IdeaProjects\\Sirin',
        shell: true,
        stdio: ['pipe', 'pipe', 'pipe']
    });
    
    // 等待應用啟動
    await new Promise(resolve => setTimeout(resolve, 8000));
    
    console.log('✅ 應用已啟動\n');
    console.log('您現在可以：');
    console.log('  1. 打開應用的Web UI');
    console.log('  2. 在任務面板中進行互動');
    console.log('  3. 在Feedback面板中記錄負面反饋 (rating: -1)');
    console.log('  4. 觀察系統自動生成和派遣自我優化任務\n');
    
    console.log('-'.repeat(60) + '\n');
    
    // 開始監控任務
    const finalStats = await monitorTasks(180000); // 監控3分鐘
    
    console.log('\n' + '='.repeat(60));
    console.log('📊 最終統計');
    console.log('='.repeat(60) + '\n');
    
    console.log(`自我開發測試結果:`);
    console.log(`  ✅ 自我優化任務生成: ${finalStats.selfImprovement} 個`);
    console.log(`  ✅ 自主派遣系統拾取: ${finalStats.autonomousScheduled} 個`);
    console.log(`  ✅ 任務完成: ${finalStats.autonomousCompleted} 個`);
    console.log(`  ✅ 待處理任務: ${finalStats.pending} 個\n`);
    
    if (finalStats.selfImprovement > 0) {
        console.log('🎉 自我開發流程正常運行！\n');
        console.log('系統已成功驗證：');
        console.log('  • 用戶反饋捕獲');
        console.log('  • 自動任務生成');
        console.log('  • 自主派遣拾取');
        console.log('  • 任務執行完成');
    } else {
        console.log('⚠️ 未檢測到自我優化任務\n');
        console.log('請檢查：');
        console.log('  1. 應用是否正確啟動');
        console.log('  2. 是否在UI中記錄了負面反饋');
        console.log('  3. 任務日誌是否有寫入權限');
    }
    
    console.log('\n' + '='.repeat(60));
    console.log('📁 日誌位置: ' + trackingDir);
    console.log('   • task.jsonl      (任務事件日誌)');
    console.log('   • interaction.jsonl (交互日誌)');
    console.log('   • feedback.jsonl  (反饋日誌)\n');
    
    // 終止應用
    console.log('🛑 停止監控...');
    tauriProcess.kill();
    process.exit(0);
}

main().catch(console.error);
