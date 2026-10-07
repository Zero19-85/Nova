#include "pch.h"
#include "VideoRenderer.h"

#include <windows.ui.xaml.media.dxinterop.h>   // ISwapChainPanelNative
#include <winrt/Windows.Graphics.Display.Core.h>
#include <winrt/Windows.Graphics.Display.h>        // AdvancedColorInfo
#include <winrt/Windows.Foundation.Collections.h>

#include <cmath>
#include <vector>
#include <algorithm>

using namespace winrt;
using namespace Windows::UI::Xaml::Controls;
using namespace Windows::Graphics::Display::Core;

namespace echo {
namespace {

// ── `IsSmpte2084Supported` is read as a BYTE, never compared as a bool ──────
//
// The console writes a "true" here whose byte is not 1. Shown live on
// 2026-10-07 by the twin-search verdict table: the 4K120 BT2020 row printed
// "(PQ capable)" -- from `if (mode.IsSmpte2084Supported())`, a nonzero test --
// and, from the same call on the same object, "pq NO" -- from
// `mode.IsSmpte2084Supported() == enable`, a compare against C++ `true` (1).
// Size ok, stereo no, refresh delta 0.000000: the flag was the only failure.
// The verdict column now prints the raw byte, which is the confirmation.
//
// A bool holding any byte other than 0 or 1 is not a valid bool, so C++ gives
// no guarantee at all about comparing it; MSVC compares the bytes. That is why
// every HDR search Echo ever ran found nothing, and why the "Series X reports
// IsSmpte2084Supported clear on 119.88 Hz modes" conclusion of 0e5f2b5 was
// wrong. moonlight-xbox and Kodi never hit it because they compare two OS
// values with each other (`mode.IsSmpte2084Supported() !=
// current.IsSmpte2084Supported()`, i.e. 0xNN != 0), never with a literal.
//
// So the ABI writes into an `unsigned char`, which may hold any value and is
// read as an integer. RawPqByte is kept separate so the verdict table can show
// the byte itself. NEVER call `IsSmpte2084Supported()` directly in this file.
uint8_t RawPqByte(HdmiDisplayMode const& mode) noexcept {
    if (!mode) return 0;
    using Abi = winrt::impl::abi_t<winrt::Windows::Graphics::Display::Core::IHdmiDisplayMode>;
    auto* abi = static_cast<Abi*>(winrt::get_abi(mode));
    unsigned char raw = 0;
    if (!abi || abi->get_IsSmpte2084Supported(reinterpret_cast<bool*>(&raw)) < 0) return 0;
    return raw;
}

bool IsPq(HdmiDisplayMode const& mode) noexcept { return RawPqByte(mode) != 0; }

std::wstring DescribeMode(HdmiDisplayMode const& mode) {
    std::wstring out = std::to_wstring(mode.ResolutionWidthInRawPixels()) + L"x" +
                       std::to_wstring(mode.ResolutionHeightInRawPixels());
    wchar_t hz[40]{};
    swprintf_s(hz, L" @ %.2f Hz", mode.RefreshRate());
    out += hz;
    out += L", " + std::to_wstring(mode.BitsPerPixel()) + L" bpp";
    // What this mode IS, which is not what it SUPPORTS. `IsSmpte2084Supported`
    // says the mode is *capable* of PQ; the colour space says which variant
    // this entry actually is. Only the second answers "is the console about to
    // be driven in a colour space this app does not render in", and that
    // distinction is why the log could look right while the picture was not.
    switch (mode.ColorSpace()) {
        case HdmiDisplayColorSpace::BT2020:      out += L", BT2020"; break;
        case HdmiDisplayColorSpace::BT709:       out += L", BT709"; break;
        case HdmiDisplayColorSpace::RgbFull:     out += L", RGB full"; break;
        case HdmiDisplayColorSpace::RgbLimited:  out += L", RGB limited"; break;
    }
    if (IsPq(mode))    out += L" (PQ capable)";
    if (mode.Is2086MetadataSupported()) out += L" (HDR10 capable)";
    // The one search criterion the table did not show. A stereo entry is
    // skipped by every twin search (Echo's, moonlight's, Kodi's), so a PQ
    // mode that only exists as stereo would look findable here and not be.
    if (mode.StereoEnabled())           out += L" (stereo)";
    return out;
}

// The pixel shader constant buffer, and the layout is the load-bearing part.
//
// HLSL packs a `float3x3` in a constant buffer as three float4 registers, one
// per COLUMN, so the 9 raw coefficients become 12 floats with a pad after each
// group of 3 -- and the CPU side must transpose into that shape or every
// colour comes out of the wrong channel. `offsets` is a float3 and needs its
// own pad to land the following float2 pair on a 16-byte boundary.
//
// Mirrors `moonlight-xbox`'s `_CSC_CONST_BUF` exactly, including the padding
// field, because the shader it feeds is the same shader.
struct CscConstants {
    float cscMatrix[12];
    float offsets[3];
    float padding;
    float chromaOffset[2];
    float chromaTexMax[2];
};
static_assert(sizeof(CscConstants) % 16 == 0,
              "constant buffers must be a multiple of 16 bytes");

}  // namespace

// ── HDMI output mode ────────────────────────────────────────────────────────

HdmiOutcome RequestBestHdmiMode(uint32_t width, uint32_t height) noexcept {
    HdmiOutcome outcome;
    HdmiDisplayInformation hdmi{ nullptr };
    try {
        hdmi = HdmiDisplayInformation::GetForCurrentView();
    } catch (...) {
    }
    if (!hdmi) {
        outcome.note = L"(not a console - HDMI mode left alone)";
        return outcome;
    }

    // Whatever happens below, the answer to "what is the TV showing" is read
    // from the console at the end, not inferred from what we asked for.
    auto readBack = [&hdmi](HdmiOutcome& out) {
        try {
            if (auto mode = hdmi.GetCurrentDisplayMode()) {
                out.width = mode.ResolutionWidthInRawPixels();
                out.height = mode.ResolutionHeightInRawPixels();
                out.refreshHz = mode.RefreshRate();
                // The authority on whether this session may ask for HDR.
                // Deliberately the mode the console REPORTS, not the one that
                // was requested: a request can succeed and still leave the
                // panel in Rec.709, and asking the host for PQ on the strength
                // of a successful call is how the washed-out picture happens.
                //
                // `IsSmpte2084Supported` ALONE. This also tested
                // `ColorSpace() == BT2020`, which is a different property and
                // is false for the RgbFull/RgbLimited entries many consoles
                // use for HDR -- so a console that had genuinely switched to
                // PQ was still reported as SDR. Same mistake as the selection
                // filter above, in the one place that would have caught it.
                out.hdrActive = IsPq(mode);
            }
        } catch (...) {
        }
    };

    try {
        auto current = hdmi.GetCurrentDisplayMode();
        const std::wstring before = current ? DescribeMode(current) : L"(unknown)";

        // ── If the console is already the right size, DO NOT TOUCH IT ──────
        //
        // This is the whole fix for "HDR only at 60 Hz", and the bug it ends
        // was self-inflicted (live 2026-09-22: 4K120 HDR on the dashboard,
        // Echo landing on 4K60 HDR or 4K120 SDR and never both).
        //
        // This function used to request a mode unconditionally, and it asked
        // for an SDR one. On a console whose dashboard is already set to
        // 4K120 HDR that is actively destructive: it drives the panel OUT of
        // PQ, and `RequestHdrMode` then has to find a PQ entry at 119.88 Hz to
        // get back -- which `GetSupportedDisplayModes` does not reliably flag,
        // so it could not.
        //
        // `moonlight-xbox` never hits this because it never requests a mode at
        // all: `SetDisplayHDR` reads the CURRENT mode and, when the console is
        // already in HDR, simply resends that same mode with metadata
        // (`resendCurrentMode`, State/MoonlightClient.cpp:74). It adapts to the
        // console instead of arguing with it.
        //
        // Nova still needs the resolution takeover, because a console WILL sit
        // at 1080p for something it thinks is an app. But taking it when it is
        // already correct buys nothing and costs the dynamic range.
        if (current &&
            current.ResolutionWidthInRawPixels()  == width &&
            current.ResolutionHeightInRawPixels() == height) {
            readBack(outcome);
            outcome.note = L"already at " + before + L" - left alone";
            return outcome;
        }

        // The size is wrong and has to change. Carry the console's CURRENT
        // dynamic range through that change rather than picking one: this
        // function moves the resolution, `RequestHdrMode` moves the transfer
        // function, and neither may undo the other.
        const bool currentIsPq = current && IsPq(current);

        // Pick the highest refresh rate at the requested size. Refresh is the
        // tie-break rather than bit depth because this is a *latency* path: a
        // 120 Hz mode halves the time a finished frame waits for a scanout,
        // and that is worth more here than 10-bit colour.
        //
        // SDR modes are preferred at equal refresh, and that is not a
        // preference — it is a correctness requirement today. This pipeline is
        // SDR end to end: BGRA8 swap chain, BT.709 video processor output, no
        // PQ anywhere (see "Not done: HDR10"). `GetSupportedDisplayModes`
        // returns SDR and HDR10 variants of the SAME resolution and refresh,
        // and nothing here used to tell them apart — so the console could be
        // driven in BT.2020 PQ while the app fed it Rec.709 values. Bright
        // content survives that surprisingly well; near-black does not, which
        // is why it shows up as a background that has gone blue and washed out
        // rather than as an obviously broken picture.
        //
        // When HDR10 lands, this becomes a real choice rather than a filter.
        // Two candidates, not one, and the second is what stops this fix from
        // costing a resolution. A console may offer a size ONLY as a BT2020
        // entry; filtering those out unconditionally would then find nothing
        // and leave the console wherever it was -- trading a washed-out 4K
        // picture for a correct 1080p one, which is not the trade being asked
        // for. So: take the best Rec.709 mode when one exists, and otherwise
        // fall back to the best mode of any colour space and lean on
        // `HdmiDisplayHdrOption::None` below to drive it with an SDR transfer.
        // `outcome.note` says which of the two happened, because "picked an
        // SDR mode" and "picked a BT2020 entry and asked for SDR anyway" fail
        // differently and must not look identical in the log.
        HdmiDisplayMode best{ nullptr };
        double bestRefresh = 0.0;
        HdmiDisplayMode bestAny{ nullptr };
        double bestAnyRefresh = 0.0;
        for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
            if (mode.ResolutionWidthInRawPixels() != width ||
                mode.ResolutionHeightInRawPixels() != height) {
                continue;
            }
            if (mode.RefreshRate() > bestAnyRefresh) {
                bestAnyRefresh = mode.RefreshRate();
                bestAny = mode;
            }
            // ── The discriminator is PQ SUPPORT, not the colour space ──────
            //
            // This read `ColorSpace() == BT2020` and that was wrong, in a way
            // that cost the HDR handshake outright (2026-09-22: 4K reached,
            // stream clamped to HEVC Main 8).
            //
            // The two properties are independent. A console's HDR entries are
            // frequently `RgbFull` or `RgbLimited` rather than `BT2020` --
            // `moonlight-xbox` logs them as separate columns for exactly that
            // reason (`State/MoonlightClient.cpp:38-47`) and keys every HDR
            // decision it makes on `IsSmpte2084Supported` alone. Filtering on
            // the colour space therefore skipped every PQ-capable RGB mode, so
            // the HDR search found nothing and fell back to SDR -- which looks
            // from the sofa exactly like a console refusing the request.
            //
            // `IsSmpte2084Supported` is the property that decides the TRANSFER
            // FUNCTION, which is the thing that actually has to match what the
            // shader emits. It is also what moonlight verifies against after
            // the change, so it is the same question asked the same way.
            const bool isPq = IsPq(mode);
            // A HARD FILTER, not a tie-break -- and that distinction was the
            // whole bug (reported 2026-09-22 as "overly white, blacks grey").
            //
            // This used to prefer SDR only at EQUAL refresh. Consoles do not
            // offer the two variants symmetrically: where the BT2020 entry
            // carries a refresh the Rec.709 entry does not, the old test took
            // the higher number and drove the panel in BT.2020 PQ while the
            // pipeline kept sending Rec.709. Values down the wrong transfer
            // lift near-black off the floor and push highlights toward PQ's
            // 10,000-nit ceiling, which is exactly "blacks go grey, whites
            // blow out".
            //
            // Match whatever the console is ALREADY doing, rather than
            // forcing a transfer function here.
            //
            // This read `isPq != wantHdr` with wantHdr false, i.e. "always
            // pick an SDR mode" -- which dragged a console that was happily in
            // HDR down into SDR just to change resolution. This function's job
            // is the RESOLUTION; `RequestHdrMode` owns the dynamic range, and
            // the two must not fight over it.
            if (isPq != currentIsPq) continue;
            const bool better = mode.RefreshRate() > bestRefresh;
            if (better) {
                bestRefresh = mode.RefreshRate();
                best = mode;
            }
        }

        // No Rec.709 entry at this size: keep the resolution, and say so.
        bool tookWideGamutEntry = false;
        if (!best && bestAny) {
            best = bestAny;
            tookWideGamutEntry = true;
        }

        // Every refresh rate offered at the requested size, listed once.
        //
        // `moonlight-xbox` logs the whole mode table and it is the first thing
        // worth having when a mode request disappoints: "the console refused
        // 4K120" and "this console was never offered 4K120" look identical
        // from the sofa and mean completely different things — the first is an
        // app bug, the second is Settings → General → TV & display options →
        // Video modes → Allow 4K120 being off, or a cable that cannot carry it.
        {
            std::vector<uint32_t> rates;
            for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
                if (mode.ResolutionWidthInRawPixels() != width ||
                    mode.ResolutionHeightInRawPixels() != height) {
                    continue;
                }
                const auto hz = static_cast<uint32_t>(mode.RefreshRate() + 0.5);
                if (std::find(rates.begin(), rates.end(), hz) == rates.end()) rates.push_back(hz);
            }
            std::sort(rates.begin(), rates.end());
            outcome.offered = L"offered at " + std::to_wstring(width) + L"x" +
                              std::to_wstring(height) + L": ";
            if (rates.empty()) {
                outcome.offered += L"nothing";
            } else {
                for (size_t i = 0; i < rates.size(); ++i) {
                    if (i) outcome.offered += L", ";
                    outcome.offered += std::to_wstring(rates[i]) + L" Hz";
                }
            }
        }

        if (!best) {
            readBack(outcome);
            outcome.note = L"no " + std::to_wstring(width) + L"x" + std::to_wstring(height) +
                           L" mode offered; staying at " + before;
            return outcome;
        }

        // Blocking. This runs on a background thread - see the header.
        //
        // `EotfSdr` is stated rather than left to the console's discretion.
        // Choosing an SDR-colour-space mode above says which ENTRY we want;
        // this says which transfer function the console should actually drive,
        // and the two are separate knobs. Asking for both is what makes it
        // deterministic instead of dependent on whatever the console last had
        // configured — and until HDR10 exists in this pipeline, SDR is simply
        // the truth about what we are sending.
        // ── `None`, NOT `EotfSdr` ───────────────────────────────────────────
        //
        // `EotfSdr` looks like the obvious way to say "drive this SDR" and the
        // console answers it with E_INVALIDARG — thrown, not returned. That
        // took the whole function out through the catch below, so the mode was
        // never set, the console stayed at 1920x1080@60, and the swap chain
        // sized itself to match. One wrong enum value cost the app 4K entirely.
        //
        // `moonlight-xbox` uses `HdmiDisplayHdrOption::None` for SDR and
        // `Eotf2084` for HDR, and never anything else. Checked against
        // `State/MoonlightClient.cpp:137-150` rather than reasoned about.
        //
        // Each attempt gets its own try/catch, because "returns false" and
        // "throws" are both real answers here and only one of them was being
        // handled. A fallback that cannot run is not a fallback.
        const auto attempt = [&hdmi, &best](HdmiDisplayHdrOption option) noexcept {
            try {
                return hdmi.RequestSetCurrentDisplayModeAsync(best, option).get();
            } catch (...) {
                return false;
            }
        };

        if (tookWideGamutEntry) {
            // Worth a line of its own: the picture is now riding on the
            // requested transfer actually being honoured. If a washed-out
            // report ever survives this fix, this is the note that says why.
            outcome.note = currentIsPq
                ? L"no PQ entry at this size - took a Rec.709 one: "
                : L"no Rec.709 entry at this size - took a PQ one: ";
        }

        // `Eotf2084` for HDR, `None` for SDR, and never anything else --
        // `EotfSdr` is the value that throws E_INVALIDARG (see above).
        //
        // `outcome.hdrActive` is NOT set from this succeeding. `readBack`
        // fills it from the mode the console reports afterwards, because a
        // request can return true and still leave the panel in Rec.709 -- and
        // a session that asks the host for PQ on the strength of a successful
        // call is exactly the washed-out picture this whole path exists to end.
        const HdmiDisplayHdrOption wanted =
            currentIsPq ? HdmiDisplayHdrOption::Eotf2084 : HdmiDisplayHdrOption::None;

        bool applied = attempt(wanted);
        if (!applied) {
            // The overload without an HDR option at all: let the console keep
            // whatever transfer function it was using. Losing the transfer
            // function is a far better trade than losing the resolution.
            try {
                applied = hdmi.RequestSetCurrentDisplayModeAsync(best).get();
                // `+=`, not `=`: the wide-gamut-fallback note above must not be
                // overwritten here. Both facts matter, and together they are
                // the exact combination that can still wash the picture out.
                if (applied) outcome.note += L"HDR option refused - took the mode as offered: ";
            } catch (...) {
                applied = false;
            }
        }
        readBack(outcome);
        outcome.note += applied ? (L"set " + DescribeMode(best) + L"  (was " + before + L")")
                                : (L"console refused " + DescribeMode(best) +
                                   L"; staying at " + before);
        // What the console is ACTUALLY driving, which is the number every later
        // decision should use. `readBack` has already filled it in from the
        // console rather than from what we asked for — see HdmiOutcome.
        {
            wchar_t actual[80]{};
            swprintf_s(actual, L"  [output now %ux%u @ %.0f Hz]",
                       outcome.width, outcome.height, outcome.refreshHz);
            outcome.note += actual;
        }
        return outcome;
    } catch (hresult_error const& e) {
        readBack(outcome);
        outcome.note = std::wstring(L"HDMI mode request failed: ") + e.message().c_str();
        return outcome;
    } catch (...) {
        readBack(outcome);
        outcome.note = L"HDMI mode request failed";
        return outcome;
    }
}

bool RequestHdrMode(bool enable, std::wstring& note) noexcept {
    HdmiDisplayInformation hdmi{ nullptr };
    try {
        hdmi = HdmiDisplayInformation::GetForCurrentView();
    } catch (...) {
    }
    if (!hdmi) { note = L"(not a console - HDR left alone)"; return false; }

    try {
        auto current = hdmi.GetCurrentDisplayMode();
        if (!current) { note = L"no current display mode"; return false; }

        // ── Already there: RESEND the current mode, do not search ──────────
        //
        // `moonlight-xbox`'s `resendCurrentMode` (State/MoonlightClient.cpp:74),
        // and on a console whose dashboard is set to 4K120 HDR this is the ONLY
        // branch that ever runs. It is why moonlight reaches 4K120 HDR10 on
        // hardware where a mode search cannot: it never performs one. The
        // resend exists to push our mastering metadata to the TV -- the mode
        // itself is already right.
        bool resendCurrentMode = false;
        if (IsPq(current) == enable) {
            if (!enable) { note = L"already in SDR"; return false; }
            resendCurrentMode = true;
            note = L"already in HDR - resending with metadata: ";
        }

        // The mode that differs from the current one ONLY in PQ support.
        //
        // Straight from `moonlight-xbox` (`State/MoonlightClient.cpp:113-125`),
        // including the epsilon on refresh -- `RefreshRate` is a double and the
        // 119.88 Hz entries do not compare equal to themselves across calls.
        // `StereoEnabled` is excluded because a stereo mode matching on every
        // other axis would otherwise be a legal answer here, and it is not one.
        // ── Bit depth is NEVER matched, and must not be ────────────────────
        //
        // HDR10 IS 10-bit. The console reports that as `BitsPerPixel` 30
        // against 24 for 8-bit, so the PQ entry at a given resolution and
        // refresh is a DIFFERENT bit depth than the SDR one by definition.
        // Requiring the depth to match would make the search unsatisfiable
        // exactly when it matters, and the swap chain is already 10-bit
        // (R10G10B10A2) so there is nothing on this side that needs to change
        // when it moves.
        //
        // moonlight matches width, height, refresh and non-stereo, and nothing
        // else, for the same reason.
        // ── The refresh twin, not the exact refresh ─────────────────────────
        //
        // moonlight matches refresh to 0.00001 Hz and gets away with it because
        // it never changes the mode first: it starts from the dashboard's entry,
        // whose PQ twin sits at the identical rate. Echo does change it --
        // `RequestDisplayMode` takes the HIGHEST SDR refresh at the requested
        // size, and a console can list SDR 4K at both 120.00 and 119.88 Hz with
        // the PQ variant only at 119.88. An exact match from 120.00 then finds
        // nothing, the speculative ask below is refused, and the panel stays
        // at "24 bpp RGB limited" while the host sends PQ (live 2026-09-23:
        // bleached 4K120 on the TV where moonlight streams 4K120 HDR10).
        //
        // So: exact first, then the NEAREST refresh within half a hertz. The
        // window is wide enough for the 1000/1001 NTSC pairs and far too narrow
        // to ever trade 120 Hz for 60 -- that trade is not this function's to
        // make silently.
        constexpr double kRefreshTwinToleranceHz = 0.5;
        // A lambda because the search has to be run TWICE -- see "second
        // look" below. It always fetches a fresh list: the whole point of the
        // second run is that the list it gets back can differ from the first.
        const HdmiDisplayMode original = current;
        const auto findTwin = [&hdmi, &original, enable](bool& approx) -> HdmiDisplayMode {
            HdmiDisplayMode best{ nullptr };
            double bestDelta = kRefreshTwinToleranceHz;
            approx = false;
            for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
                if (IsPq(mode) != enable ||
                    mode.ResolutionWidthInRawPixels()  != original.ResolutionWidthInRawPixels() ||
                    mode.ResolutionHeightInRawPixels() != original.ResolutionHeightInRawPixels() ||
                    mode.StereoEnabled()) {
                    continue;
                }
                const double delta = std::fabs(mode.RefreshRate() - original.RefreshRate());
                if (delta <= 0.00001) { approx = false; return mode; }
                // Ties go to the higher rate: this is a latency path.
                if (delta < bestDelta ||
                    (delta == bestDelta && best && mode.RefreshRate() > best.RefreshRate())) {
                    bestDelta = delta;
                    best = mode;
                    approx = true;
                }
            }
            return best;
        };

        // ── When the strict search misses an entry the table plainly shows ──
        //
        // Live 2026-10-07: "second look after the refusal: still no PQ mode
        // near this refresh" -- five fresh searches over a second -- printed
        // directly above a table listing "3840x2160 @ 119.88 Hz, 30 bpp,
        // BT2020 (PQ capable)". So the list was NOT changing; findTwin was
        // rejecting an entry that looks like an exact match. Size, refresh and
        // the PQ flag all print identically; the one predicate the table never
        // showed was StereoEnabled. Rather than guess a third time:
        //
        //  * `findLoose` matches PQ flag + size + refresh to two decimals (the
        //    precision the table prints) and does NOT exclude stereo;
        //  * `verdict` prints every predicate findTwin applies, with the raw
        //    refresh to six places, so the next table names the culprit.
        const auto findLoose = [&hdmi, &original, enable]() -> HdmiDisplayMode {
            const long wantCentiHz = std::lround(original.RefreshRate() * 100.0);
            for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
                if (IsPq(mode) == enable &&
                    mode.ResolutionWidthInRawPixels()  == original.ResolutionWidthInRawPixels() &&
                    mode.ResolutionHeightInRawPixels() == original.ResolutionHeightInRawPixels() &&
                    std::lround(mode.RefreshRate() * 100.0) == wantCentiHz) {
                    return mode;
                }
            }
            return HdmiDisplayMode{ nullptr };
        };
        const auto verdict = [&original, enable](HdmiDisplayMode const& mode) -> std::wstring {
            const bool pqOk   = IsPq(mode) == enable;
            const bool sizeOk = mode.ResolutionWidthInRawPixels()  == original.ResolutionWidthInRawPixels() &&
                                mode.ResolutionHeightInRawPixels() == original.ResolutionHeightInRawPixels();
            const bool stereo = mode.StereoEnabled();
            const double delta = std::fabs(mode.RefreshRate() - original.RefreshRate());
            const bool hzOk   = delta <= kRefreshTwinToleranceHz;
            // The raw byte the OS wrote for IsSmpte2084Supported -- see
            // RawPqByte. Anything other than 0x00/0x01 confirms the cause.
            wchar_t buf[240]{};
            swprintf_s(buf, L"   <- pq %s (raw 0x%02X), size %s, stereo %s, %.6f Hz (d %.6f) %s => %s",
                       pqOk ? L"ok" : L"NO", static_cast<unsigned>(RawPqByte(mode)),
                       sizeOk ? L"ok" : L"NO",
                       stereo ? L"YES (rejected)" : L"no", mode.RefreshRate(), delta,
                       hzOk ? L"ok" : L"NO",
                       (pqOk && sizeOk && !stereo && hzOk) ? L"MATCH" : L"rejected");
            return buf;
        };

        HdmiDisplayMode target{ nullptr };
        bool tookNearTwin = false;
        if (resendCurrentMode) {
            target = current;
        } else {
            target = findTwin(tookNearTwin);
            if (!target && enable) {
                target = findLoose();
                if (target) {
                    note += L"strict search missed it, loose match found " +
                            DescribeMode(target) + verdict(target) + L": ";
                }
            }
        }
        if (tookNearTwin) {
            wchar_t twin[120]{};
            swprintf_s(twin, L"no PQ twin at exactly %.2f Hz - took the one at %.2f Hz: ",
                       current.RefreshRate(), target.RefreshRate());
            note += twin;
        }

        // ── Last resort: ask on the CURRENT mode anyway ────────────────────
        //
        // `GetSupportedDisplayModes` is not reliable about `IsSmpte2084Supported`
        // at high refresh rates: a console that streams 4K120 HDR natively can
        // still enumerate its 119.88 Hz entries with the flag clear, so a
        // search finds nothing while the OS would accept the command happily.
        //
        // Dropping to 60 Hz to satisfy that enumeration was the previous
        // behaviour and it was the wrong trade -- it gave up half the frame
        // rate on the strength of a flag the console contradicts in practice.
        // Asking on the current mode costs one refused call when the flag is
        // honest, and wins 4K120 HDR when it is not. The read-back below
        // decides which happened, so a refusal cannot be mistaken for success.
        bool speculative = false;
        if (!target && enable) {
            target = current;
            speculative = true;
            note = L"no PQ mode enumerated at this refresh - asking on the current mode anyway: ";
        }

        if (!target) {
            // The mode table, because this is the one failure where "this TV
            // cannot do HDR at this resolution" and "we asked wrongly" look
            // identical and mean opposite things.
            // Spell out WHAT is being matched. The previous wording printed
            // the whole current mode, bit depth included, which read as though
            // the depth had to match -- it never did, and that misreading sent
            // a diagnosis down the wrong path.
            wchar_t want[160]{};
            swprintf_s(want,
                       L"no mode offers PQ %s at %ux%u @ %.2f Hz "
                       L"(matching size and refresh only - bit depth is free); offered:",
                       enable ? L"ON" : L"OFF",
                       current.ResolutionWidthInRawPixels(),
                       current.ResolutionHeightInRawPixels(),
                       current.RefreshRate());
            note = want;
            for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
                if (mode.ResolutionWidthInRawPixels()  == current.ResolutionWidthInRawPixels() &&
                    mode.ResolutionHeightInRawPixels() == current.ResolutionHeightInRawPixels()) {
                    note += L"\n              " + DescribeMode(mode);
                }
            }
            return IsPq(current);
        }

        // ── The 2086 metadata, and why it is not optional ───────────────────
        //
        // `moonlight-xbox` uses the THREE-argument overload
        // (mode, option, metadata) and never the two-argument one for HDR.
        // Asking for `Eotf2084` with no mastering metadata gives the console
        // nothing to put in the HDMI infoframe, and that is a plausible reason
        // for a refusal that reports nothing -- which is what the two-argument
        // call was getting.
        //
        // Echo has no equivalent of Sunshine's SS_HDR_METADATA on the wire, so
        // these are the BT.2020 primaries and D65 white point (fixed by the
        // standard) plus the luminance Nova's own HDR10 SEI is built from --
        // `[hdr]` in nova.toml, defaults 1000/1000/400. Host and console
        // therefore describe the same mastering display.
        //
        // Units are not nits across the board: primaries are 0.00002 each,
        // MaxMasteringLuminance is 1 nit, MinMasteringLuminance is 0.0001 nit.
        HdmiDisplayHdr2086Metadata metadata{};
        metadata.RedPrimaryX   = 35400;  metadata.RedPrimaryY   = 14600;   // 0.708, 0.292
        metadata.GreenPrimaryX =  8500;  metadata.GreenPrimaryY = 39850;   // 0.170, 0.797
        metadata.BluePrimaryX  =  6550;  metadata.BluePrimaryY  =  2300;   // 0.131, 0.046
        metadata.WhitePointX   = 15635;  metadata.WhitePointY   = 16450;   // D65
        metadata.MaxMasteringLuminance = 1000;
        metadata.MinMasteringLuminance = 1;      // 0.0001 nit
        metadata.MaxContentLightLevel = 1000;
        metadata.MaxFrameAverageLightLevel = 400;

        const HdmiDisplayHdrOption option =
            enable ? HdmiDisplayHdrOption::Eotf2084 : HdmiDisplayHdrOption::None;

        // With metadata first; without it as a last try. If the console
        // dislikes the mastering data specifically, this separates that from a
        // refusal of the mode or the transfer function.
        const auto ask = [&hdmi, &metadata, &note, option](HdmiDisplayMode const& mode) -> bool {
            try {
                if (hdmi.RequestSetCurrentDisplayModeAsync(mode, option, metadata).get()) return true;
            } catch (...) {
            }
            try {
                if (hdmi.RequestSetCurrentDisplayModeAsync(mode, option).get()) {
                    note += L"(accepted without 2086 metadata) ";
                    return true;
                }
            } catch (...) {
            }
            return false;
        };
        const bool applied = ask(target);

        // Read back. moonlight's own comment on this line is "XXX sometimes
        // this lies and the TV is in another mode", so it is the best answer
        // available rather than a guarantee -- which is exactly why the
        // session's dynamic range is decided from it rather than from
        // `applied`.
        current = hdmi.GetCurrentDisplayMode();
        bool nowPq = current && IsPq(current);
        note += applied ? (L"set " + DescribeMode(target))
                        : (L"console refused " + DescribeMode(target));
        note += nowPq ? L"  [now HDR]" : L"  [now SDR]";
        if (speculative && nowPq) {
            // Worth recording loudly: the console accepted PQ on a mode its
            // own enumeration said could not do it. That is the API quirk,
            // confirmed rather than assumed, and it is why the 60 Hz fallback
            // is gone.
            note += L"  (enumeration said this mode had no PQ - it was wrong)";
        }

        // ── Second look: the list can change underneath us ─────────────────
        //
        // Live 2026-10-07, 4K120 Series X, `hevcPlayback`, app type Game: the
        // first search found NO PQ mode at 119.88 Hz, so the speculative ask
        // went out on the plain SDR entry ("24 bpp, RGB limited") -- which the
        // console refused, as it always would. The mode table printed straight
        // AFTER that refusal listed "3840x2160 @ 119.88 Hz, 30 bpp, BT2020
        // (PQ capable) (HDR10 capable)": the exact twin the search exists to
        // find. Same API, same console, a moment apart. Whether the refusal
        // itself or plain time made it appear is not known; moonlight never
        // sees the gap because it asks mid-stream, long after launch.
        //
        // So after a speculative refusal, fetch the list again (for up to a
        // second) and, if the real twin is there now, ask for THAT.
        if (enable && !nowPq && speculative) {
            HdmiDisplayMode twin{ nullptr };
            bool approx = false;
            for (int look = 0; look < 5 && !twin; ++look) {
                if (look) Sleep(250);
                twin = findTwin(approx);
                if (!twin) twin = findLoose();
            }
            if (twin) {
                note += L"\n              second look after the refusal: the list now offers " +
                        DescribeMode(twin) + L" - asking for that: ";
                const bool again = ask(twin);
                current = hdmi.GetCurrentDisplayMode();
                nowPq = current && IsPq(current);
                note += again ? L"set" : L"console refused it";
                note += nowPq ? L"  [now HDR]" : L"  [now SDR]";
            } else {
                note += L"\n              second look after the refusal: still no PQ mode near this refresh";
            }
        }

        if (enable && !nowPq && current) {
            // HDR wanted, SDR got: print what the console offers at this size,
            // PQ flag included. This is the one table that separates "this
            // console has no PQ entry at this refresh at all" (then HDR here
            // means a lower refresh, which is a policy choice) from "it has
            // one and we asked wrongly" (then it is a bug). Without it the two
            // read identically, and on 2026-10-05 they were confused with each
            // other: the refresh-twin theory was built on a guess about this
            // list instead of on the list.
            // Each row carries findTwin's verdict on it (see `verdict`), and
            // the header says exactly what was searched for.
            wchar_t want[200]{};
            swprintf_s(want, L"\n              searched for: %ux%u, %.6f Hz +/- %.1f, PQ %s, not stereo"
                             L"  (current mode stereo %s)",
                       original.ResolutionWidthInRawPixels(), original.ResolutionHeightInRawPixels(),
                       original.RefreshRate(), kRefreshTwinToleranceHz, enable ? L"ON" : L"OFF",
                       original.StereoEnabled() ? L"YES" : L"no");
            note += want;
            note += L"\n              modes at this size:";
            for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
                if (mode.ResolutionWidthInRawPixels()  == current.ResolutionWidthInRawPixels() &&
                    mode.ResolutionHeightInRawPixels() == current.ResolutionHeightInRawPixels()) {
                    note += L"\n              " + DescribeMode(mode) + verdict(mode);
                }
            }
        }
        return nowPq;
    } catch (hresult_error const& e) {
        note = std::wstring(L"HDR mode request failed: ") + e.message().c_str();
        return false;
    } catch (...) {
        note = L"HDR mode request failed";
        return false;
    }
}

bool ConsoleOutputIsHdr(std::wstring& how) noexcept {
    try {
        using winrt::Windows::Graphics::Display::AdvancedColorKind;
        using winrt::Windows::Graphics::Display::DisplayInformation;
        auto info = DisplayInformation::GetForCurrentView();
        if (!info) { how = L"no DisplayInformation"; return false; }
        auto color = info.GetAdvancedColorInfo();
        if (!color) { how = L"no AdvancedColorInfo"; return false; }
        switch (color.CurrentAdvancedColorKind()) {
            case AdvancedColorKind::HighDynamicRange: how = L"HDR"; return true;
            case AdvancedColorKind::WideColorGamut:   how = L"WCG (not HDR)"; return false;
            case AdvancedColorKind::StandardDynamicRange: how = L"SDR"; return false;
        }
        how = L"unknown kind";
        return false;
    } catch (hresult_error const& e) {
        how = std::wstring(L"query failed: ") + e.message().c_str();
        return false;
    } catch (...) {
        how = L"query failed";
        return false;
    }
}

// ── Renderer ────────────────────────────────────────────────────────────────

VideoRenderer::~VideoRenderer() { Shutdown(); }

void VideoRenderer::SetFrameSource(FrameSource source) noexcept {
    std::lock_guard<std::mutex> guard(m_sourceLock);
    m_frameSource = std::move(source);
}

void VideoRenderer::SetSourceSize(uint32_t width, uint32_t height) noexcept {
    m_sourceWidth.store(width, std::memory_order_relaxed);
    m_sourceHeight.store(height, std::memory_order_relaxed);
}

DisplayFacts VideoRenderer::Initialize(SwapChainPanel const& panel) noexcept {
    DisplayFacts facts;

    try {
        // BGRA_SUPPORT for XAML composition; VIDEO_SUPPORT because the Media
        // Foundation decoder and the video processor both need it on this same
        // device — a second device would mean copying every frame through
        // system memory.
        const UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0 };

        D3D_FEATURE_LEVEL achieved{};
        HRESULT hr = D3D11CreateDevice(
            nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags,
            levels, ARRAYSIZE(levels), D3D11_SDK_VERSION,
            m_device.put(), &achieved, m_context.put());
        if (FAILED(hr)) {
            wchar_t buf[96]{};
            swprintf_s(buf, L"D3D11CreateDevice failed: 0x%08X", static_cast<unsigned>(hr));
            facts.note = buf;
            return facts;
        }

        // NOT optional: the decoder runs on its own threads and shares this
        // context. Without it the corruption looks exactly like packet loss,
        // which sends the search to the network where nothing is wrong.
        if (auto mt = m_device.try_as<ID3D10Multithread>()) {
            mt->SetMultithreadProtected(TRUE);
        }

        // Adapter name.
        if (auto dxgiDevice = m_device.try_as<IDXGIDevice>()) {
            com_ptr<IDXGIAdapter> adapter;
            if (SUCCEEDED(dxgiDevice->GetAdapter(adapter.put()))) {
                DXGI_ADAPTER_DESC desc{};
                if (SUCCEEDED(adapter->GetDesc(&desc))) facts.adapter = desc.Description;
            }
        }

        try {
            if (auto hdmi = HdmiDisplayInformation::GetForCurrentView()) {
                if (auto mode = hdmi.GetCurrentDisplayMode()) facts.hdmiMode = DescribeMode(mode);
            } else {
                facts.hdmiMode = L"(not a console)";
            }
        } catch (...) { facts.hdmiMode = L"(query failed)"; }

        // ── Panel geometry and the composition-scale trick ───────────────────
        //
        // The swap chain is sized in REAL pixels (logical x composition scale)
        // and an inverse matrix keeps XAML laying out in logical units. Sizing
        // it to the logical size instead renders 1080p and lets the console
        // upscale — a soft picture with no error anywhere to explain it.
        const float scaleX = panel.CompositionScaleX();
        const float scaleY = panel.CompositionScaleY();
        const float dipW   = static_cast<float>(panel.ActualWidth());
        const float dipH   = static_cast<float>(panel.ActualHeight());

        facts.panelWidth  = static_cast<uint32_t>(std::lround(dipW));
        facts.panelHeight = static_cast<uint32_t>(std::lround(dipH));
        facts.scaleX = scaleX;
        facts.scaleY = scaleY;

        uint32_t width  = static_cast<uint32_t>(std::lround(dipW * scaleX));
        uint32_t height = static_cast<uint32_t>(std::lround(dipH * scaleY));
        if (width == 0 || height == 0) {
            width = 1920; height = 1080;
            facts.note = L"panel had no size yet; started at 1920x1080";
        }
        facts.backBufferWidth  = width;
        facts.backBufferHeight = height;

        // ── Swap chain ──────────────────────────────────────────────────────
        auto dxgiDevice = m_device.as<IDXGIDevice>();
        com_ptr<IDXGIAdapter> adapter;
        hr = dxgiDevice->GetAdapter(adapter.put());
        if (FAILED(hr)) { facts.note = L"GetAdapter failed"; return facts; }

        com_ptr<IDXGIFactory2> factory;
        hr = adapter->GetParent(IID_PPV_ARGS(factory.put()));
        if (FAILED(hr)) { facts.note = L"GetParent(IDXGIFactory2) failed"; return facts; }

        // ── Tearing ─────────────────────────────────────────────────────
        //
        // Asked for, and checked rather than assumed. A composition swap chain
        // is drawn by the compositor, which owns the scanout — so even where
        // DXGI reports the feature, a SwapChainPanel is very unlikely to
        // actually tear. The flag is set when it is offered because it costs
        // nothing and removes a present-time vblank wait where it is honoured,
        // and `facts.note` reports which of the two happened instead of
        // claiming a latency win that may not exist.
        //
        // What does the real work here is SyncInterval 0 plus FLIP_DISCARD:
        // present the newest buffer and do not queue.
        if (auto factory5 = factory.try_as<IDXGIFactory5>()) {
            BOOL allowed = FALSE;
            if (SUCCEEDED(factory5->CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                                                        &allowed, sizeof(allowed)))) {
                m_tearingSupported = allowed != FALSE;
            }
        }

        DXGI_SWAP_CHAIN_DESC1 desc{};
        desc.Width  = width;
        desc.Height = height;
        // 10-bit, matching `moonlight-xbox` (`Common/DeviceResources.cpp:59`,
        // `m_backBufferFormat(DXGI_FORMAT_R10G10B10A2_UNORM) // 10-bit for HDR`),
        // which uses it unconditionally for SDR and HDR alike.
        //
        // It earns its place before HDR10 exists here. The video processor
        // writes Rec.709 RGB into this buffer at 8 bits per channel today, and
        // 8-bit is where near-black banding in a dark UI comes from; 10-bit
        // costs one extra byte per pixel of bandwidth and nothing in latency.
        // It is also a precondition for ever declaring a PQ colour space,
        // because DXGI will not accept `RGB_FULL_G2084_NONE_P2020` on an 8-bit
        // swap chain -- so adopting it now removes a step from the HDR work
        // rather than adding one.
        //
        // `CreateSwapChainSurfaces` asks for a render-target view with a null
        // desc, which takes the buffer's own format, and the clear colour is
        // (0,0,0,1) -- zero is zero under either format. Nothing else in this
        // file names the back-buffer format, so this is the only line to change.
        desc.Format = DXGI_FORMAT_R10G10B10A2_UNORM;
        desc.SampleDesc.Count = 1;
        desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        desc.BufferCount = 2;
        desc.Scaling = DXGI_SCALING_STRETCH;              // required for composition
        // FLIP_DISCARD, not FLIP_SEQUENTIAL. Sequential presents every buffer in
        // order, so a present at SyncInterval 0 still enqueues one — which is
        // the queue this whole change exists to remove. Discard shows the
        // newest and abandons the rest, which is what a live stream wants.
        desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
        desc.AlphaMode = DXGI_ALPHA_MODE_IGNORE;
        desc.Flags = SwapChainFlags();

        com_ptr<IDXGISwapChain1> swapChain1;
        hr = factory->CreateSwapChainForComposition(m_device.get(), &desc, nullptr, swapChain1.put());
        if (FAILED(hr) && m_tearingSupported) {
            // Some drivers advertise tearing and then refuse it on a composition
            // swap chain. Losing the picture over an optimisation would be a bad
            // trade, so drop the flag and take the compositor's pacing.
            m_tearingSupported = false;
            desc.Flags = SwapChainFlags();
            hr = factory->CreateSwapChainForComposition(m_device.get(), &desc, nullptr,
                                                        swapChain1.put());
        }
        if (FAILED(hr) && desc.Format == DXGI_FORMAT_R10G10B10A2_UNORM) {
            // 10-bit composition swap chains are not universally creatable.
            // The picture matters more than the extra two bits, so fall back
            // to the format this shipped with rather than failing outright.
            desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
            hr = factory->CreateSwapChainForComposition(m_device.get(), &desc, nullptr,
                                                        swapChain1.put());
        }
        if (FAILED(hr)) {
            wchar_t buf[96]{};
            swprintf_s(buf, L"CreateSwapChainForComposition failed: 0x%08X", static_cast<unsigned>(hr));
            facts.note = buf;
            return facts;
        }

        m_swapChain = swapChain1.as<IDXGISwapChain2>();

        // ── Declare the colour space, which nothing here ever did ───────────
        //
        // Without this the swap chain carries DXGI's default,
        // `RGB_FULL_G22_NONE_P709`, by assumption rather than by statement --
        // and an assumption is exactly what fails when the console is already
        // being driven in BT.2020 PQ (Moonlight sets `Eotf2084` and the
        // console can still be in it when this app starts). Saying it
        // explicitly is what lets the compositor convert instead of passing
        // Rec.709 values down a PQ transfer.
        //
        // `moonlight-xbox` does this in `Streaming/VideoRenderer.cpp:161-181`,
        // and the shape is copied from it deliberately: check first with
        // `CheckColorSpaceSupport` for the PRESENT flag, and only then set.
        // An unchecked `SetColorSpace1` is a hard failure on a swap chain that
        // cannot honour the space, and losing the picture to a colour-accuracy
        // call would be a bad trade.
        //
        // SDR today. When the HDR10 path lands, this same call takes
        // `DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020` instead, chosen from
        // the stream's transfer characteristics the way moonlight chooses it
        // from `frame->color_trc`.
        if (auto swapChain3 = m_swapChain.try_as<IDXGISwapChain3>()) {
            const DXGI_COLOR_SPACE_TYPE space = DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;
            UINT support = 0;
            if (SUCCEEDED(swapChain3->CheckColorSpaceSupport(space, &support)) &&
                (support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT)) {
                swapChain3->SetColorSpace1(space);
            }
        }

        m_swapChain->SetMaximumFrameLatency(1);
        m_frameLatencyWaitable.attach(m_swapChain->GetFrameLatencyWaitableObject());

        DXGI_MATRIX_3X2_F transform{};
        transform._11 = 1.0f / scaleX;
        transform._22 = 1.0f / scaleY;
        m_swapChain->SetMatrixTransform(&transform);

        {
            std::lock_guard<std::mutex> guard(m_lock);
            if (!CreateSwapChainSurfaces()) { facts.note = L"back-buffer view failed"; return facts; }
        }

        auto panelNative = panel.as<ISwapChainPanelNative>();
        hr = panelNative->SetSwapChain(m_swapChain.get());
        if (FAILED(hr)) { facts.note = L"SetSwapChain failed"; return facts; }

        m_running.store(true, std::memory_order_release);
        m_presentThread = std::thread([this] { PresentLoop(); });

        // Say which of the two we got. "Tearing supported" on a composited panel
        // is a claim worth being able to check against the console rather than
        // against a comment.
        {
            const std::wstring present = m_tearingSupported
                ? L"present     immediate, tearing allowed (flip-discard, latency 1)"
                : L"present     immediate, compositor-paced (flip-discard, latency 1)";
            facts.note = facts.note.empty() ? present : facts.note + L"\n" + present;
        }

        facts.ok = true;
        return facts;
    } catch (hresult_error const& e) {
        facts.note = std::wstring(e.message().c_str());
        return facts;
    } catch (...) {
        facts.note = L"unknown failure creating the renderer";
        return facts;
    }
}

bool VideoRenderer::CreateSwapChainSurfaces() noexcept {
    m_backBufferView = nullptr;
    com_ptr<ID3D11Texture2D> backBuffer;
    if (FAILED(m_swapChain->GetBuffer(0, IID_PPV_ARGS(backBuffer.put())))) return false;
    return SUCCEEDED(m_device->CreateRenderTargetView(backBuffer.get(), nullptr, m_backBufferView.put()));
}

// ── The shader pipeline ─────────────────────────────────────────────────────
//
// Built once and kept. `m_shaderTried` makes a failure sticky: the bytecode is
// packaged content, so if it is missing it will be missing every frame, and
// retrying the load sixty times a second would bury the reason rather than
// report it.
bool VideoRenderer::EnsureShaderPipeline() noexcept {
    if (m_shaderReady) return true;
    if (m_shaderTried) return false;
    m_shaderTried = true;

    // Packaged beside the app, compiled by build-app.ps1. Read synchronously:
    // this runs once, on the render thread, before the first picture.
    const auto load = [](wchar_t const* name, std::vector<uint8_t>& out) noexcept {
        try {
            const auto path = std::wstring(
                winrt::Windows::ApplicationModel::Package::Current()
                    .InstalledLocation().Path().c_str()) + L"\\Assets\\Shader\\" + name;
            winrt::file_handle file{ CreateFile2(path.c_str(), GENERIC_READ, FILE_SHARE_READ,
                                                 OPEN_EXISTING, nullptr) };
            if (!file) return false;
            LARGE_INTEGER size{};
            if (!GetFileSizeEx(file.get(), &size) || size.QuadPart <= 0 ||
                size.QuadPart > (1 << 20)) {
                return false;
            }
            out.resize(static_cast<size_t>(size.QuadPart));
            DWORD read = 0;
            return ReadFile(file.get(), out.data(), static_cast<DWORD>(out.size()), &read, nullptr)
                   && read == out.size();
        } catch (...) {
            return false;
        }
    };

    std::vector<uint8_t> vs, ps;
    if (!load(L"echo_video_vertex.cso", vs) || !load(L"echo_video_pixel.cso", ps)) {
        return Fail(RenderFailure::ShaderFile, HRESULT_FROM_WIN32(GetLastError()));
    }

    if (const HRESULT hr = m_device->CreateVertexShader(vs.data(), vs.size(), nullptr,
                                                        m_vertexShader.put()); FAILED(hr)) {
        return Fail(RenderFailure::ShaderCreate, hr);
    }
    if (const HRESULT hr = m_device->CreatePixelShader(ps.data(), ps.size(), nullptr,
                                                       m_pixelShader.put()); FAILED(hr)) {
        return Fail(RenderFailure::ShaderCreate, hr);
    }

    const D3D11_INPUT_ELEMENT_DESC layout[] = {
        { "POSITION", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 0, D3D11_INPUT_PER_VERTEX_DATA, 0 },
        { "TEXCOORD", 0, DXGI_FORMAT_R32G32_FLOAT, 0, 8, D3D11_INPUT_PER_VERTEX_DATA, 0 },
    };
    if (const HRESULT hr = m_device->CreateInputLayout(layout, 2, vs.data(), vs.size(),
                                                       m_inputLayout.put()); FAILED(hr)) {
        return Fail(RenderFailure::PipelineState, hr);
    }

    // LINEAR, and CLAMP on both axes. The clamp is a second line of defence
    // behind the shader chroma clamp: between them, nothing the sampler does
    // can reach the alignment padding past the coded picture.
    D3D11_SAMPLER_DESC samp{};
    samp.Filter = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
    samp.AddressU = samp.AddressV = samp.AddressW = D3D11_TEXTURE_ADDRESS_CLAMP;
    samp.ComparisonFunc = D3D11_COMPARISON_NEVER;
    samp.MaxLOD = D3D11_FLOAT32_MAX;
    if (const HRESULT hr = m_device->CreateSamplerState(&samp, m_sampler.put()); FAILED(hr)) {
        return Fail(RenderFailure::PipelineState, hr);
    }

    D3D11_BUFFER_DESC cb{};
    cb.ByteWidth = sizeof(CscConstants);
    cb.Usage = D3D11_USAGE_DEFAULT;          // UpdateSubresource, not a map
    cb.BindFlags = D3D11_BIND_CONSTANT_BUFFER;
    if (const HRESULT hr = m_device->CreateBuffer(&cb, nullptr, m_cscBuffer.put()); FAILED(hr)) {
        return Fail(RenderFailure::PipelineState, hr);
    }

    m_shaderReady = true;
    return true;
}

// A texture of OUR OWN, and a copy into it before anything samples.
//
// ── Why this is not zero-copy, deliberately ────────────────────────────────
//
// The decoder hands back a slice of its own pool and keeps owning it. Binding
// an SRV straight onto that slice is possible (it needs
// `D3D11_BIND_SHADER_RESOURCE` on the pool) and it is what this did for one
// commit. The problem is lifetime: the surface is the decoder's, it goes back
// into rotation as soon as the AVFrame reference is dropped, and the draw that
// samples it is asynchronous. Every guarantee that this is safe lives in the
// driver's hazard tracking rather than in anything visible here.
//
// `moonlight-xbox` does not rely on that, and it is the reference this port is
// meant to mirror: `Streaming/VideoRenderer.cpp:130` copies the slice into a
// private single-slice texture with `CopySubresourceRegion1` +
// `D3D11_COPY_DISCARD`, and binds SRVs to the copy. The decoder's surfaces are
// then never read by the render thread at all.
//
// The cost is one GPU-side copy of an NV12/P010 picture per frame -- no CPU
// involvement, no readback, and cheap next to the colour conversion that
// follows it. The benefit is that the decoder and the renderer stop sharing
// memory, which is the entire class of bug this removes rather than mitigates.
//
// `D3D11_COPY_DISCARD` states that the destination's previous contents are
// dead, so the driver need not preserve them across the write -- without it a
// copy into a texture the GPU may still be reading has to serialise.
bool VideoRenderer::EnsurePlaneViews(ID3D11Texture2D* frame, uint32_t slice,
                                     D3D11_TEXTURE2D_DESC const& desc,
                                     ID3D11ShaderResourceView* out[2]) noexcept {
    // (Re)build our texture whenever the picture's shape or format changes.
    if (!m_videoTexture || m_videoTexW != desc.Width || m_videoTexH != desc.Height ||
        m_videoTexFormat != desc.Format) {
        m_planeViews.clear();
        m_videoTexture = nullptr;

        D3D11_TEXTURE2D_DESC own{};
        own.Width = desc.Width;
        own.Height = desc.Height;
        own.MipLevels = 1;
        own.ArraySize = 1;               // single slice: nothing to index
        own.Format = desc.Format;        // NV12 or P010, unchanged
        own.SampleDesc.Count = 1;
        own.Usage = D3D11_USAGE_DEFAULT;
        own.BindFlags = D3D11_BIND_SHADER_RESOURCE;
        if (const HRESULT hr = m_device->CreateTexture2D(&own, nullptr, m_videoTexture.put());
            FAILED(hr)) {
            return Fail(RenderFailure::VideoTexture, hr);
        }

        // P010 keeps its 10 bits in the HIGH bits of a 16-bit word, so an
        // R16_UNORM view normalises to very nearly code/1023 -- the scale the
        // CSC constants are derived at. NV12 is the 8-bit pair.
        const bool tenBit = (desc.Format == DXGI_FORMAT_P010);
        const DXGI_FORMAT planeFormats[2] = {
            tenBit ? DXGI_FORMAT_R16_UNORM    : DXGI_FORMAT_R8_UNORM,
            tenBit ? DXGI_FORMAT_R16G16_UNORM : DXGI_FORMAT_R8G8_UNORM,
        };
        m_planeViews.resize(1);
        for (int i = 0; i < 2; ++i) {
            D3D11_SHADER_RESOURCE_VIEW_DESC srv{};
            srv.Format = planeFormats[i];
            srv.ViewDimension = D3D11_SRV_DIMENSION_TEXTURE2D;
            srv.Texture2D.MostDetailedMip = 0;
            srv.Texture2D.MipLevels = 1;
            if (const HRESULT hr = m_device->CreateShaderResourceView(
                    m_videoTexture.get(), &srv, m_planeViews[0][i].put()); FAILED(hr)) {
                m_planeViews.clear();
                m_videoTexture = nullptr;
                return Fail(RenderFailure::PlaneViews, hr);
            }
        }
        m_videoTexW = desc.Width;
        m_videoTexH = desc.Height;
        m_videoTexFormat = desc.Format;
    }

    // The decoder's pool is a texture ARRAY, so the source subresource must
    // name the slice. Getting this wrong copies slice 0 every time, which
    // looks like a stream frozen on its first picture.
    if (auto ctx1 = m_context.try_as<ID3D11DeviceContext1>()) {
        ctx1->CopySubresourceRegion1(m_videoTexture.get(), 0, 0, 0, 0, frame, slice,
                                     nullptr, D3D11_COPY_DISCARD);
    } else {
        m_context->CopySubresourceRegion(m_videoTexture.get(), 0, 0, 0, 0, frame, slice, nullptr);
    }

    out[0] = m_planeViews[0][0].get();
    out[1] = m_planeViews[0][1].get();
    return true;
}

// The letterbox, baked into clip space.
//
// The video processor used to do this scaling; a shader does it by drawing a
// smaller quad. Same result, and it costs one vertex-buffer rebuild whenever
// the picture or the back buffer changes size rather than anything per frame.
void VideoRenderer::EnsureQuad(D3D11_TEXTURE2D_DESC const& desc) noexcept {
    DXGI_SWAP_CHAIN_DESC1 sc{};
    if (FAILED(m_swapChain->GetDesc1(&sc))) return;

    // The CODED size, not the texture size: a decoder surface is allocated
    // aligned (1088 rows for 1080) and the extra rows are undefined memory.
    uint32_t codedW = m_sourceWidth.load(std::memory_order_relaxed);
    uint32_t codedH = m_sourceHeight.load(std::memory_order_relaxed);
    if (!codedW || !codedH || codedW > desc.Width || codedH > desc.Height) {
        codedW = desc.Width;
        codedH = desc.Height;
    }

    if (m_quad && m_quadSrcW == desc.Width && m_quadSrcH == desc.Height &&
        m_quadDstW == sc.Width && m_quadDstH == sc.Height &&
        m_quadCodedW == codedW && m_quadCodedH == codedH) {
        return;
    }

    const float scale = (std::min)(static_cast<float>(sc.Width) / codedW,
                                   static_cast<float>(sc.Height) / codedH);
    const float w = codedW * scale;
    const float h = codedH * scale;
    const float x = (sc.Width - w) * 0.5f;
    const float y = (sc.Height - h) * 0.5f;

    // Screen space to NDC. Y is flipped: NDC +1 is the top of the screen.
    const float l = (x / (sc.Width * 0.5f)) - 1.0f;
    const float r = ((x + w) / (sc.Width * 0.5f)) - 1.0f;
    const float t = 1.0f - (y / (sc.Height * 0.5f));
    const float b = 1.0f - ((y + h) / (sc.Height * 0.5f));

    // Crop the alignment padding through the texture coordinates, which is
    // what the video processor source rect used to do.
    const float uMax = static_cast<float>(codedW) / desc.Width;
    const float vMax = static_cast<float>(codedH) / desc.Height;

    struct Vertex { float x, y, u, v; };
    const Vertex verts[4] = {          // triangle strip
        { l, t, 0.0f, 0.0f },
        { r, t, uMax, 0.0f },
        { l, b, 0.0f, vMax },
        { r, b, uMax, vMax },
    };

    D3D11_BUFFER_DESC bd{};
    bd.ByteWidth = sizeof(verts);
    bd.Usage = D3D11_USAGE_IMMUTABLE;
    bd.BindFlags = D3D11_BIND_VERTEX_BUFFER;
    D3D11_SUBRESOURCE_DATA init{};
    init.pSysMem = verts;

    m_quad = nullptr;
    if (FAILED(m_device->CreateBuffer(&bd, &init, m_quad.put()))) return;
    m_quadSrcW = desc.Width;  m_quadSrcH = desc.Height;
    m_quadDstW = sc.Width;    m_quadDstH = sc.Height;
    m_quadCodedW = codedW;    m_quadCodedH = codedH;
}

// The colour matrix and range offsets, premultiplied on the CPU.
//
// Straight from the moonlight-xbox `getFramePremultipliedCscConstants`, which
// is the point: this is the arithmetic that lets a FULL-range PQ stream be
// rendered correctly, and DXGI video-processor colour spaces cannot express it
// at all -- there is no full-range PQ YCbCr entry in DXGI_COLOR_SPACE_TYPE.
void VideoRenderer::EnsureCscConstants(D3D11_TEXTURE2D_DESC const& desc) noexcept {
    const FrameColor colour = m_frameColor;
    if (m_cscValid && m_cscFor.bitsPerChannel == colour.bitsPerChannel &&
        m_cscFor.fullRange == colour.fullRange && m_cscFor.matrix == colour.matrix &&
        m_cscSrcW == desc.Width && m_cscSrcH == desc.Height) {
        return;
    }

    // Full-range coefficients; the range scaling is folded in below.
    static const float kBt601[9] = {
        1.0f, 1.0f, 1.0f,
        0.0f, -0.3441f, 1.7720f,
        1.4020f, -0.7141f, 0.0f,
    };
    static const float kBt709[9] = {
        1.0f, 1.0f, 1.0f,
        0.0f, -0.1873f, 1.8556f,
        1.5748f, -0.4681f, 0.0f,
    };
    static const float kBt2020[9] = {
        1.0f, 1.0f, 1.0f,
        0.0f, -0.1646f, 1.8814f,
        1.4746f, -0.5714f, 0.0f,
    };

    const int bits = colour.bitsPerChannel;
    const int channelRange = 1 << bits;
    const double yMin  = colour.fullRange ? 0.0 : double(16 << (bits - 8));
    const double yMax  = colour.fullRange ? double(channelRange - 1) : double(235 << (bits - 8));
    const double uvMin = colour.fullRange ? 0.0 : double(16 << (bits - 8));
    const double uvMax = colour.fullRange ? double(channelRange - 1) : double(240 << (bits - 8));
    const double yScale  = (channelRange - 1) / (yMax - yMin);
    const double uvScale = (channelRange - 1) / (uvMax - uvMin);

    const float* base = colour.matrix == FrameColor::Matrix::Bt601  ? kBt601
                      : colour.matrix == FrameColor::Matrix::Bt2020 ? kBt2020
                                                                    : kBt709;
    float m[9];
    for (int i = 0; i < 9; ++i) m[i] = base[i];
    for (int i = 0; i < 3; ++i) m[i] = static_cast<float>(m[i] * yScale);
    for (int i = 3; i < 9; ++i) m[i] = static_cast<float>(m[i] * uvScale);

    CscConstants constants{};
    // Transposed into 3 rows of float3-plus-padding, which is how HLSL packs a
    // float3x3 in a constant buffer.
    for (int i = 0; i < 3; ++i) {
        for (int j = 0; j < 3; ++j) constants.cscMatrix[i * 4 + j] = m[j * 3 + i];
    }
    constants.offsets[0] = static_cast<float>(yMin / double(channelRange - 1));
    constants.offsets[1] = static_cast<float>((channelRange / 2) / double(channelRange - 1));
    constants.offsets[2] = constants.offsets[1];

    // Chroma siting. HEVC and H.264 both default to LEFT (MPEG-2 style) and
    // the host never says otherwise, so this is a constant rather than another
    // field on FrameColor. If a codec that cosites differently is ever added,
    // this is where AVFrame::chroma_location belongs.
    constants.chromaOffset[0] = 0.5f / desc.Width;
    constants.chromaOffset[1] = 0.0f;

    uint32_t codedW = m_sourceWidth.load(std::memory_order_relaxed);
    uint32_t codedH = m_sourceHeight.load(std::memory_order_relaxed);
    if (!codedW || !codedH || codedW > desc.Width || codedH > desc.Height) {
        codedW = desc.Width;
        codedH = desc.Height;
    }
    constants.chromaTexMax[0] = codedW != desc.Width
        ? static_cast<float>(codedW - 1) / desc.Width : 1.0f;
    constants.chromaTexMax[1] = codedH != desc.Height
        ? static_cast<float>(codedH - 1) / desc.Height : 1.0f;

    m_context->UpdateSubresource(m_cscBuffer.get(), 0, nullptr, &constants, 0, 0);
    m_cscFor = colour;
    m_cscSrcW = desc.Width;
    m_cscSrcH = desc.Height;
    m_cscValid = true;
}

// Tell the compositor what is in the back buffer.
//
// The shader writes PQ-encoded RGB for an HDR stream and Rec.709 RGB for an
// SDR one, and those are different colour spaces in the same buffer format.
// Declaring it is what makes the console convert instead of passing the values
// straight through -- the failure this whole path exists to end.
//
// Checked with CheckColorSpaceSupport first, exactly as moonlight-xbox does
// (Streaming/VideoRenderer.cpp:172): SetColorSpace1 fails hard on a swap chain
// that cannot honour the space, and losing the picture to a colour call would
// be a bad trade.
void VideoRenderer::ApplySwapChainColorSpace(bool pq) noexcept {
    const DXGI_COLOR_SPACE_TYPE want = pq ? DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                                          : DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;
    if (m_colorSpaceApplied == static_cast<int>(want)) return;

    auto swapChain3 = m_swapChain.try_as<IDXGISwapChain3>();
    if (!swapChain3) { m_colorSpaceState.store(static_cast<int>(ColorSpaceState::NoSwapChain3), std::memory_order_relaxed); return; }
    UINT support = 0;
    if (FAILED(swapChain3->CheckColorSpaceSupport(want, &support)) ||
        !(support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT)) {
        // Leave whatever is set. A console that will not present PQ is one
        // that should keep showing an SDR picture, not a black screen.
        //
        // RECORDED rather than swallowed, because this is the line that
        // explains a washed-out HDR stream: the host is sending PQ, the shader
        // is writing PQ, and the compositor was never told -- so it passes the
        // values through as if they were Rec.709. Refusal here means the
        // console's output is not in HDR, whatever the stream is.
        m_colorSpaceState.store(static_cast<int>(pq ? ColorSpaceState::PqRefused
                                                    : ColorSpaceState::SrgbRefused),
                                std::memory_order_relaxed);
        return;
    }
    if (SUCCEEDED(swapChain3->SetColorSpace1(want))) {
        m_colorSpaceApplied = static_cast<int>(want);
        m_colorSpaceState.store(static_cast<int>(pq ? ColorSpaceState::Pq
                                                    : ColorSpaceState::Srgb),
                                std::memory_order_relaxed);
    } else {
        m_colorSpaceState.store(static_cast<int>(ColorSpaceState::SetRefused),
                                std::memory_order_relaxed);
    }
}

bool VideoRenderer::Fail(RenderFailure stage, HRESULT hr) noexcept {
    int none = 0;
    if (m_renderFailure.compare_exchange_strong(none, static_cast<int>(stage),
                                                std::memory_order_relaxed)) {
        m_renderFailureHr.store(static_cast<uint32_t>(hr), std::memory_order_relaxed);
    }
    // Most of these stages fail for one reason in practice: the device under
    // them has already been removed. Asking costs nothing and turns "could not
    // create a texture" into the reason that actually matters.
    if (m_device && FAILED(m_device->GetDeviceRemovedReason())) NoteDeviceRemoved(hr);
    return false;
}

void VideoRenderer::NoteDeviceRemoved(HRESULT fallback) noexcept {
    HRESULT reason = m_device ? m_device->GetDeviceRemovedReason() : fallback;
    if (SUCCEEDED(reason)) reason = fallback;          // removed, but no detail
    if (SUCCEEDED(reason)) reason = DXGI_ERROR_DEVICE_REMOVED;
    uint32_t expected = 0;
    m_deviceRemovedReason.compare_exchange_strong(expected, static_cast<uint32_t>(reason),
                                                  std::memory_order_acq_rel);
}

std::wstring VideoRenderer::FailureReport() const {
    std::wstring out;
    wchar_t buf[200]{};
    if (const uint32_t removed = m_deviceRemovedReason.load(std::memory_order_acquire)) {
        // Named, because the hex alone sends someone to a search engine, and
        // the four answers point at completely different places.
        const wchar_t* why = L"no further detail";
        switch (removed) {
            case 0x887A0006u: why = L"GPU HUNG - a command took too long and the GPU was reset"; break;
            case 0x887A0007u: why = L"device RESET - a badly formed command"; break;
            case 0x887A0020u: why = L"DRIVER internal error"; break;
            case 0x887A0001u: why = L"INVALID CALL"; break;
            case 0x8007000Eu: why = L"OUT OF MEMORY"; break;
            case 0x887A0005u: why = L"removed, no further detail"; break;
        }
        swprintf_s(buf, L"GPU device lost: 0x%08X (%s) - rebuilt at the next stream start",
                   removed, why);
        out += buf;
    }
    if (const int stage = m_renderFailure.load(std::memory_order_relaxed)) {
        const wchar_t* what = L"unknown stage";
        switch (static_cast<RenderFailure>(stage)) {
            case RenderFailure::ShaderFile:    what = L"shader file missing/unreadable"; break;
            case RenderFailure::ShaderCreate:  what = L"shader rejected by the device"; break;
            case RenderFailure::PipelineState: what = L"input layout/sampler/constant buffer"; break;
            case RenderFailure::VideoTexture:  what = L"video copy texture"; break;
            case RenderFailure::PlaneViews:    what = L"plane views (NV12/P010 SRVs)"; break;
            case RenderFailure::Quad:          what = L"vertex buffer"; break;
            case RenderFailure::SwapChainDesc: what = L"swap chain GetDesc1"; break;
            default: break;
        }
        swprintf_s(buf, L"first render failure: %s (0x%08X)", what,
                   m_renderFailureHr.load(std::memory_order_relaxed));
        if (!out.empty()) out += L"\n";
        out += buf;
    }
    return out;
}

bool VideoRenderer::CanPresentPq() const noexcept {
    if (!m_swapChain) return false;
    auto swapChain3 = m_swapChain.try_as<IDXGISwapChain3>();
    if (!swapChain3) return false;
    UINT support = 0;
    return SUCCEEDED(swapChain3->CheckColorSpaceSupport(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                                                        &support)) &&
           (support & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT);
}

bool VideoRenderer::RenderFrame(ID3D11Texture2D* frame, uint32_t slice) noexcept {
    if (!EnsureShaderPipeline()) return false;

    D3D11_TEXTURE2D_DESC desc{};
    frame->GetDesc(&desc);

    ID3D11ShaderResourceView* planes[2]{};
    if (!EnsurePlaneViews(frame, slice, desc, planes)) return false;

    EnsureQuad(desc);
    if (!m_quad) return Fail(RenderFailure::Quad, E_FAIL);
    EnsureCscConstants(desc);
    ApplySwapChainColorSpace(m_frameColor.pq);

    DXGI_SWAP_CHAIN_DESC1 sc{};
    if (const HRESULT hr = m_swapChain->GetDesc1(&sc); FAILED(hr)) {
        return Fail(RenderFailure::SwapChainDesc, hr);
    }

    ID3D11RenderTargetView* rtv = m_backBufferView.get();
    m_context->OMSetRenderTargets(1, &rtv, nullptr);

    // Cleared every frame because the quad may not cover the buffer: the
    // letterbox bars would otherwise hold whatever was last presented, which
    // at a resolution change is the previous picture behind the new one.
    const float black[4] = { 0.0f, 0.0f, 0.0f, 1.0f };
    m_context->ClearRenderTargetView(rtv, black);

    D3D11_VIEWPORT vp{};
    vp.Width = static_cast<float>(sc.Width);
    vp.Height = static_cast<float>(sc.Height);
    vp.MaxDepth = 1.0f;
    m_context->RSSetViewports(1, &vp);

    const UINT stride = sizeof(float) * 4, offset = 0;
    ID3D11Buffer* vb = m_quad.get();
    m_context->IASetInputLayout(m_inputLayout.get());
    m_context->IASetVertexBuffers(0, 1, &vb, &stride, &offset);
    m_context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
    m_context->VSSetShader(m_vertexShader.get(), nullptr, 0);
    m_context->PSSetShader(m_pixelShader.get(), nullptr, 0);
    m_context->PSSetShaderResources(0, 2, planes);
    ID3D11SamplerState* sampler = m_sampler.get();
    m_context->PSSetSamplers(0, 1, &sampler);
    ID3D11Buffer* cb = m_cscBuffer.get();
    m_context->PSSetConstantBuffers(0, 1, &cb);

    m_context->Draw(4, 0);

    // Unbind: these views name a decoder surface the decoder will reuse, and
    // leaving them bound holds a reference into a slice it wants back.
    ID3D11ShaderResourceView* none[2]{};
    m_context->PSSetShaderResources(0, 2, none);
    return true;
}

void VideoRenderer::Resize(uint32_t panelWidth, uint32_t panelHeight,
                           float scaleX, float scaleY) noexcept {
    if (!m_swapChain) return;
    const uint32_t width  = static_cast<uint32_t>(std::lround(panelWidth  * scaleX));
    const uint32_t height = static_cast<uint32_t>(std::lround(panelHeight * scaleY));
    if (width == 0 || height == 0) return;

    std::lock_guard<std::mutex> guard(m_lock);
    m_pendingWidth  = width;
    m_pendingHeight = height;
    m_resizePending.store(true, std::memory_order_release);

    DXGI_MATRIX_3X2_F transform{};
    transform._11 = 1.0f / scaleX;
    transform._22 = 1.0f / scaleY;
    m_swapChain->SetMatrixTransform(&transform);
}

void VideoRenderer::IdleWait() noexcept {
    // A high-resolution waitable timer, not Sleep(1): the default Windows timer
    // tick is ~15.6 ms, which is coarser than a whole frame at 60 Hz and nearly
    // two at 120. Sleeping on it would add up to a frame of latency to every
    // picture that arrives just after a poll. `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`
    // gives sub-millisecond precision without changing the process-wide timer
    // resolution the way `timeBeginPeriod` does.
    if (!m_idleTimer) {
        m_idleTimer.attach(CreateWaitableTimerExW(
            nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS));
    }
    if (m_idleTimer) {
        LARGE_INTEGER due{};
        due.QuadPart = -10000;   // 1 ms, relative
        if (SetWaitableTimer(m_idleTimer.get(), &due, 0, nullptr, nullptr, FALSE)) {
            WaitForSingleObjectEx(m_idleTimer.get(), 5, FALSE);
            return;
        }
    }
    Sleep(1);   // the timer is unavailable; coarse, but never a hot spin
}

void VideoRenderer::PresentLoop() noexcept {
    while (m_running.load(std::memory_order_acquire)) {
        std::unique_lock<std::mutex> guard(m_lock);
        if (!m_swapChain) break;

        // ── Resize ──────────────────────────────────────────────────────────
        bool resized = false;
        if (m_resizePending.exchange(false, std::memory_order_acq_rel)) {
            m_backBufferView = nullptr;
            m_context->OMSetRenderTargets(0, nullptr, nullptr);
            m_context->Flush();
            if (SUCCEEDED(m_swapChain->ResizeBuffers(
                    0, m_pendingWidth, m_pendingHeight, DXGI_FORMAT_UNKNOWN,
                    SwapChainFlags()))) {
                CreateSwapChainSurfaces();
                // The back buffer changed size, so the letterbox quad no longer
                // matches it. The plane views are untouched: they name decoder
                // surfaces, which a swap-chain resize does not affect.
                m_quad = nullptr;
                resized = true;
            }
        }
        if (!m_backBufferView) { guard.unlock(); IdleWait(); continue; }

        // ── A decoded frame, if there is one ────────────────────────────────
        bool drew = false;
        {
            std::lock_guard<std::mutex> sourceGuard(m_sourceLock);
            if (m_frameSource) {
                com_ptr<ID3D11Texture2D> frame;
                uint32_t subresource = 0;
                if (m_frameSource(frame, subresource) && frame) {
                    drew = RenderFrame(frame.get(), subresource);
                    if (drew) m_blitted.fetch_add(1, std::memory_order_relaxed);
                }
            }
        }

        // ── Nothing new: HOLD the last picture ──────────────────────────────
        //
        // This is the whole fix for the idle flashing. The host throttles a
        // motionless desktop to a 200 ms keep-alive (Tier 0's
        // `static_duplicate_is_due`), so at 60 Hz roughly eleven of every
        // twelve passes through this loop have no new frame. Painting a
        // background on those and presenting it produced a picture that
        // strobed between the desktop and blue whenever nothing moved — and
        // stopped the instant the mouse did, which is exactly the reported
        // symptom.
        //
        // Not presenting leaves the previously presented buffer on screen,
        // which is precisely the "hold the last frame" behaviour wanted. The
        // background is painted only when there is genuinely nothing to hold:
        // before the first frame, and after an explicit request.
        if (!drew) {
            const bool nothingToHold =
                m_blitted.load(std::memory_order_relaxed) == 0 ||
                m_showBackground.load(std::memory_order_acquire) || resized;

            if (!nothingToHold) {
                guard.unlock();
                IdleWait();
                continue;
            }

            // A solid field, deliberately not the animated pulse the bring-up
            // build used: an animation here is indistinguishable from the
            // flashing bug it caused.
            //
            // Pure black, and this is THE background of the application.
            //
            // Not a figure of speech: the SwapChainPanel is stretched across the
            // whole window and sits above the Page's own fill, so whatever is
            // cleared here is what fills every pixel the chrome does not cover.
            // The Page's Background, the Frame's, and every theme brush behind
            // them are painted underneath it and never seen. A "the background
            // is not black" report is answered HERE first and in XAML second.
            //
            // It was 5/5/8 — the old Ion Void #050508 — which it was right to
            // match at the time. The palette is #000000 now for a reason that
            // makes this line load-bearing rather than cosmetic: a Mini-LED or
            // OLED panel switches its backlight OFF only for a genuinely black
            // pixel, and R5 G5 B8 keeps every dimming zone under the UI lit at
            // its floor. Being blue-dominant on top of that (B8 against R5) is
            // what a television's shadow handling then exaggerates into a
            // visibly blue-grey field.
            //
            // The format is B8G8R8A8_UNORM, not _SRGB, so these are written
            // straight through. Zero is zero under either encoding, so unlike
            // the old value this one survives a later move to an _SRGB swap
            // chain unchanged.
            // The colour space this background is presented in: PQ while an
            // HDR request is pending or wanted (see PreferPq), sRGB otherwise.
            // Render thread only, like every other ApplySwapChainColorSpace.
            ApplySwapChainColorSpace(m_pqPreferred.load(std::memory_order_acquire));

            const float clear[4] = { 0.0f, 0.0f, 0.0f, 1.0f };
            ID3D11RenderTargetView* rtv = m_backBufferView.get();
            m_context->OMSetRenderTargets(1, &rtv, nullptr);
            m_context->ClearRenderTargetView(rtv, clear);
            m_showBackground.store(false, std::memory_order_release);
        }

        // ── Present ─────────────────────────────────────────────────────────
        //
        // SyncInterval 0: show this frame now, do not wait for a vblank to
        // pass first. At SyncInterval 1 a present costs a refresh interval
        // whether or not the frame is late, so a picture that fell behind
        // could never catch up — it could only stay exactly as far behind as
        // the worst moment of the session. That, and not the network, is
        // where the lag-then-race came from.
        //
        // The waitable object stays, and is now the ONLY pacer: it blocks
        // until the compositor is ready for another frame, so at
        // MaximumFrameLatency 1 there is never more than one frame in flight
        // and the loop cannot spin. The 100 ms cap keeps a stalled compositor
        // from making this thread unkillable.
        if (m_frameLatencyWaitable) {
            WaitForSingleObjectEx(m_frameLatencyWaitable.get(), 100, FALSE);
        }
        if (!m_running.load(std::memory_order_acquire)) break;

        DXGI_PRESENT_PARAMETERS params{};
        // ALLOW_TEARING is only legal at SyncInterval 0, and only on a swap
        // chain created with the matching flag: passing it otherwise is an
        // invalid-call failure, not a silent ignore.
        const UINT interval = m_syncInterval.load(std::memory_order_relaxed);
        const UINT flags =
            (m_tearingSupported && interval == 0) ? DXGI_PRESENT_ALLOW_TEARING : 0u;
        const HRESULT hr = m_swapChain->Present1(interval, flags, &params);
        if (SUCCEEDED(hr)) {
            m_presented.fetch_add(1, std::memory_order_relaxed);
            continue;
        }

        // ── A failed present must never be fatal ─────────────────────────
        //
        // This used to `break`, and that made one refused present a permanently
        // blank screen: the thread exited, nothing restarted it, and the XAML
        // layer above kept drawing perfectly — so the app looked alive and the
        // video simply never appeared. A renderer that gives up silently is
        // worse than one that stutters.
        m_lastPresentHr.store(static_cast<uint32_t>(hr), std::memory_order_relaxed);

        if (hr == DXGI_ERROR_INVALID_CALL && interval == 0) {
            // A composition swap chain is presented by the compositor, and not
            // every configuration will accept "show this immediately" or a
            // tearing flag. Fall back to the compositor pace ONCE and keep
            // going: the frame dropping in the decoder is what removes the
            // backlog, and it works at either interval. This is the whole
            // reason the interval is a variable and not a literal.
            m_tearingSupported = false;
            m_syncInterval.store(1, std::memory_order_relaxed);
            continue;
        }
        if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET) {
            // The device is gone and nothing on THIS renderer can recover it.
            // Record why, so the owner can say so and build a new renderer at
            // the next stream start -- this used to end the thread silently,
            // and every later session in the app's life was blank with only
            // 0x887A0005 on the panel to explain it (live 2026-10-05).
            NoteDeviceRemoved(hr);
            break;
        }
        // Anything else: leave the last picture up and try again next pass.
        guard.unlock();
        IdleWait();
    }
}

void VideoRenderer::ShowBackground() noexcept {
    // Used when a session ends: without it the last frame of a dead stream
    // stays on the TV under the host picker, which reads as a frozen stream
    // rather than a finished one.
    m_showBackground.store(true, std::memory_order_release);
}

void VideoRenderer::Shutdown() noexcept {
    m_running.store(false, std::memory_order_release);
    if (m_presentThread.joinable()) m_presentThread.join();

    {
        std::lock_guard<std::mutex> guard(m_sourceLock);
        m_frameSource = nullptr;
    }
    std::lock_guard<std::mutex> guard(m_lock);
    m_planeViews.clear();
    m_videoTexture = nullptr;
    m_quad = nullptr;
    m_cscBuffer = nullptr;
    m_sampler = nullptr;
    m_inputLayout = nullptr;
    m_pixelShader = nullptr;
    m_vertexShader = nullptr;
    m_backBufferView = nullptr;
    m_swapChain = nullptr;
    m_frameLatencyWaitable.close();
    m_idleTimer.close();
    m_context = nullptr;
    m_device = nullptr;
}

}  // namespace echo
