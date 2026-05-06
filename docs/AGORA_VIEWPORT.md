# AgoraMarket 網頁端架構 — Viewport 對照

寫 buyer / seller / delivery / admin E2E 前必讀。viewport 設錯會讓
test 看到錯的版面(桌面 vs 手機),selector 整批失準。

## 各 role viewport 對照表

| 端 | 類型 | 正確 viewport | URL 特徵 |
|---|---|---|---|
| **Buyer**(會員/購物) | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=buyer` |
| **Seller**(商家後台) | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=seller` |
| **Delivery**(外送員) | **H5 手機版** | `390×844 scale=2 mobile=true` | `__test_role=delivery` |
| **Admin**(管理後台) | PC 桌面版 | `1280×900 scale=1 mobile=false` | `__test_role=admin` |

## Buyer / Seller / Delivery 必加 viewport block

```yaml
viewport:
  width: 390
  height: 844
  scale: 2.0
  mobile: true
```

## Viewport 設錯的症狀

- 截圖 > 800KB(應為 ~500KB)
- 看到兩欄寬桌面版面而非手機單欄
- 用 `browser_exec action=set_viewport + screenshot` 先驗證再寫 YAML

## 外送員特殊路由

- `?__test_role=delivery` 落點是 `#/home`
- 需要 `goto target="...#/delivery"` 才會進外送員主畫面

## 跨 role 切換 SOP

```yaml
- action: goto
  target: "${BASE_URL}?__test_role=delivery"
- action: wait
  duration_ms: 5000
- action: enable_a11y
- action: wait
  duration_ms: 2000
```

不要用 `clear_state` 跨 role — 會掉 `__test_role` URL param,
落回 buyer 預設。

## 相關 KB

- `trap-agoramarket-buyer-h5-viewport`
- `sirin-trap-clear-state-loses-test-role-url`
- `agora-trap-production-url-no-auto-login`
