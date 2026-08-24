// Local stream recording: remux, never re-encode.
//
// See recorder.cpp for the design and for why the container is Matroska. The
// surface here is four calls, three of them exported to Rust.
#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Codec of the bitstream that will be submitted. Matches the shim's own
// g_encoderCodec numbering (0 = H.264, 1 = HEVC, 2 = AV1).
#define NOVA_REC_CODEC_H264 0
#define NOVA_REC_CODEC_HEVC 1
#define NOVA_REC_CODEC_AV1  2

// Begin recording to `path` (UTF-16, as everything Win32 in this project is).
//
//   0   recording
//  -1   already recording
//  -2   the file could not be created
//  -3   the codec cannot be remuxed into this container (see recorder.cpp)
//  -4   bad arguments
//
// Nothing is written until the first keyframe arrives: a Matroska track needs
// the parameter sets, and those come out of the bitstream, not the caller.
__declspec(dllexport) int RecorderStart(const wchar_t* path, int codec, int width, int height,
                                        int fps, int is_hdr);

// Hand the recorder one encoded frame — the exact bytes NVENC produced.
//
// Non-blocking and safe to call from the encode hot path: it copies into a
// bounded queue and returns. A full queue DROPS the frame and says so, because
// the alternative is stalling capture for a disk, which trades a glitch in a
// recording for a stutter in the live stream.
//
// Returns 1 if the frame was queued, 0 if it was dropped or nothing is
// recording.
__declspec(dllexport) int RecorderSubmit(const uint8_t* data, int size, int is_keyframe,
                                         uint64_t frame_index);

// Finish the file and close it.
//
// Flushes the queue, patches the segment size and duration if the file is
// seekable, and closes. Safe to call when nothing is recording.
//
//   0   finalised
//  -1   nothing was recording
//  -2   closed, but the header could not be patched (the file is still playable)
__declspec(dllexport) int RecorderStop(void);

// Whether a recording is currently open. For the tray's menu label.
__declspec(dllexport) int RecorderIsActive(void);

// Frames dropped because the writer could not keep up, for the tray tooltip and
// the bug reporter's telemetry.
__declspec(dllexport) uint64_t RecorderDroppedFrames(void);

#ifdef __cplusplus
}
#endif
