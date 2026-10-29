#pragma once
// ── Per-stage timing, for HANDOFF_ECHO_XBOX.md §18.3 ─────────────────────────
//
// The console cannot quite sustain 4K120: its 50 ms frame queue overflows about
// once a second under full motion. These timers say WHERE the time goes, on the
// diagnostics panel, without changing what any stage does.
//
// The buckets are chosen for one suspect in particular. When the hardware
// decoder is busy, FFmpeg's D3D11VA path retries DecoderBeginFrame with
// av_usleep(2000) (libavcodec/dxva2.c), which an MSVC build turns into
// Sleep(2) -- and on the default ~15.6 ms timer that sleeps a whole tick. So a
// submit that hit it once lands in 14-20 ms and twice in 30-35 ms, while real
// decode work spreads smoothly below 8. A pile in those two buckets convicts
// the sleep; a smooth tail clears it.
//
// Lock-free: Add runs on the decode or present thread, Report on the UI tick.

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cwchar>
#include <string>

namespace echo {

class StageTimer {
public:
    void Add(std::chrono::steady_clock::duration elapsed) noexcept {
        const auto us = std::chrono::duration_cast<std::chrono::microseconds>(elapsed).count();
        const uint32_t v = us <= 0 ? 0u : (us > 0xFFFFFFFFll ? 0xFFFFFFFFu : static_cast<uint32_t>(us));
        size_t i = 0;
        while (i < kBounds && v >= kBoundUs[i]) ++i;
        m_buckets[i].fetch_add(1, std::memory_order_relaxed);
        Raise(m_worstWindow, v);
        Raise(m_worstSession, v);
    }

    // One line: cumulative buckets, then the worst since the previous report
    // and the worst this session. Reading clears the window figure.
    std::wstring Report(const wchar_t* name) noexcept {
        static constexpr const wchar_t* kLabels[kBounds + 1] = {
            L"<2", L"2-8", L"8-14", L"14-20", L"20-35", L"35+" };
        std::wstring out = name;
        for (size_t i = 0; i <= kBounds; ++i) {
            out += L"  " + std::wstring(kLabels[i]) + L" " +
                   std::to_wstring(m_buckets[i].load(std::memory_order_relaxed));
        }
        wchar_t worst[64]{};
        swprintf_s(worst, L"   worst %.1f / %.1f ms",
                   m_worstWindow.exchange(0, std::memory_order_relaxed) / 1000.0,
                   m_worstSession.load(std::memory_order_relaxed) / 1000.0);
        return out + worst;
    }

    void Reset() noexcept {
        for (auto& b : m_buckets) b.store(0, std::memory_order_relaxed);
        m_worstWindow.store(0, std::memory_order_relaxed);
        m_worstSession.store(0, std::memory_order_relaxed);
    }

private:
    static constexpr size_t kBounds = 5;
    static constexpr uint32_t kBoundUs[kBounds] = { 2000, 8000, 14000, 20000, 35000 };

    static void Raise(std::atomic<uint32_t>& slot, uint32_t v) noexcept {
        uint32_t prev = slot.load(std::memory_order_relaxed);
        while (v > prev && !slot.compare_exchange_weak(prev, v, std::memory_order_relaxed)) {}
    }

    std::atomic<uint32_t> m_buckets[kBounds + 1]{};
    std::atomic<uint32_t> m_worstWindow{0};
    std::atomic<uint32_t> m_worstSession{0};
};

// Times the enclosing scope into a StageTimer.
class ScopedStage {
public:
    explicit ScopedStage(StageTimer& timer) noexcept
        : m_timer(timer), m_start(std::chrono::steady_clock::now()) {}
    ~ScopedStage() { m_timer.Add(std::chrono::steady_clock::now() - m_start); }
    ScopedStage(const ScopedStage&) = delete;
    ScopedStage& operator=(const ScopedStage&) = delete;
private:
    StageTimer& m_timer;
    std::chrono::steady_clock::time_point m_start;
};

} // namespace echo
