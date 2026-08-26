// COM local-server entrypoint for the Windows Widgets host.
#include "pch.h"
#include "WidgetProvider.h"

static constexpr GUID widget_provider_clsid
{
    0xc0bbcae5, 0x713a, 0x49c7, {0x84, 0x22, 0x3b, 0x28, 0xd6, 0xb0, 0xb3, 0x65}
};

wil::unique_event g_shutdownEvent(wil::EventOptions::None);

void SignalLocalServerShutdown()
{
    g_shutdownEvent.SetEvent();
}

template <typename T>
struct SingletonClassFactory : winrt::implements<SingletonClassFactory<T>, IClassFactory, winrt::no_module_lock>
{
    STDMETHODIMP CreateInstance(IUnknown* outer, GUID const& iid, void** result) noexcept final
    {
        *result = nullptr;
        std::scoped_lock lock(m_mutex);
        if (outer)
        {
            return CLASS_E_NOAGGREGATION;
        }
        if (!m_instance)
        {
            m_instance = winrt::make<T>();
        }
        return m_instance.as(iid, result);
    }

    STDMETHODIMP LockServer(BOOL) noexcept final { return S_OK; }

private:
    std::mutex m_mutex;
    winrt::IWidgetProvider m_instance{nullptr};
};

int WINAPI wWinMain(HINSTANCE, HINSTANCE, PWSTR, int)
{
    winrt::init_apartment(winrt::apartment_type::multi_threaded);

    wil::unique_com_class_object_cookie providerCookie;
    auto factory = winrt::make<SingletonClassFactory<WidgetProvider>>();
    winrt::check_hresult(CoRegisterClassObject(
        widget_provider_clsid,
        factory.get(),
        CLSCTX_LOCAL_SERVER,
        REGCLS_MULTIPLEUSE,
        providerCookie.put()));

    DWORD index{};
    HANDLE events[] = {g_shutdownEvent.get()};
    winrt::check_hresult(CoWaitForMultipleObjects(
        CWMO_DISPATCH_CALLS | CWMO_DISPATCH_WINDOW_MESSAGES,
        INFINITE,
        static_cast<ULONG>(std::size(events)),
        events,
        &index));
    return 0;
}


