// Architecture derived from Microsoft's WindowsAppSDK widget sample (MIT).
#pragma once

#ifndef NOMINMAX
#define NOMINMAX
#endif

#include <windows.h>
#include <winhttp.h>
#include <combaseapi.h>
#include <stdint.h>

void SignalLocalServerShutdown();

namespace winrt
{
    inline auto get_module_lock() noexcept
    {
        struct service_lock
        {
            uint32_t operator++() noexcept { return ::CoAddRefServerProcess(); }
            uint32_t operator--() noexcept
            {
                const auto refs = ::CoReleaseServerProcess();
                if (refs == 0)
                {
                    SignalLocalServerShutdown();
                }
                return refs;
            }
        };
        return service_lock{};
    }
}
#define WINRT_CUSTOM_MODULE_LOCK

#include <wil/cppwinrt.h>
#include <wil/resource.h>

#include <winrt/Windows.Data.Json.h>
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Storage.h>
#include <winrt/Microsoft.Windows.Widgets.Providers.h>

#include <algorithm>
#include <cmath>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <ctime>
#include <iomanip>
#include <mutex>
#include <optional>
#include <sstream>
#include <string>
#include <thread>
#include <unordered_map>
#include <unordered_set>
#include <vector>

namespace winrt
{
    namespace Microsoft::Windows::Widgets {};
    using namespace Microsoft::Windows::Widgets;

    namespace Microsoft::Windows::Widgets::Providers {};
    using namespace Microsoft::Windows::Widgets::Providers;

    using namespace Windows::Data::Json;
    using namespace Windows::Foundation;
    using namespace Windows::Storage;
}
