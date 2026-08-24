// Record the stream to a file without encoding it again.
//
// Nova already has a compressed, correctly-paced, hardware-produced bitstream:
// it is the thing being sent to the client. Recording it is therefore a
// *container* problem, not a video problem. Every byte written here came
// straight out of NVENC; the only transformation is framing.
//
// The alternative — decode-and-re-encode, or a second NVENC session — costs a
// second encoder's worth of GPU on a machine that is also running a game, and
// produces a worse picture than the one already in hand. There is no version of
// that trade worth making.
//
// ## Why Matroska rather than MP4
//
// Both were on the table and MP4 is the more universally accepted file. It lost
// on the one property that matters most for this feature:
//
// **A Matroska file survives its own writer dying.** The Segment is written with
// unknown size, Clusters are self-contained, and every Cluster begins with a
// keyframe. A recording cut off by a crash, a power loss, or a bluescreen is
// playable right up to the last complete Cluster. An MP4's index (`moov`) is
// written at the *end*: the same crash leaves a file that no player will open at
// all.
//
// Since the whole point of local recording is to capture something that went
// wrong, a format that cannot survive things going wrong is the wrong format.
// [RecorderStop] still seeks back and patches the Segment size and Duration when
// it can, so a cleanly-finished file is a fully-indexed one — but that is an
// improvement on a file that was already valid, not a step it depends on.
//
// ## Why the writer is its own thread
//
// The submit path is called from the capture/encode thread, which owns the D3D11
// immediate context and is TIME_CRITICAL. A synchronous `WriteFile` there puts a
// disk — possibly a spinning one, possibly one a game is streaming textures off
// — inside the frame budget. That is exactly the mistake 15.4 removed when RTP
// send moved off this thread, and for the same reason it is not being
// re-introduced.
//
// So submit copies into a bounded queue and returns. When the queue is full the
// frame is DROPPED and counted. Dropping is the correct failure: a gap in a
// recording is a glitch in a file nobody is watching yet, while blocking is a
// visible stutter for the person actually streaming.
//
// ## What "remux" actually involves
//
// Two things, and only two:
//
// 1. **Annex-B to length-prefixed.** NVENC emits `00 00 00 01 <nal>` start
//    codes; Matroska (like MP4) wants each NAL preceded by its length. This is a
//    scan and a rewrite, no re-compression.
// 2. **Parameter sets into CodecPrivate.** A decoder opening the file needs
//    SPS/PPS (and VPS for HEVC) before the first frame, in the track header
//    rather than in the stream. They are lifted out of the first keyframe and
//    built into an `avcC`/`hvcC` record.
//
//    They are also left *in* the frame data. Strictly redundant, deliberately
//    kept: it is what makes each Cluster independently decodable, which is what
//    makes a truncated file recoverable. The cost is a few dozen bytes per
//    keyframe under an infinite GOP — nothing.
//
// ## AV1 is refused, not half-supported
//
// Nova's AV1 path emits low-overhead OBUs, not Annex-B, and a Matroska AV1 track
// needs an `AV1CodecConfigurationRecord` built from the sequence header rather
// than an `hvcC`. None of the code below applies to it. Refusing with a clear
// error beats writing a file that looks fine and plays as noise.

#include "recorder.h"

#include <windows.h>

#include <atomic>
#include <condition_variable>
#include <cstring>
#include <deque>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

extern void NovaShimLogExternal(const char* fmt, ...);

namespace {

// ── Tuning ──────────────────────────────────────────────────────────────────

// Frames the writer may fall behind before submissions start being dropped.
//
// Sized in frames rather than bytes because that is the unit the failure is felt
// in. At 120 fps this is half a second of slack, which absorbs the write stalls
// a filesystem produces on its own (a flush, an antivirus scan opening the file)
// without ever letting the backlog grow into the seconds of memory a
// "just make it bigger" bound would.
constexpr size_t kMaxQueuedFrames = 64;

// A new Cluster is opened at least this often, and additionally at every
// keyframe.
//
// Matroska's SimpleBlock timestamp is a SIGNED 16-BIT offset from its Cluster's
// timestamp, so a Cluster physically cannot span more than 32767 ms. This is
// well under that, and its real job is different: a Cluster is the unit a
// truncated file is recoverable to, so shorter Clusters mean less lost at the
// tail. Under Nova's infinite GOP keyframes are rare, so without this bound a
// Cluster could otherwise run for the whole recording.
constexpr uint64_t kMaxClusterMs = 5000;

// Matroska timestamps are in units of TimestampScale nanoseconds. 1 ms is the
// conventional choice and is plenty: it is finer than a frame at any rate Nova
// supports, and it keeps every timestamp comfortably inside the 16-bit
// SimpleBlock offset.
constexpr uint64_t kTimestampScaleNs = 1000000;

// ── EBML primitives ─────────────────────────────────────────────────────────

using Bytes = std::vector<uint8_t>;

void PutBE(Bytes& out, uint64_t value, int bytes) {
    for (int i = bytes - 1; i >= 0; --i) out.push_back((uint8_t)((value >> (i * 8)) & 0xFF));
}

// An EBML element ID is written as the literal bytes it is defined as — the
// leading-one marker is part of the ID, not a length to be recomputed.
void PutId(Bytes& out, uint32_t id) {
    if (id > 0x00FFFFFF) PutBE(out, id, 4);
    else if (id > 0x0000FFFF) PutBE(out, id, 3);
    else if (id > 0x000000FF) PutBE(out, id, 2);
    else out.push_back((uint8_t)id);
}

// EBML variable-length size: a leading-one marker picks the width, the value
// fills the rest.
void PutSize(Bytes& out, uint64_t size) {
    if (size < 0x7FULL) { out.push_back((uint8_t)(0x80 | size)); return; }
    if (size < 0x3FFFULL) { PutBE(out, 0x4000ULL | size, 2); return; }
    if (size < 0x1FFFFFULL) { PutBE(out, 0x200000ULL | size, 3); return; }
    if (size < 0x0FFFFFFFULL) { PutBE(out, 0x10000000ULL | size, 4); return; }
    if (size < 0x07FFFFFFFFULL) { PutBE(out, 0x0800000000ULL | size, 5); return; }
    if (size < 0x03FFFFFFFFFFULL) { PutBE(out, 0x040000000000ULL | size, 6); return; }
    if (size < 0x01FFFFFFFFFFFFULL) { PutBE(out, 0x02000000000000ULL | size, 7); return; }
    PutBE(out, 0x0100000000000000ULL | size, 8);
}

// A size written in a FIXED width, so it can be overwritten later without the
// element moving.
//
// This is what makes finalisation possible at all: the Segment's real size is
// unknown when its header is written, and a variable-width size would need a
// different number of bytes once it is known — which would mean shifting the
// entire file. Eight bytes, always, and patched in place at the end.
void PutSizeFixed8(Bytes& out, uint64_t size) {
    PutBE(out, 0x0100000000000000ULL | size, 8);
}

void PutUint(Bytes& out, uint32_t id, uint64_t value) {
    Bytes payload;
    int width = 1;
    while (width < 8 && (value >> (width * 8)) != 0) ++width;
    PutBE(payload, value, width);
    PutId(out, id);
    PutSize(out, payload.size());
    out.insert(out.end(), payload.begin(), payload.end());
}

void PutString(Bytes& out, uint32_t id, const char* text) {
    const size_t len = strlen(text);
    PutId(out, id);
    PutSize(out, len);
    out.insert(out.end(), text, text + len);
}

void PutBinary(Bytes& out, uint32_t id, const Bytes& data) {
    PutId(out, id);
    PutSize(out, data.size());
    out.insert(out.end(), data.begin(), data.end());
}

void PutFloat64(Bytes& out, uint32_t id, double value) {
    uint64_t bits;
    memcpy(&bits, &value, 8);
    PutId(out, id);
    PutSize(out, 8);
    PutBE(out, bits, 8);
}

// ── Annex-B ─────────────────────────────────────────────────────────────────

struct Nal {
    const uint8_t* data;
    size_t size;
    uint8_t type;  // already masked for the codec in question
};

// Split an Annex-B buffer into NALs.
//
// Tolerates both 3- and 4-byte start codes, and trailing zero bytes (encoders
// pad). Returns the NALs in order, pointing into `buffer` — nothing is copied.
std::vector<Nal> SplitAnnexB(const uint8_t* buffer, size_t size, bool hevc) {
    std::vector<Nal> nals;
    size_t i = 0;
    // Find the first start code. A buffer that does not begin with one is not
    // Annex-B — most likely it is sealed, or it is AV1 — and the caller has
    // already refused that case, so this simply finds nothing.
    while (i + 3 <= size) {
        if (buffer[i] == 0 && buffer[i + 1] == 0 &&
            (buffer[i + 2] == 1 || (i + 4 <= size && buffer[i + 2] == 0 && buffer[i + 3] == 1))) {
            break;
        }
        ++i;
    }
    while (i + 3 <= size) {
        const size_t skip = (buffer[i + 2] == 1) ? 3 : 4;
        const size_t start = i + skip;
        if (start >= size) break;

        // Scan to the next start code.
        size_t j = start;
        size_t end = size;
        while (j + 3 <= size) {
            if (buffer[j] == 0 && buffer[j + 1] == 0 &&
                (buffer[j + 2] == 1 ||
                 (j + 4 <= size && buffer[j + 2] == 0 && buffer[j + 3] == 1))) {
                end = j;
                break;
            }
            ++j;
        }
        if (j + 3 > size) end = size;

        // Trailing zeroes belong to the padding before the next start code, not
        // to this NAL.
        while (end > start && buffer[end - 1] == 0) --end;

        if (end > start) {
            Nal nal{};
            nal.data = buffer + start;
            nal.size = end - start;
            nal.type = hevc ? (uint8_t)((buffer[start] >> 1) & 0x3F) : (uint8_t)(buffer[start] & 0x1F);
            nals.push_back(nal);
        }
        if (end >= size) break;
        i = end;
    }
    return nals;
}

// Rewrite Annex-B as 4-byte-length-prefixed NALs.
void ToLengthPrefixed(const std::vector<Nal>& nals, Bytes& out) {
    size_t total = 0;
    for (const Nal& n : nals) total += 4 + n.size;
    out.clear();
    out.reserve(total);
    for (const Nal& n : nals) {
        PutBE(out, (uint64_t)n.size, 4);
        out.insert(out.end(), n.data, n.data + n.size);
    }
}

// Strip emulation-prevention bytes (00 00 03 -> 00 00).
//
// Needed for exactly one read in this file: the 12-byte profile_tier_level an
// hvcC copies out of the HEVC SPS. Those bytes include
// general_constraint_indicator_flags, which encoders very often leave as six
// zero bytes — a run that *forces* an emulation-prevention 0x03 into the middle
// of the field. Reading the escaped bytes directly would produce an hvcC with a
// plausible but wrong level, and players that trust it would refuse the file.
Bytes UnescapeRbsp(const uint8_t* data, size_t size, size_t limit) {
    Bytes out;
    out.reserve(limit);
    size_t zeros = 0;
    for (size_t i = 0; i < size && out.size() < limit; ++i) {
        if (zeros >= 2 && data[i] == 0x03) { zeros = 0; continue; }
        zeros = (data[i] == 0) ? zeros + 1 : 0;
        out.push_back(data[i]);
    }
    return out;
}

// ── Codec-private records ───────────────────────────────────────────────────

// ISO/IEC 14496-15 AVCDecoderConfigurationRecord.
bool BuildAvcC(const Bytes& sps, const Bytes& pps, Bytes& out) {
    if (sps.size() < 4 || pps.empty()) return false;
    out.clear();
    out.push_back(1);           // configurationVersion
    out.push_back(sps[1]);      // AVCProfileIndication
    out.push_back(sps[2]);      // profile_compatibility
    out.push_back(sps[3]);      // AVCLevelIndication
    out.push_back(0xFF);        // reserved(6) | lengthSizeMinusOne = 3
    out.push_back(0xE1);        // reserved(3) | numOfSequenceParameterSets = 1
    PutBE(out, sps.size(), 2);
    out.insert(out.end(), sps.begin(), sps.end());
    out.push_back(1);           // numOfPictureParameterSets
    PutBE(out, pps.size(), 2);
    out.insert(out.end(), pps.begin(), pps.end());
    return true;
}

void PutHevcArray(Bytes& out, uint8_t nal_type, const Bytes& nal) {
    // array_completeness = 1 (these are all the NALs of this type in the file),
    // reserved = 0, NAL_unit_type in the low 6 bits.
    out.push_back((uint8_t)(0x80 | (nal_type & 0x3F)));
    PutBE(out, 1, 2);           // numNalus
    PutBE(out, nal.size(), 2);
    out.insert(out.end(), nal.begin(), nal.end());
}

// ISO/IEC 14496-15 HEVCDecoderConfigurationRecord.
//
// The profile/tier/level block is copied verbatim out of the SPS rather than
// parsed field by field. It can be, because the SPS's layout up to that point is
// byte-aligned by construction: 2 bytes of NAL header, then
// sps_video_parameter_set_id(4) + sps_max_sub_layers_minus1(3) +
// sps_temporal_id_nesting_flag(1) = exactly one byte, then 12 bytes of
// profile_tier_level. No bit reader required — only the unescaping above.
bool BuildHvcC(const Bytes& vps, const Bytes& sps, const Bytes& pps, int bit_depth, Bytes& out) {
    if (vps.empty() || pps.empty()) return false;
    const Bytes raw = UnescapeRbsp(sps.data(), sps.size(), 3 + 12);
    if (raw.size() < 15) return false;
    const uint8_t* ptl = raw.data() + 3;

    out.clear();
    out.push_back(1);                       // configurationVersion
    out.push_back(ptl[0]);                  // profile_space | tier | profile_idc
    out.insert(out.end(), ptl + 1, ptl + 5);   // profile_compatibility_flags
    out.insert(out.end(), ptl + 5, ptl + 11);  // constraint_indicator_flags
    out.push_back(ptl[11]);                 // level_idc
    PutBE(out, 0xF000, 2);                  // reserved | min_spatial_segmentation_idc = 0
    out.push_back(0xFC);                    // reserved | parallelismType = 0 (unknown)
    out.push_back(0xFC | 0x01);             // reserved | chromaFormat = 1 (4:2:0)
    out.push_back((uint8_t)(0xF8 | ((bit_depth - 8) & 0x07)));  // bitDepthLumaMinus8
    out.push_back((uint8_t)(0xF8 | ((bit_depth - 8) & 0x07)));  // bitDepthChromaMinus8
    PutBE(out, 0, 2);                       // avgFrameRate = 0 (unspecified)
    // constantFrameRate = 0 | numTemporalLayers = 1 | temporalIdNested = 1 |
    // lengthSizeMinusOne = 3.
    out.push_back((uint8_t)((0 << 6) | (1 << 3) | (1 << 2) | 3));
    out.push_back(3);                       // numOfArrays: VPS, SPS, PPS
    PutHevcArray(out, 32, vps);
    PutHevcArray(out, 33, sps);
    PutHevcArray(out, 34, pps);
    return true;
}

// ── The recorder ────────────────────────────────────────────────────────────

struct QueuedFrame {
    Bytes data;          // raw Annex-B, exactly as NVENC produced it
    bool keyframe = false;
    uint64_t timestamp_ms = 0;
};

class Recorder {
public:
    int Start(const wchar_t* path, int codec, int width, int height, int fps, bool is_hdr) {
        std::lock_guard<std::mutex> guard(lock_);
        if (running_) return -1;
        if (!path || width <= 0 || height <= 0) return -4;
        if (codec != NOVA_REC_CODEC_H264 && codec != NOVA_REC_CODEC_HEVC) {
            NovaShimLogExternal(
                "[Rec] codec %d cannot be remuxed: this writer handles Annex-B H.264 and HEVC. "
                "AV1 needs an AV1CodecConfigurationRecord built from its sequence header, which "
                "is separate work — refusing rather than writing a file that will not play.\n",
                codec);
            return -3;
        }

        file_ = CreateFileW(path, GENERIC_WRITE, FILE_SHARE_READ, nullptr, CREATE_ALWAYS,
                            FILE_ATTRIBUTE_NORMAL, nullptr);
        if (file_ == INVALID_HANDLE_VALUE) {
            NovaShimLogExternal("[Rec] could not create the recording file (error %lu)\n",
                                GetLastError());
            return -2;
        }

        codec_ = codec;
        hevc_ = (codec == NOVA_REC_CODEC_HEVC);
        width_ = (uint32_t)width;
        height_ = (uint32_t)height;
        fps_ = fps > 0 ? (uint32_t)fps : 60;
        bit_depth_ = is_hdr ? 10 : 8;
        dropped_.store(0);
        header_written_ = false;
        cluster_open_ = false;
        base_ms_ = 0;
        have_base_ = false;
        last_ms_ = 0;
        bytes_written_ = 0;
        vps_.clear(); sps_.clear(); pps_.clear();
        running_ = true;
        stopping_ = false;

        writer_ = std::thread([this] { WriterLoop(); });
        NovaShimLogExternal("[Rec] recording %ux%u @%u %s%s — remux only, no re-encode\n", width_,
                            height_, fps_, hevc_ ? "HEVC" : "H.264", is_hdr ? " HDR" : "");
        return 0;
    }

    int Submit(const uint8_t* data, int size, bool keyframe, uint64_t frame_index) {
        if (!running_.load(std::memory_order_relaxed)) return 0;
        if (!data || size <= 0) return 0;

        QueuedFrame frame;
        // The timestamp is derived from the frame index and the negotiated
        // cadence rather than from a clock read.
        //
        // A wall-clock stamp taken here measures when the *encoder handed the
        // frame over*, which includes whatever jitter the capture loop had that
        // millisecond — so a recording of a perfectly paced stream would carry
        // the pacing noise of the machine that made it, and play back with it.
        // The index is what the stream itself says the cadence was.
        frame.timestamp_ms = (frame_index * 1000ULL) / (fps_ ? fps_ : 60);
        frame.keyframe = keyframe;
        frame.data.assign(data, data + size);

        {
            std::lock_guard<std::mutex> guard(queue_lock_);
            if (queue_.size() >= kMaxQueuedFrames) {
                const uint64_t n = dropped_.fetch_add(1) + 1;
                // Throttled: a disk that has stopped keeping up drops every
                // frame, and a log line per frame at 120 fps is its own problem.
                if (n == 1 || (n % 120) == 0) {
                    NovaShimLogExternal(
                        "[Rec] writer is behind — dropped frame %llu (%llu total). The recording "
                        "will have a gap; the live stream is unaffected, which is the trade.\n",
                        (unsigned long long)frame_index, (unsigned long long)n);
                }
                return 0;
            }
            queue_.push_back(std::move(frame));
        }
        queue_wake_.notify_one();
        return 1;
    }

    int Stop() {
        std::thread writer;
        {
            std::lock_guard<std::mutex> guard(lock_);
            if (!running_) return -1;
            running_ = false;
            stopping_ = true;
            writer = std::move(writer_);
        }
        queue_wake_.notify_all();
        if (writer.joinable()) writer.join();

        // Everything below runs with no writer thread alive, so no locking is
        // needed and the file handle is ours alone.
        int rc = 0;
        if (cluster_open_) { FinishCluster(); }
        if (header_written_) {
            if (!PatchHeader()) rc = -2;
        } else {
            NovaShimLogExternal(
                "[Rec] stopped before any keyframe arrived — the file has no track and is being "
                "left empty rather than half-written\n");
        }
        if (file_ != INVALID_HANDLE_VALUE) {
            FlushFileBuffers(file_);
            CloseHandle(file_);
            file_ = INVALID_HANDLE_VALUE;
        }
        NovaShimLogExternal("[Rec] finalised: %llu bytes, %llu dropped frame(s)\n",
                            (unsigned long long)bytes_written_,
                            (unsigned long long)dropped_.load());
        return rc;
    }

    bool Active() const { return running_.load(std::memory_order_relaxed); }
    uint64_t Dropped() const { return dropped_.load(std::memory_order_relaxed); }

private:
    void WriterLoop() {
        for (;;) {
            QueuedFrame frame;
            {
                std::unique_lock<std::mutex> guard(queue_lock_);
                queue_wake_.wait(guard, [this] { return !queue_.empty() || stopping_; });
                if (queue_.empty()) {
                    if (stopping_) return;
                    continue;
                }
                frame = std::move(queue_.front());
                queue_.pop_front();
            }
            WriteFrame(frame);
        }
    }

    void Write(const Bytes& bytes) {
        if (file_ == INVALID_HANDLE_VALUE || bytes.empty()) return;
        DWORD written = 0;
        if (!WriteFile(file_, bytes.data(), (DWORD)bytes.size(), &written, nullptr)) {
            NovaShimLogExternal("[Rec] write failed (error %lu) — recording abandoned\n",
                                GetLastError());
            running_.store(false);
            return;
        }
        bytes_written_ += written;
    }

    void WriteFrame(const QueuedFrame& frame) {
        const std::vector<Nal> nals = SplitAnnexB(frame.data.data(), frame.data.size(), hevc_);
        if (nals.empty()) {
            NovaShimLogExternal(
                "[Rec] a submitted frame contained no Annex-B start code. Sealed or otherwise "
                "non-elementary bytes reached the recorder; abandoning rather than writing "
                "garbage.\n");
            running_.store(false);
            return;
        }

        if (!header_written_) {
            CollectParameterSets(nals);
            if (!WriteHeader()) return;   // still waiting for the parameter sets
        }

        // The caller's keyframe flag is a hint; the bitstream is the authority.
        //
        // The shim's flag records that an IDR was *requested*, which covers every
        // keyframe Nova produces under its infinite GOP — but "every keyframe the
        // host currently asks for" is a policy, and a recording that mislabels one
        // is a recording that cannot be seeked. Reading the NAL types costs a
        // walk over a list already built.
        bool keyframe = frame.keyframe;
        for (const Nal& n : nals) {
            // H.264: IDR slice. HEVC: any IRAP (BLA/IDR/CRA, types 16..23).
            if (hevc_ ? (n.type >= 16 && n.type <= 23) : (n.type == 5)) { keyframe = true; break; }
        }

        if (!have_base_) { base_ms_ = frame.timestamp_ms; have_base_ = true; }
        const uint64_t ts = frame.timestamp_ms >= base_ms_ ? frame.timestamp_ms - base_ms_ : 0;
        last_ms_ = ts;

        // A Cluster starts at every keyframe (so it is independently decodable)
        // and whenever one has run long enough.
        if (!cluster_open_ || keyframe || (ts - cluster_ts_) >= kMaxClusterMs) {
            if (cluster_open_) FinishCluster();
            StartCluster(ts);
        }

        Bytes payload;
        ToLengthPrefixed(nals, payload);

        Bytes block;
        PutSize(block, 1);                          // track number, as a vint
        const int16_t rel = (int16_t)((int64_t)ts - (int64_t)cluster_ts_);
        block.push_back((uint8_t)((rel >> 8) & 0xFF));
        block.push_back((uint8_t)(rel & 0xFF));
        block.push_back(keyframe ? 0x80 : 0x00);  // keyframe flag, no lacing
        block.insert(block.end(), payload.begin(), payload.end());

        Bytes element;
        PutBinary(element, 0xA3, block);            // SimpleBlock
        cluster_body_.insert(cluster_body_.end(), element.begin(), element.end());
    }

    void CollectParameterSets(const std::vector<Nal>& nals) {
        for (const Nal& n : nals) {
            if (hevc_) {
                if (n.type == 32 && vps_.empty()) vps_.assign(n.data, n.data + n.size);
                if (n.type == 33 && sps_.empty()) sps_.assign(n.data, n.data + n.size);
                if (n.type == 34 && pps_.empty()) pps_.assign(n.data, n.data + n.size);
            } else {
                if (n.type == 7 && sps_.empty()) sps_.assign(n.data, n.data + n.size);
                if (n.type == 8 && pps_.empty()) pps_.assign(n.data, n.data + n.size);
            }
        }
    }

    // Write the EBML header, Segment header, Info and Tracks.
    //
    // Deferred until the parameter sets are in hand, because a Matroska track
    // without CodecPrivate is a track no decoder can open. Nova forces an IDR
    // with OUTPUT_SPSPPS at session start and on every repair, so in practice
    // this succeeds on the first frame submitted.
    bool WriteHeader() {
        if (sps_.empty() || pps_.empty() || (hevc_ && vps_.empty())) return false;

        Bytes codec_private;
        const bool built = hevc_ ? BuildHvcC(vps_, sps_, pps_, bit_depth_, codec_private)
                                 : BuildAvcC(sps_, pps_, codec_private);
        if (!built) {
            NovaShimLogExternal("[Rec] the stream's parameter sets could not be turned into a "
                                "decoder configuration record — abandoning\n");
            running_.store(false);
            return false;
        }

        Bytes out;

        // EBML header.
        Bytes ebml;
        PutUint(ebml, 0x4286, 1);          // EBMLVersion
        PutUint(ebml, 0x42F7, 1);          // EBMLReadVersion
        PutUint(ebml, 0x42F2, 4);          // EBMLMaxIDLength
        PutUint(ebml, 0x42F3, 8);          // EBMLMaxSizeLength
        PutString(ebml, 0x4282, "matroska");
        PutUint(ebml, 0x4287, 4);          // DocTypeVersion
        PutUint(ebml, 0x4285, 2);          // DocTypeReadVersion
        PutBinary(out, 0x1A45DFA3, ebml);

        // Segment, with a fixed-width size patched at the end. Until then it
        // reads as "unknown", which is what makes a crashed recording playable.
        PutId(out, 0x18538067);
        segment_size_offset_ = out.size();
        PutSizeFixed8(out, 0x00FFFFFFFFFFFFULL);  // unknown-size marker
        segment_body_offset_ = out.size();

        // Info. Duration is written as a placeholder and patched at Stop, so its
        // exact byte position has to be remembered — `PutFloat64` emits a 2-byte
        // id and a 1-byte size ahead of the 8 payload bytes.
        Bytes info;
        PutUint(info, 0x2AD7B1, kTimestampScaleNs);   // TimestampScale
        PutString(info, 0x4D80, "Nova");               // MuxingApp
        PutString(info, 0x5741, "Nova recorder");      // WritingApp
        const size_t duration_payload_in_info = info.size() + 3;
        PutFloat64(info, 0x4489, 0.0);                 // Duration, patched at Stop
        PutBinary(out, 0x1549A966, info);
        // Derived by subtraction rather than by predicting the id and size
        // widths: `PutBinary` has just appended exactly `info.size()` payload
        // bytes at the end of `out`, so the payload starts there.
        const size_t info_payload_start = out.size() - info.size();

        // Tracks.
        Bytes video;
        PutUint(video, 0xB0, width_);      // PixelWidth
        PutUint(video, 0xBA, height_);     // PixelHeight

        Bytes track;
        PutUint(track, 0xD7, 1);           // TrackNumber
        PutUint(track, 0x73C5, 1);         // TrackUID
        PutUint(track, 0x83, 1);           // TrackType: video
        PutUint(track, 0x9C, 0);           // FlagLacing: off
        PutUint(track, 0x23E383, 1000000000ULL / (fps_ ? fps_ : 60));  // DefaultDuration (ns)
        PutString(track, 0x86, hevc_ ? "V_MPEGH/ISO/HEVC" : "V_MPEG4/ISO/AVC");
        PutBinary(track, 0x63A2, codec_private);
        PutBinary(track, 0xE0, video);

        Bytes tracks;
        PutBinary(tracks, 0xAE, track);
        PutBinary(out, 0x1654AE6B, tracks);

        // The header is the first thing written, so `bytes_written_` is the file
        // position `out` will land at. Offsets recorded here are absolute file
        // positions, which is what SetFilePointerEx wants at Stop.
        const uint64_t base = bytes_written_;
        segment_size_offset_ += base;
        segment_body_offset_ += base;
        duration_offset_ = base + info_payload_start + duration_payload_in_info;

        Write(out);
        header_written_ = true;
        return true;
    }

    void StartCluster(uint64_t ts) {
        cluster_ts_ = ts;
        cluster_body_.clear();
        PutUint(cluster_body_, 0xE7, ts);   // Cluster Timestamp
        cluster_open_ = true;
    }

    void FinishCluster() {
        if (!cluster_open_) return;
        Bytes element;
        PutBinary(element, 0x1F43B675, cluster_body_);
        Write(element);
        cluster_body_.clear();
        cluster_open_ = false;
    }

    // Seek back and fill in what could not be known while writing.
    //
    // Best-effort by design: a failure here leaves a file that is still valid
    // and still plays, because the Segment size stays at the unknown-size
    // marker and the Duration stays zero. Players handle both — they simply have
    // to scan for the end rather than being told where it is.
    bool PatchHeader() {
        if (file_ == INVALID_HANDLE_VALUE) return false;
        bool ok = true;

        const uint64_t segment_size = bytes_written_ - segment_body_offset_;
        Bytes size_bytes;
        PutSizeFixed8(size_bytes, segment_size);
        ok &= WriteAt(segment_size_offset_, size_bytes);

        Bytes duration;
        const double value = (double)(last_ms_ + (1000.0 / (fps_ ? fps_ : 60)));
        uint64_t bits;
        const double as_scale = value;   // Duration is in TimestampScale units (ms here)
        memcpy(&bits, &as_scale, 8);
        PutBE(duration, bits, 8);
        ok &= WriteAt(duration_offset_, duration);

        if (!ok) {
            NovaShimLogExternal("[Rec] the file could not be seeked to patch its header — it is "
                                "still playable, but a player will have to scan it\n");
        }
        return ok;
    }

    bool WriteAt(uint64_t offset, const Bytes& bytes) {
        LARGE_INTEGER pos;
        pos.QuadPart = (LONGLONG)offset;
        if (!SetFilePointerEx(file_, pos, nullptr, FILE_BEGIN)) return false;
        DWORD written = 0;
        if (!WriteFile(file_, bytes.data(), (DWORD)bytes.size(), &written, nullptr)) return false;
        return written == bytes.size();
    }

    // Lifecycle
    std::mutex lock_;
    std::atomic<bool> running_{false};
    bool stopping_ = false;
    std::thread writer_;
    HANDLE file_ = INVALID_HANDLE_VALUE;

    // Queue
    std::mutex queue_lock_;
    std::condition_variable queue_wake_;
    std::deque<QueuedFrame> queue_;
    std::atomic<uint64_t> dropped_{0};

    // Stream description
    int codec_ = NOVA_REC_CODEC_HEVC;
    bool hevc_ = true;
    uint32_t width_ = 0, height_ = 0, fps_ = 60;
    int bit_depth_ = 8;
    Bytes vps_, sps_, pps_;

    // File state — touched only by the writer thread, or by Stop with the
    // writer joined.
    bool header_written_ = false;
    uint64_t bytes_written_ = 0;
    uint64_t segment_size_offset_ = 0;
    uint64_t segment_body_offset_ = 0;
    uint64_t duration_offset_ = 0;
    bool cluster_open_ = false;
    Bytes cluster_body_;
    uint64_t cluster_ts_ = 0;
    uint64_t base_ms_ = 0;
    bool have_base_ = false;
    uint64_t last_ms_ = 0;
};

Recorder g_recorder;

}  // namespace

extern "C" {

__declspec(dllexport) int RecorderStart(const wchar_t* path, int codec, int width, int height,
                                        int fps, int is_hdr) {
    return g_recorder.Start(path, codec, width, height, fps, is_hdr != 0);
}

__declspec(dllexport) int RecorderSubmit(const uint8_t* data, int size, int is_keyframe,
                                         uint64_t frame_index) {
    return g_recorder.Submit(data, size, is_keyframe != 0, frame_index);
}

__declspec(dllexport) int RecorderStop(void) { return g_recorder.Stop(); }

__declspec(dllexport) int RecorderIsActive(void) { return g_recorder.Active() ? 1 : 0; }

__declspec(dllexport) uint64_t RecorderDroppedFrames(void) { return g_recorder.Dropped(); }

}  // extern "C"
