// btc_price.rhai — 查詢 BTC 當前價格
// 全域變數：skill_id, user_input, agent_id

log("Fetching BTC price from CoinGecko...");

let body = http_get("https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd,twd");

if body.starts_with("ERROR:") {
    print(`❌ 無法取得 BTC 價格：${body}`);
} else {
    let data = parse_json(body);
    let usd = data["bitcoin"]["usd"];
    let twd = data["bitcoin"]["twd"];

    log(`USD: ${usd}, TWD: ${twd}`);

    print("## BTC 當前價格\n");
    print("| 幣別 | 價格 |");
    print("|------|------|");
    print(`| USD  | **$${usd}** |`);
    print(`| TWD  | **NT$${twd}** |`);
    print("\n> 資料來源：CoinGecko（即時）");
}
