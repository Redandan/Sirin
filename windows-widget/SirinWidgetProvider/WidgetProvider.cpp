#include "pch.h"
#include "WidgetProvider.h"

namespace
{
    constexpr wchar_t kWidgetDefinitionId[] = L"Sirin_AI_Work_Monitor";
    constexpr wchar_t kHost[] = L"127.0.0.1";
    constexpr wchar_t kPath[] = L"/api/ai-monitor";
    constexpr INTERNET_PORT kPort = 7700;
    constexpr size_t kMaxResponseBytes = 2 * 1024 * 1024;

    winrt::hstring LocalTimeLabel()
    {
        const auto now = std::chrono::system_clock::now();
        const auto value = std::chrono::system_clock::to_time_t(now);
        std::tm local{};
        localtime_s(&local, &value);

        std::wostringstream output;
        output << std::put_time(&local, L"%H:%M:%S") << L" 本機";
        return winrt::hstring{output.str()};
    }

    uint64_t CurrentUnixTimeMs()
    {
        const auto value = std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::system_clock::now().time_since_epoch()).count();
        return value > 0 ? static_cast<uint64_t>(value) : 0;
    }

    winrt::hstring FormatActivityAge(double updatedAtMs)
    {
        const auto updated = updatedAtMs > 0 ? static_cast<uint64_t>(updatedAtMs) : 0;
        const auto now = CurrentUnixTimeMs();
        const auto ageSeconds = now > updated ? (now - updated) / 1000 : 0;
        if (ageSeconds < 60)
        {
            return L"剛剛";
        }
        if (ageSeconds < 3600)
        {
            return winrt::to_hstring(ageSeconds / 60) + L" 分鐘前";
        }
        if (ageSeconds < 86400)
        {
            return winrt::to_hstring(ageSeconds / 3600) + L" 小時前";
        }
        return winrt::to_hstring(ageSeconds / 86400) + L" 天前";
    }

    winrt::hstring ActivityLabel(winrt::hstring const& activity)
    {
        if (activity == L"ACTIVE_RECENTLY")
        {
            return L"活動中（推定）";
        }
        if (activity == L"RECENT")
        {
            return L"最近活動";
        }
        return L"閒置";
    }

    struct InternetHandle
    {
        HINTERNET value{};
        ~InternetHandle()
        {
            if (value)
            {
                WinHttpCloseHandle(value);
            }
        }
        explicit operator bool() const noexcept { return value != nullptr; }
    };

    std::optional<std::string> FetchSnapshotUtf8()
    {
        InternetHandle session{WinHttpOpen(
            L"Sirin-Windows-Widget/0.1",
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            WINHTTP_NO_PROXY_NAME,
            WINHTTP_NO_PROXY_BYPASS,
            0)};
        if (!session)
        {
            return std::nullopt;
        }
        WinHttpSetTimeouts(session.value, 1000, 1000, 3000, 5000);

        InternetHandle connection{WinHttpConnect(session.value, kHost, kPort, 0)};
        if (!connection)
        {
            return std::nullopt;
        }

        InternetHandle request{WinHttpOpenRequest(
            connection.value,
            L"GET",
            kPath,
            nullptr,
            WINHTTP_NO_REFERER,
            WINHTTP_DEFAULT_ACCEPT_TYPES,
            0)};
        if (!request)
        {
            return std::nullopt;
        }

        if (!WinHttpSendRequest(
                request.value,
                WINHTTP_NO_ADDITIONAL_HEADERS,
                0,
                WINHTTP_NO_REQUEST_DATA,
                0,
                0,
                0) ||
            !WinHttpReceiveResponse(request.value, nullptr))
        {
            return std::nullopt;
        }

        DWORD statusCode{};
        DWORD statusSize = sizeof(statusCode);
        if (!WinHttpQueryHeaders(
                request.value,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                WINHTTP_HEADER_NAME_BY_INDEX,
                &statusCode,
                &statusSize,
                WINHTTP_NO_HEADER_INDEX) ||
            statusCode != 200)
        {
            return std::nullopt;
        }

        std::string body;
        for (;;)
        {
            DWORD available{};
            if (!WinHttpQueryDataAvailable(request.value, &available))
            {
                return std::nullopt;
            }
            if (available == 0)
            {
                break;
            }
            if (body.size() + available > kMaxResponseBytes)
            {
                return std::nullopt;
            }
            const auto offset = body.size();
            body.resize(offset + available);
            DWORD read{};
            if (!WinHttpReadData(request.value, body.data() + offset, available, &read))
            {
                return std::nullopt;
            }
            body.resize(offset + read);
        }
        return body.empty() ? std::nullopt : std::optional<std::string>{std::move(body)};
    }

    double NumberOr(winrt::JsonObject const& object, wchar_t const* name, double fallback = 0.0)
    {
        try
        {
            return object.GetNamedNumber(name, fallback);
        }
        catch (...)
        {
            return fallback;
        }
    }

    winrt::hstring StringOr(
        winrt::JsonObject const& object,
        wchar_t const* name,
        wchar_t const* fallback = L"—")
    {
        try
        {
            return object.GetNamedString(name, fallback);
        }
        catch (...)
        {
            return fallback;
        }
    }

    winrt::hstring FormatCompact(double value)
    {
        std::wostringstream output;
        if (value >= 1'000'000.0)
        {
            output << std::fixed << std::setprecision(1) << (value / 1'000'000.0) << L"M";
        }
        else if (value >= 1'000.0)
        {
            output << std::fixed << std::setprecision(1) << (value / 1'000.0) << L"K";
        }
        else
        {
            output << std::fixed << std::setprecision(0) << value;
        }
        return winrt::hstring{output.str()};
    }

    struct TokenChartView
    {
        winrt::hstring sparkline{L"············"};
        winrt::hstring windowLabel{L"等待歷史資料"};
        winrt::hstring totalLabel{L"—"};
        winrt::hstring peakLabel{L"—"};
    };

    TokenChartView BuildTokenChart(winrt::JsonObject const& trend)
    {
        TokenChartView result;
        try
        {
            const auto history = trend.GetNamedArray(L"history");
            if (history.Size() == 0)
            {
                return result;
            }

            constexpr wchar_t levels[] = L"▁▂▃▄▅▆▇█";
            constexpr size_t chartSlots = 30;
            constexpr double slotMs = 120'000.0;
            constexpr double windowMs = chartSlots * slotMs;

            double newestSampleMs = 0;
            for (const auto& pointValue : history)
            {
                const auto point = pointValue.GetObject();
                newestSampleMs = std::max(newestSampleMs, NumberOr(point, L"sampled_at_ms"));
            }

            // New backends provide an epoch timestamp for every minute sample.
            // Bin those samples into fixed two-minute slots so horizontal space
            // represents real time and sleep/restart gaps stay visibly blank.
            if (newestSampleMs > 0)
            {
                std::vector<double> rates(chartSlots, 0);
                std::vector<bool> present(chartSlots, false);
                const auto windowStartMs = newestSampleMs - windowMs + slotMs;
                double totalTokens = 0;
                double peakRate = 0;

                for (const auto& pointValue : history)
                {
                    const auto point = pointValue.GetObject();
                    const auto sampledAtMs = NumberOr(point, L"sampled_at_ms");
                    if (sampledAtMs < windowStartMs || sampledAtMs > newestSampleMs + slotMs)
                    {
                        continue;
                    }
                    const auto rawSlot = static_cast<size_t>(
                        std::max(0.0, std::floor((sampledAtMs - windowStartMs) / slotMs)));
                    const auto slot = std::min(rawSlot, chartSlots - 1);
                    const auto rate = NumberOr(point, L"tokens_per_min");
                    rates[slot] = present[slot] ? std::max(rates[slot], rate) : rate;
                    present[slot] = true;
                    totalTokens += NumberOr(point, L"delta_tokens");
                    peakRate = std::max(peakRate, rate);
                }

                std::wstring graph;
                graph.reserve(chartSlots);
                for (size_t index = 0; index < chartSlots; ++index)
                {
                    if (!present[index])
                    {
                        graph.push_back(L'·');
                        continue;
                    }
                    const auto scaled = peakRate > 0
                        ? static_cast<size_t>((rates[index] / peakRate) * 7.0 + 0.5)
                        : 0;
                    graph.push_back(levels[std::min<size_t>(scaled, 7)]);
                }

                result.sparkline = winrt::hstring{graph};
                result.windowLabel = L"最近 60 分鐘 · 2 分/格";
                result.totalLabel = L"+" + FormatCompact(totalTokens) + L" tokens";
                result.peakLabel = L"峰值 " + FormatCompact(peakRate) + L"/min";
                return result;
            }

            // Compatibility fallback while a newly installed Widget is still
            // talking to an older Sirin backend without sampled_at_ms.
            std::vector<double> rates;
            rates.reserve(history.Size());
            uint64_t totalSeconds = 0;
            double totalTokens = 0;
            double peakRate = 0;
            for (const auto& pointValue : history)
            {
                const auto point = pointValue.GetObject();
                const auto rate = NumberOr(point, L"tokens_per_min");
                rates.push_back(rate);
                totalSeconds += static_cast<uint64_t>(NumberOr(point, L"interval_secs"));
                totalTokens += NumberOr(point, L"delta_tokens");
                peakRate = std::max(peakRate, rate);
            }
            std::wstring graph;
            const auto visiblePoints = std::min<size_t>(rates.size(), 12);
            graph.append(12 - visiblePoints, L'·');
            const auto start = rates.size() - visiblePoints;
            for (size_t index = start; index < rates.size(); ++index)
            {
                const auto scaled = peakRate > 0
                    ? static_cast<size_t>((rates[index] / peakRate) * 7.0 + 0.5)
                    : 0;
                graph.push_back(levels[std::min<size_t>(scaled, 7)]);
            }
            result.sparkline = winrt::hstring{graph};
            result.windowLabel = totalSeconds < 60
                ? L"最近 " + winrt::to_hstring(totalSeconds) + L" 秒"
                : L"最近 " + winrt::to_hstring((totalSeconds + 30) / 60) + L" 分鐘";
            result.totalLabel = L"+" + FormatCompact(totalTokens) + L" tokens";
            result.peakLabel = L"峰值 " + FormatCompact(peakRate) + L"/min";
        }
        catch (...)
        {
        }
        return result;
    }

    winrt::hstring FormatProcess(winrt::JsonObject const& process)
    {
        if (!process.GetNamedBoolean(L"running", false))
        {
            return L"OFF";
        }
        std::wostringstream output;
        output << static_cast<uint64_t>(NumberOr(process, L"process_count")) << L" · "
               << std::fixed << std::setprecision(0) << NumberOr(process, L"working_set_mb") << L" MB";
        return winrt::hstring{output.str()};
    }

    void PutString(winrt::JsonObject& data, wchar_t const* name, winrt::hstring const& value)
    {
        data.SetNamedValue(name, winrt::JsonValue::CreateStringValue(value));
    }
}
WidgetProvider::WidgetProvider()
{
    RecoverRunningWidgets();
    m_refreshThread = std::jthread([this](std::stop_token stopToken) { RefreshLoop(stopToken); });
}

WidgetProvider::~WidgetProvider()
{
    m_refreshThread.request_stop();
}

void WidgetProvider::CreateWidget(winrt::WidgetContext widgetContext)
{
    if (widgetContext.DefinitionId() != kWidgetDefinitionId)
    {
        return;
    }
    {
        std::scoped_lock lock(m_mutex);
        m_widgets[widgetContext.Id()] = WidgetEntry{true};
    }
    // Do not make the host wait for netsh/ping/API collection. Render a valid
    // placeholder card immediately, then replace it from the worker thread.
    SendUpdate(widgetContext.Id(), true, false);
    RequestRefresh();
}

void WidgetProvider::DeleteWidget(
    winrt::hstring const& widgetId,
    [[maybe_unused]] winrt::hstring const& customState)
{
    std::scoped_lock lock(m_mutex);
    m_widgets.erase(widgetId);
}

void WidgetProvider::OnActionInvoked(winrt::WidgetActionInvokedArgs actionInvokedArgs)
{
    if (actionInvokedArgs.Verb() == L"refresh")
    {
        {
            std::scoped_lock lock(m_mutex);
            m_widgets[actionInvokedArgs.WidgetContext().Id()].active = true;
        }
        RequestRefresh();
    }
}

void WidgetProvider::OnWidgetContextChanged(winrt::WidgetContextChangedArgs contextChangedArgs)
{
    SendUpdate(contextChangedArgs.WidgetContext().Id(), true, false);
    RequestRefresh();
}

void WidgetProvider::Activate(winrt::WidgetContext widgetContext)
{
    {
        std::scoped_lock lock(m_mutex);
        m_widgets[widgetContext.Id()].active = true;
    }
    RequestRefresh();
}

void WidgetProvider::Deactivate(winrt::hstring widgetId)
{
    std::scoped_lock lock(m_mutex);
    if (const auto found = m_widgets.find(widgetId); found != m_widgets.end())
    {
        found->second.active = false;
    }
}

void WidgetProvider::RecoverRunningWidgets()
{
    try
    {
        const auto widgetManager = winrt::WidgetManager::GetDefault();
        for (const auto& widgetInfo : widgetManager.GetWidgetInfos())
        {
            const auto context = widgetInfo.WidgetContext();
            if (context.DefinitionId() == kWidgetDefinitionId)
            {
                m_widgets[context.Id()] = WidgetEntry{true};
                SendUpdate(context.Id(), true, false);
                RequestRefresh();
            }
        }
    }
    catch (...)
    {
        // The widget host may not be ready during provider activation. It will
        // call CreateWidget or Activate when an instance becomes available.
    }
}

void WidgetProvider::RequestRefresh() noexcept
{
    m_refreshRequested.store(true, std::memory_order_release);
}

void WidgetProvider::RefreshLoop(std::stop_token stopToken)
{
    winrt::init_apartment(winrt::apartment_type::multi_threaded);
    while (!stopToken.stop_requested())
    {
        for (int second = 0; second < 60 && !stopToken.stop_requested(); ++second)
        {
            std::this_thread::sleep_for(std::chrono::seconds(1));
            if (m_refreshRequested.exchange(false, std::memory_order_acq_rel))
            {
                break;
            }
        }
        if (stopToken.stop_requested())
        {
            break;
        }

        std::vector<winrt::hstring> activeWidgetIds;
        {
            std::scoped_lock lock(m_mutex);
            for (const auto& [id, entry] : m_widgets)
            {
                if (entry.active)
                {
                    activeWidgetIds.push_back(id);
                }
            }
        }
        for (const auto& id : activeWidgetIds)
        {
            SendUpdate(id, false);
        }
    }
}

void WidgetProvider::SendUpdate(
    winrt::hstring const& widgetId,
    bool includeTemplate,
    bool fetchSnapshot) noexcept
{
    try
    {
        winrt::WidgetUpdateRequestOptions options{widgetId};
        if (includeTemplate)
        {
            options.Template(LoadTemplate());
        }
        options.Data(BuildData(fetchSnapshot));
        options.CustomState(L"sirin-ai-monitor-v1");
        winrt::WidgetManager::GetDefault().UpdateWidget(options);
    }
    catch (...)
    {
        // Fail closed. The host keeps the previous card and will request a
        // refresh again when the widget is activated.
    }
}

winrt::hstring WidgetProvider::LoadTemplate()
{
    static const winrt::hstring widgetTemplate = []
    {
        const auto uri = winrt::Uri(L"ms-appx:///Templates/SirinAiMonitorWidget.json");
        const auto file = winrt::StorageFile::GetFileFromApplicationUriAsync(uri).get();
        return winrt::FileIO::ReadTextAsync(file).get();
    }();
    return widgetTemplate;
}

winrt::hstring WidgetProvider::BuildData(bool fetchSnapshot)
{
    bool online = false;
    bool splitRoute = false;
    bool hasTask1 = false;
    bool hasTask2 = false;
    bool hasTask3 = false;
    bool chatgptRunning = false;
    bool codexRunning = false;
    bool sirinRunning = false;
    winrt::hstring updated = L"未取得";
    winrt::hstring workState = L"正在讀取";
    winrt::hstring workColor = L"Warning";
    winrt::hstring workDetail = L"等待本機工作證據";
    winrt::hstring workEvidence = L"尚未取得活動時間戳";
    winrt::hstring activeSummary = L"—";
    winrt::hstring tokenActivity = L"Token 趨勢需第二次取樣";
    winrt::hstring tokenSparkline = L"············";
    winrt::hstring tokenWindow = L"等待歷史資料";
    winrt::hstring tokenWindowTotal = L"—";
    winrt::hstring tokenPeak = L"—";
    winrt::hstring task1Name = L"—";
    winrt::hstring task1Meta = L"—";
    winrt::hstring task1Color = L"Default";
    winrt::hstring task2Name = L"—";
    winrt::hstring task2Meta = L"—";
    winrt::hstring task2Color = L"Default";
    winrt::hstring task3Name = L"—";
    winrt::hstring task3Meta = L"—";
    winrt::hstring task3Color = L"Default";
    winrt::hstring v4Interface = L"缺證據";
    winrt::hstring v4Metric = L"—";
    winrt::hstring v4Latency = L"無回覆";
    winrt::hstring v6Interface = L"缺證據";
    winrt::hstring v6Metric = L"—";
    winrt::hstring v6Latency = L"無回覆";
    winrt::hstring wifiSignal = L"—";
    winrt::hstring chatgpt = L"OFF";
    winrt::hstring codex = L"OFF";
    winrt::hstring sirin = L"OFF";
    winrt::hstring codexTokens = L"—";
    winrt::hstring tokenRate = L"—";
    winrt::hstring tokenEvidence = L"缺證據";
    winrt::hstring chatgptState = L"未執行";
    winrt::hstring codexState = L"未執行";
    winrt::hstring sirinState = L"未執行";
    winrt::hstring networkSummary = L"網路證據尚未取得";
    winrt::hstring networkWarning = L"";

    if (const auto response = fetchSnapshot ? FetchSnapshotUtf8() : std::nullopt)
    {
        try
        {
            const auto root = winrt::JsonObject::Parse(winrt::to_hstring(*response));
            updated = LocalTimeLabel();

            const auto network = root.GetNamedObject(L"network");
            splitRoute = network.GetNamedBoolean(L"split_default_route", false);
            for (const auto& routeValue : network.GetNamedArray(L"default_routes"))
            {
                const auto route = routeValue.GetObject();
                if (!route.GetNamedBoolean(L"selected", false))
                {
                    continue;
                }
                const auto family = StringOr(route, L"family", L"");
                auto latency = winrt::hstring{L"無回覆"};
                if (route.HasKey(L"latency_ms"))
                {
                    try
                    {
                        latency = winrt::to_hstring(
                            static_cast<uint64_t>(route.GetNamedNumber(L"latency_ms"))) + L" ms";
                    }
                    catch (...)
                    {
                    }
                }
                if (family == L"IPv4")
                {
                    v4Interface = StringOr(route, L"interface_alias", L"缺證據");
                    v4Metric = winrt::to_hstring(
                        static_cast<uint64_t>(NumberOr(route, L"effective_metric")));
                    v4Latency = latency;
                }
                else if (family == L"IPv6")
                {
                    v6Interface = StringOr(route, L"interface_alias", L"缺證據");
                    v6Metric = winrt::to_hstring(
                        static_cast<uint64_t>(NumberOr(route, L"effective_metric")));
                    v6Latency = latency;
                }
            }
            if (network.HasKey(L"wifi"))
            {
                try
                {
                    const auto wifi = network.GetNamedObject(L"wifi");
                    wifiSignal = winrt::to_hstring(
                        static_cast<uint64_t>(NumberOr(wifi, L"signal_percent"))) + L"%";
                }
                catch (...)
                {
                }
            }

            const auto aiWork = root.GetNamedObject(L"ai_work");
            for (const auto& processValue : aiWork.GetNamedArray(L"processes"))
            {
                const auto process = processValue.GetObject();
                const auto app = StringOr(process, L"app", L"");
                if (app == L"ChatGPT")
                {
                    chatgpt = FormatProcess(process);
                    chatgptRunning = process.GetNamedBoolean(L"running", false);
                }
                else if (app == L"Codex")
                {
                    codex = FormatProcess(process);
                    codexRunning = process.GetNamedBoolean(L"running", false);
                }
                else if (app == L"Sirin")
                {
                    sirin = FormatProcess(process);
                    sirinRunning = process.GetNamedBoolean(L"running", false);
                }
            }

            const auto tasks = aiWork.GetNamedArray(L"codex_tasks");
            uint32_t activeCount = 0;
            uint32_t recentCount = 0;
            std::array<winrt::hstring, 3> taskNames{L"—", L"—", L"—"};
            std::array<winrt::hstring, 3> taskMetadata{L"—", L"—", L"—"};
            std::array<winrt::hstring, 3> taskColors{L"Default", L"Default", L"Default"};
            for (uint32_t index = 0; index < tasks.Size(); ++index)
            {
                const auto task = tasks.GetObjectAt(index);
                const auto activity = StringOr(task, L"activity", L"IDLE");
                if (activity == L"ACTIVE_RECENTLY")
                {
                    ++activeCount;
                }
                else if (activity == L"RECENT")
                {
                    ++recentCount;
                }
                if (index < taskNames.size())
                {
                    taskNames[index] = StringOr(task, L"display_name", L"Codex 工作");
                    taskMetadata[index] = ActivityLabel(activity) + L" · " +
                        FormatActivityAge(NumberOr(task, L"updated_at_ms"));
                    taskColors[index] = activity == L"ACTIVE_RECENTLY"
                        ? L"Good"
                        : activity == L"RECENT" ? L"Warning" : L"Default";
                }
            }
            if (tasks.Size() > 0)
            {
                codexTokens = FormatCompact(NumberOr(tasks.GetObjectAt(0), L"tokens_used"));
                task1Name = taskNames[0];
                task1Meta = taskMetadata[0];
                task1Color = taskColors[0];
                hasTask1 = true;
            }
            if (tasks.Size() > 1)
            {
                task2Name = taskNames[1];
                task2Meta = taskMetadata[1];
                task2Color = taskColors[1];
                hasTask2 = true;
            }
            if (tasks.Size() > 2)
            {
                task3Name = taskNames[2];
                task3Meta = taskMetadata[2];
                task3Color = taskColors[2];
                hasTask3 = true;
            }
            const auto trend = aiWork.GetNamedObject(L"codex_token_trend");
            const auto deltaTokens = NumberOr(trend, L"delta_tokens");
            const auto intervalSecs = static_cast<uint64_t>(NumberOr(trend, L"interval_secs"));
            const auto tokensPerMin = NumberOr(trend, L"tokens_per_min");
            tokenRate = FormatCompact(tokensPerMin) + L" / min";
            const auto evidence = StringOr(trend, L"evidence", L"MISSING_PROOF");
            tokenEvidence = evidence == L"INFERRED" ? L"推估" : L"缺證據";
            const auto chart = BuildTokenChart(trend);
            tokenSparkline = chart.sparkline;
            tokenWindow = chart.windowLabel;
            tokenWindowTotal = chart.totalLabel;
            tokenPeak = chart.peakLabel;
            if (evidence == L"INFERRED")
            {
                tokenActivity = deltaTokens > 0
                    ? FormatCompact(deltaTokens) + L" 新增 / " +
                        winrt::to_hstring(intervalSecs) + L" 秒 · " +
                        FormatCompact(tokensPerMin) + L"/min"
                    : L"本次取樣尚無 Token 增量";
            }

            if (activeCount > 0)
            {
                workState = L"推定工作中";
                workColor = L"Good";
                activeSummary = winrt::to_hstring(activeCount) + L" 項近期活動";
                workDetail = winrt::to_hstring(activeCount) + L" 項 Codex 工作在 2 分鐘內更新";
                workEvidence = L"依工作時間戳推定 · 非完成證明";
            }
            else if (recentCount > 0)
            {
                workState = L"最近有活動";
                workColor = L"Warning";
                activeSummary = winrt::to_hstring(recentCount) + L" 項一小時內";
                workDetail = L"Codex 已開啟，最近一小時有更新";
                workEvidence = L"可能等待或暫停 · 無法判定卡住";
            }
            else if (codexRunning)
            {
                workState = L"待命或等待";
                workColor = L"Default";
                activeSummary = L"沒有近期更新";
                workDetail = L"Codex 程序仍在，1 小時內未見工作更新";
                workEvidence = L"無法區分等待使用者與卡住";
            }
            else
            {
                workState = L"目前閒置";
                workColor = L"Default";
                activeSummary = L"Codex 未執行";
                workDetail = L"沒有偵測到 Codex 工作程序";
                workEvidence = L"僅代表本機程序狀態";
            }

            chatgptState = chatgptRunning ? L"已開啟 · 工作狀態缺證據" : L"未執行";
            codexState = workState + L" · " + activeSummary;
            const auto sirinUsage = aiWork.GetNamedObject(L"sirin_tokens");
            const auto sirinCalls = static_cast<uint64_t>(NumberOr(sirinUsage, L"api_calls"));
            sirinState = !sirinRunning
                ? L"未執行"
                : sirinCalls > 0
                    ? L"工作中 · " + winrt::to_hstring(sirinCalls) + L" 呼叫/5分"
                    : L"服務中 · 使用量缺證據";

            networkSummary = L"IPv4 " + v4Interface + L" · " + v4Latency;
            networkWarning = splitRoute
                ? L"IPv6 走 " + v6Interface + L" · " + v6Latency
                : L"IPv4 / IPv6 使用相同介面";
            online = true;
        }
        catch (...)
        {
            online = false;
        }
    }

    if (!online && fetchSnapshot)
    {
        workState = L"無法判定";
        workColor = L"Attention";
        workDetail = L"Sirin 離線，沒有目前 AI 工作證據";
        workEvidence = L"缺證據 · 不推斷為閒置或故障";
    }

    winrt::JsonObject data;
    data.SetNamedValue(L"online", winrt::JsonValue::CreateBooleanValue(online));
    data.SetNamedValue(L"splitRoute", winrt::JsonValue::CreateBooleanValue(splitRoute));
    data.SetNamedValue(L"hasTask1", winrt::JsonValue::CreateBooleanValue(hasTask1));
    data.SetNamedValue(L"hasTask2", winrt::JsonValue::CreateBooleanValue(hasTask2));
    data.SetNamedValue(L"hasTask3", winrt::JsonValue::CreateBooleanValue(hasTask3));
    PutString(data, L"status", online ? workState : L"Sirin 離線 · 缺證據");
    PutString(data, L"updated", updated);
    PutString(data, L"workState", workState);
    PutString(data, L"workColor", workColor);
    PutString(data, L"workDetail", workDetail);
    PutString(data, L"workEvidence", workEvidence);
    PutString(data, L"activeSummary", activeSummary);
    PutString(data, L"tokenActivity", tokenActivity);
    PutString(data, L"tokenSparkline", tokenSparkline);
    PutString(data, L"tokenWindow", tokenWindow);
    PutString(data, L"tokenWindowTotal", tokenWindowTotal);
    PutString(data, L"tokenPeak", tokenPeak);
    PutString(data, L"task1Name", task1Name);
    PutString(data, L"task1Meta", task1Meta);
    PutString(data, L"task1Color", task1Color);
    PutString(data, L"task2Name", task2Name);
    PutString(data, L"task2Meta", task2Meta);
    PutString(data, L"task2Color", task2Color);
    PutString(data, L"task3Name", task3Name);
    PutString(data, L"task3Meta", task3Meta);
    PutString(data, L"task3Color", task3Color);
    PutString(data, L"v4Interface", v4Interface);
    PutString(data, L"v4Metric", v4Metric);
    PutString(data, L"v4Latency", v4Latency);
    PutString(data, L"v6Interface", v6Interface);
    PutString(data, L"v6Metric", v6Metric);
    PutString(data, L"v6Latency", v6Latency);
    PutString(data, L"wifiSignal", wifiSignal);
    PutString(data, L"chatgpt", chatgpt);
    PutString(data, L"codex", codex);
    PutString(data, L"sirin", sirin);
    PutString(data, L"codexTokens", codexTokens);
    PutString(data, L"tokenRate", tokenRate);
    PutString(data, L"tokenEvidence", tokenEvidence);
    PutString(data, L"chatgptState", chatgptState);
    PutString(data, L"codexState", codexState);
    PutString(data, L"sirinState", sirinState);
    PutString(data, L"networkSummary", networkSummary);
    PutString(data, L"networkWarning", networkWarning);
    return data.Stringify();
}
