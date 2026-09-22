// YCbCr -> RGB for both NV12 (8-bit) and P010 (10-bit).
//
// One shader covers both because the PLANES are sampled through SRVs that
// normalise to [0,1] either way (R8/R8G8 for NV12, R16/R16G16 for P010), and
// everything that depends on bit depth lives in the constant buffer instead.
// That is what lets an HDR stream and an SDR stream share a pipeline state.
//
// Deliberately a straight matrix multiply with no tone mapping and no transfer
// function: for HDR the samples are already PQ-encoded and the SWAP CHAIN is
// told so (DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020), so re-encoding here
// would apply PQ twice. The shader's only job is the colour MATRIX and the
// range offset -- exactly the split `moonlight-xbox` uses.
Texture2D<min16float>  luminancePlane   : register(t0);
Texture2D<min16float2> chrominancePlane : register(t1);
SamplerState           theSampler       : register(s0);

struct ShaderInput {
    float4 pos : SV_POSITION;
    float2 tex : TEXCOORD0;
};

cbuffer CSC_CONST_BUF : register(b0) {
    min16float3x3 cscMatrix;
    min16float3   offsets;
    min16float2   chromaOffset;
    min16float2   chromaTexMax;
};

min16float4 main(ShaderInput input) : SV_TARGET {
    // The chroma clamp is not cosmetic. A decoder surface is allocated at the
    // ALIGNED size (1088 rows for 1080), so the rows past the coded picture
    // hold undefined memory; without the clamp the sampler's filtering reaches
    // into them along the bottom and right edges and smears whatever is there
    // across the last line of the image.
    min16float3 yuv = min16float3(
        luminancePlane.Sample(theSampler, input.tex),
        chrominancePlane.Sample(theSampler, min(input.tex + chromaOffset, chromaTexMax)));

    // Limited-range streams sit at 16/255; full-range ones at 0. Chroma is
    // always centred, so its offset is half scale regardless of range.
    yuv -= offsets;

    // Premultiplied on the CPU: the matrix already carries the range scaling,
    // so this is one multiply rather than a multiply and a divide per pixel.
    return min16float4(mul(yuv, cscMatrix), 1.0);
}
