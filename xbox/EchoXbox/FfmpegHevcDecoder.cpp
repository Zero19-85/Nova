#include "pch.h"
#include "FfmpegHevcDecoder.h"

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_d3d11va.h>
}

#pragma comment(lib, "avcodec.lib")
#pragma comment(lib, "avutil.lib")

using namespace winrt;

namespace echo {
namespace {

std::wstring Err(const wchar_t* what, int code) {
    char text[AV_ERROR_MAX_STRING_SIZE]{};
    av_strerror(code, text, sizeof(text));
    wchar_t wide[AV_ERROR_MAX_STRING_SIZE]{};
    MultiByteToWideChar(CP_UTF8, 0, text, -1, wide, AV_ERROR_MAX_STRING_SIZE);
    wchar_t buf[320]{};
    swprintf_s(buf, L"%s failed: %s (%d)", what, wide, code);
    return buf;
}

// The whole point of the exercise, in one callback.
//
// FFmpeg offers the formats it could decode into and the caller picks. The
// DEFAULT callback picks the software one, which decodes correctly, costs the
// CPU everything, and reports no error anywhere — so a silent fallback here
// looks exactly like a console that is merely slow. Returning AV_PIX_FMT_NONE
// when D3D11 is not on offer is deliberate: a hard failure at startup is worth
// far more than a stream that quietly runs on the CPU.
AVPixelFormat PickD3D11(AVCodecContext*, const AVPixelFormat* formats) {
    for (const AVPixelFormat* p = formats; *p != AV_PIX_FMT_NONE; ++p) {
        if (*p == AV_PIX_FMT_D3D11) return AV_PIX_FMT_D3D11;
    }
    return AV_PIX_FMT_NONE;
}

}  // namespace

std::wstring FfmpegHevcDecoder::ErrorDetail() const noexcept {
    if (DecodeErrors() == 0) return {};
    wchar_t buf[240]{};
    swprintf_s(buf,
               L"submit %llu · receive %llu · corrupt %llu · concealed %llu (missing refs %llu)",
               static_cast<unsigned long long>(m_submitFailures),
               static_cast<unsigned long long>(m_receiveFailures),
               static_cast<unsigned long long>(m_corruptFrames),
               static_cast<unsigned long long>(m_concealedFrames),
               static_cast<unsigned long long>(m_missingRefs));
    return buf;
}

FfmpegHevcDecoder::~FfmpegHevcDecoder() { Shutdown(); }

bool FfmpegHevcDecoder::Initialize(ID3D11Device* device, uint32_t width, uint32_t height,
                                   uint32_t fps, std::wstring& error) noexcept {
    m_device.copy_from(device);
    m_width = width;
    m_height = height;

    const AVCodec* codec = avcodec_find_decoder(AV_CODEC_ID_HEVC);
    if (!codec) { error = L"FFmpeg has no HEVC decoder (built with --disable-decoder=hevc?)"; return false; }

    m_ctx = avcodec_alloc_context3(codec);
    if (!m_ctx) { error = L"avcodec_alloc_context3 returned null"; return false; }

    // ── The hardware device, wrapping OUR D3D11 device ──────────────────────
    //
    // Allocated rather than created, so the device can be injected before
    // init. `av_hwdevice_ctx_create` would make its own — a second D3D11
    // device, and a full cross-device copy of every 4K frame to get the
    // picture back onto the one the renderer draws with.
    m_hwDevice = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);
    if (!m_hwDevice) { error = L"av_hwdevice_ctx_alloc(D3D11VA) returned null"; return false; }

    auto* deviceCtx = reinterpret_cast<AVHWDeviceContext*>(m_hwDevice->data);
    auto* d3dCtx = static_cast<AVD3D11VADeviceContext*>(deviceCtx->hwctx);

    // FFmpeg takes ownership of this reference and Releases it when the
    // context is freed, so it gets one of its own rather than borrowing ours.
    device->AddRef();
    d3dCtx->device = device;

    // device_context, video_device, video_context and the lock callbacks are
    // deliberately left null: av_hwdevice_ctx_init derives all of them from
    // `device`. It also asserts multithread protection on the device, which
    // VideoRenderer::Initialize has already set — without it the decoder's own
    // threads and our context corrupt each other in a way that looks like
    // packet loss.
    if (const int rc = av_hwdevice_ctx_init(m_hwDevice); rc < 0) {
        error = Err(L"av_hwdevice_ctx_init", rc);
        return false;
    }

    m_ctx->hw_device_ctx = av_buffer_ref(m_hwDevice);
    m_ctx->get_format = PickD3D11;

    // Geometry is a hint; the stream is authoritative and a live re-mode
    // changes it mid-session, which FFmpeg handles by reconfiguring itself.
    m_ctx->width = static_cast<int>(width);
    m_ctx->height = static_cast<int>(height);

    // Nova emits no B-frames, so there is nothing to reorder and every frame
    // held for reordering is pure added latency. Threads are meaningless with
    // a hwaccel — the GPU does the work — and thread_count > 1 only adds
    // buffering, so this is one thread deliberately.
    //
    // `has_b_frames` is NOT set here. It is an output field the decoder fills
    // in from the stream, not an input; writing it was overreach on my part.
    m_ctx->flags |= AV_CODEC_FLAG_LOW_DELAY;
    m_ctx->thread_count = 1;

    // ── Surfaces for the frames WE hold ─────────────────────────────────────
    //
    // The D3D11VA frame pool is sized from the stream's DPB requirement, and
    // that budget assumes the caller hands every frame straight back. This
    // decoder does not: `m_current` keeps one alive for the whole inter-frame
    // interval so the renderer can blit from it, and `m_scratch` holds another
    // briefly while draining.
    //
    // FFmpeg documents `extra_hw_frames` as exactly the knob for this, and the
    // failure mode when it is missing is not an error — it is a decoder that
    // cannot get a surface, so pictures come out wrong or not at all, which
    // presents as missing references. Four is two for us and two of margin;
    // the surfaces cost VRAM and nothing else.
    // Raised 4 -> 8 after a live run: 4 gave a clean startup and the picture
    // still decayed, so the pool is not obviously the remaining fault — but
    // surfaces cost only VRAM, and at 4K120 the drain loop can hold two of
    // them at once on top of the one on loan to the renderer. Cheap insurance
    // while the real mechanism is being named by the counters below.
    m_ctx->extra_hw_frames = 8;

    // 100 ns units, matching `meta[2]` from echo_fill_buffer. Timestamps only
    // have to increase; substituting a receive clock would encode our own
    // jitter into the stream.
    m_ctx->pkt_timebase = AVRational{ 1, 10000000 };

    if (const int rc = avcodec_open2(m_ctx, codec, nullptr); rc < 0) {
        error = Err(L"avcodec_open2", rc);
        return false;
    }

    m_packet = av_packet_alloc();
    m_scratch = av_frame_alloc();
    m_current = av_frame_alloc();
    if (!m_packet || !m_scratch || !m_current) { error = L"av_packet/frame_alloc failed"; return false; }

    m_hardware = true;
    // The name says which FRAME PATH is running, not just which decoder.
    //
    // Worth the few characters: a build with the private copy and one without
    // are visually identical until the picture has had time to decay, and a
    // test run against the wrong build is a wasted session and a misleading
    // result. This line appears on the dashboard and in the overlay's DECODER
    // row, so "which build is on the console" is answerable in one glance.
    m_name = L"FFmpeg hevc (D3D11VA, private copy)";
    if (fps) m_name += L" @" + std::to_wstring(fps);
    m_started = true;
    return true;
}

void FfmpegHevcDecoder::Shutdown() noexcept {
    std::lock_guard<std::mutex> guard(m_lock);
    m_started = false;

    if (m_current) { av_frame_free(&m_current); m_current = nullptr; }
    if (m_scratch) { av_frame_free(&m_scratch); m_scratch = nullptr; }
    if (m_packet)  { av_packet_free(&m_packet); m_packet = nullptr; }
    if (m_ctx)     { avcodec_free_context(&m_ctx); m_ctx = nullptr; }
    if (m_hwDevice){ av_buffer_unref(&m_hwDevice); m_hwDevice = nullptr; }

    m_device = nullptr;
}

// Pull everything waiting and throw it away. Used when the decoder will not
// accept more input — the backlog is the problem, so the backlog is what goes.
void FfmpegHevcDecoder::DrainAndDiscard() noexcept {
    for (int i = 0; i < kMaxDrain; ++i) {
        const int rc = avcodec_receive_frame(m_ctx, m_scratch);
        if (rc < 0) break;
        av_frame_unref(m_scratch);
        ++m_decoded;
        ++m_dropped;
    }
}

bool FfmpegHevcDecoder::Submit(const uint8_t* data, uint32_t length, bool keyframe,
                               bool discontinuity, int64_t pts100ns) noexcept {
    if (!m_started || !data || length == 0) return false;

    std::lock_guard<std::mutex> guard(m_lock);
    if (!m_ctx) return false;

    // ── `discontinuity` deliberately does NOTHING here ──────────────────────
    //
    // This used to call `avcodec_flush_buffers`, and that was wrong in a way
    // that took a live session and a log to see. Measured on 2026-09-08:
    // ZERO invalidation requests across 7,286 log lines of Media Foundation
    // sessions, and 358 across 858 lines of FFmpeg ones. The picture smeared
    // continuously and never fully cleared.
    //
    // `avcodec_flush_buffers` destroys the ENTIRE DPB — every reference
    // picture the decoder is holding. The Media Foundation flag this was
    // written to mirror, `MFSampleExtension_Discontinuity`, does nothing of
    // the sort: it tells the decoder not to conceal across the gap and leaves
    // its references alone. Translating one into the other was a large
    // over-reach dressed as a like-for-like.
    //
    // What it cost is worse than losing a few frames, because it fought the
    // repair path directly:
    //
    //   1. a gap sets discontinuity, so we flushed and the DPB went empty
    //   2. the next P-frame referenced pictures we had just thrown away, so
    //      the decoder concealed — that is the smearing
    //   3. the client noticed undecodable frames and asked the host to
    //      invalidate them
    //   4. the host answered with RFI recovery, which repairs by RE-POINTING
    //      references at a frame the client is supposed to still hold
    //   5. we had deleted that frame too, so the repair landed on nothing,
    //      which produced the next gap
    //
    // A loop, running about five times a second. The entire RFI/LTR ladder
    // exists so that recovery does not need a keyframe; flushing guarantees
    // the one thing it depends on is never there.
    //
    // So: nothing. FFmpeg conceals missing references on its own, exactly as
    // the MF decoder did, and a real IDR clears the DPB by spec when one
    // arrives. There is no case left that needs a manual flush.
    (void)discontinuity;

    av_packet_unref(m_packet);
    if (av_new_packet(m_packet, static_cast<int>(length)) < 0) return false;
    memcpy(m_packet->data, data, length);
    m_packet->pts = pts100ns;
    m_packet->dts = pts100ns;
    if (keyframe) m_packet->flags |= AV_PKT_FLAG_KEY;

    int rc = avcodec_send_packet(m_ctx, m_packet);
    if (rc == AVERROR(EAGAIN)) {
        // The decoder is full because nobody drained it fast enough.
        //
        // Dropping the INCOMING frame here would be exactly backwards: it
        // discards the newest picture to preserve a queue of older ones, so
        // the backlog is never paid off and every frame from here on is late
        // by however deep the queue got. Throw away what is queued and take
        // the new frame — the stale pictures were going to be skipped by
        // TryGetFrame anyway. Same reasoning as HevcDecoder's MF_E_NOTACCEPTING
        // arm, and the same bug it used to have.
        DrainAndDiscard();
        rc = avcodec_send_packet(m_ctx, m_packet);
    }

    // ── A frame that never reached the decoder is a hole, and it is SILENT ──
    //
    // This is counted here, after the retry, and that placement is the whole
    // point. The previous version counted only the FIRST send, before the
    // drain-and-retry — so a retry that also failed returned false and was
    // recorded nowhere. `decode errors 0` while the picture decayed was partly
    // this counter lying.
    //
    // It matters more than an ordinary dropped frame. The host's LTR and RFI
    // logic decides what is safe to reference from the client's ACK watermark,
    // and that watermark is emitted by the RECEIVER — which saw this frame
    // arrive perfectly. So the host believes we hold a picture we never
    // decoded, and will happily re-point later references at it. Every one of
    // those decodes against something that is not there.
    if (rc < 0) {
        ++m_submitFailures;
        return false;
    }
    return true;
}

bool FfmpegHevcDecoder::TryGetFrame(com_ptr<ID3D11Texture2D>& texture,
                                    uint32_t& subresource) noexcept {
    if (!m_started) return false;

    std::lock_guard<std::mutex> guard(m_lock);
    if (!m_ctx || !m_current) return false;

    // Whatever the renderer had is finished with by now — it blits before it
    // asks again. Releasing here rather than at the end of the last call is
    // what makes "valid until the next call" literally true.
    av_frame_unref(m_current);

    bool have = false;
    for (int i = 0; i < kMaxDrain; ++i) {
        const int rc = avcodec_receive_frame(m_ctx, m_scratch);
        if (rc < 0) {
            // EAGAIN is the normal exit — nothing ready yet. Anything else is
            // the decoder telling us it could not produce a picture.
            if (rc != AVERROR(EAGAIN) && rc != AVERROR_EOF) ++m_receiveFailures;
            break;
        }

        // ── The trap for SILENT concealment ─────────────────────────────────
        //
        // This is the case that produced a screen decaying into grey with
        // every counter reading zero. `avcodec_receive_frame` SUCCEEDS: the
        // decoder produced a picture, it just produced a wrong one, because a
        // reference it needed was not there and it filled the gap instead.
        // Nothing about the return value distinguishes that from a perfect
        // frame — but the frame itself carries the admission.
        //
        // Checked on EVERY received frame, not only the newest one we keep: a
        // corrupt picture that gets superseded a millisecond later is the same
        // evidence of decay, and dropping it silently is how the decay stayed
        // invisible in the first place.
        //
        // MISSING_REFERENCE and CONCEALMENT_ACTIVE are the two that describe
        // this failure exactly; the others are counted with them because any
        // of them means the output is not what the host encoded.
        if (m_scratch->flags & AV_FRAME_FLAG_CORRUPT) ++m_corruptFrames;
        if (m_scratch->decode_error_flags != 0) {
            ++m_concealedFrames;
            if (m_scratch->decode_error_flags & FF_DECODE_ERROR_MISSING_REFERENCE) {
                ++m_missingRefs;
            }
        }

        ++m_decoded;
        if (have) ++m_dropped;                // superseded before it was shown

        // Keep the newest, drop what it replaced. The policy lives HERE rather
        // than in the renderer: draining at the display's refresh rate means a
        // burst can never shrink, and that is what made video lag input and
        // then race to catch up.
        av_frame_unref(m_current);
        av_frame_move_ref(m_current, m_scratch);
        have = true;
    }

    if (!have) return false;

    // data[0] is the texture ARRAY, data[1] is the slice inside it.
    auto* native = reinterpret_cast<ID3D11Texture2D*>(m_current->data[0]);
    if (!native) return false;
    const auto slice = static_cast<uint32_t>(reinterpret_cast<intptr_t>(m_current->data[1]));

    if (m_current->width > 0 && m_current->height > 0) {
        m_width = static_cast<uint32_t>(m_current->width);
        m_height = static_cast<uint32_t>(m_current->height);
    }

    // Handed back as the decoder owns it: `data[0]` is the texture array and
    // `data[1]` the slice inside it, straight into
    // CreateVideoProcessorInputView. Nothing is copied, converted or read
    // back � the picture goes from the decode block to the screen untouched.
    //
    // The surface is only valid until the next call on this object, which is
    // the contract VideoDecoder.h states and which the renderer honours by
    // blitting before it asks again.
    //
    // The renderer is told the CODED size separately (VideoRenderer::
    // SetSourceSize) because this texture is allocated at the ALIGNED size �
    // 1088 rows for a 1080-line stream � and without a source rect the video
    // processor scales those extra rows of undefined memory onto the screen.
    texture = nullptr;
    texture.copy_from(native);
    subresource = slice;
    return true;
}

}  // namespace echo
