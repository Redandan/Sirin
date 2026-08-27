#pragma once

struct WidgetProvider : winrt::implements<WidgetProvider, winrt::IWidgetProvider>
{
    WidgetProvider();
    ~WidgetProvider();

    void CreateWidget(winrt::WidgetContext widgetContext);
    void DeleteWidget(winrt::hstring const& widgetId, winrt::hstring const& customState);
    void OnActionInvoked(winrt::WidgetActionInvokedArgs actionInvokedArgs);
    void OnWidgetContextChanged(winrt::WidgetContextChangedArgs contextChangedArgs);
    void Activate(winrt::WidgetContext widgetContext);
    void Deactivate(winrt::hstring widgetId);

private:
    struct WidgetEntry
    {
        bool active{false};
    };

    void RecoverRunningWidgets();
    void RequestRefresh(winrt::hstring const& widgetId) noexcept;
    void RefreshLoop(std::stop_token stopToken);
    void SendUpdate(
        winrt::hstring const& widgetId,
        bool includeTemplate,
        bool fetchSnapshot = true) noexcept;
    winrt::hstring LoadTemplate();
    winrt::hstring BuildData(bool fetchSnapshot);

    std::mutex m_mutex;
    std::unordered_map<winrt::hstring, WidgetEntry> m_widgets;
    std::unordered_set<winrt::hstring> m_pendingRefreshIds;
    std::atomic_bool m_refreshRequested{false};
    std::jthread m_refreshThread;
};
