// Last-rendered-frame capture, for the bug reporter.
//
// See snapshot.cpp for the design. The only thing shim.cpp needs from this
// header is NovaSnapshotOnFrame, which it calls once per encoded frame and
// which costs one relaxed atomic load when nothing is armed.
#pragma once

#include <d3d11.h>

// Called from EncodeFrame with the composite the encoder just consumed.
//
// Does nothing at all unless a snapshot has been armed, so the hot path pays a
// single atomic load. When armed, performs ONE GPU->CPU readback on the calling
// thread (which must be the thread that owns the immediate context) and
// disarms.
void NovaSnapshotOnFrame(ID3D11Device* device,
                         ID3D11DeviceContext* context,
                         ID3D11Texture2D* composite);
