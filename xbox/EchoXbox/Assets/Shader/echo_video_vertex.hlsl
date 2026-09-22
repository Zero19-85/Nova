// Fullscreen-quad vertex shader for the video path.
//
// The quad is not always fullscreen: the vertex buffer is built with the
// aspect-corrected destination rectangle already baked into clip space, so
// letterboxing costs nothing here and the shader stays this short. That is the
// same division of labour `moonlight-xbox` uses (Assets/Shader/d3d11_vertex.hlsl
// plus setupVertexBuffer), and it is why there is no transform matrix.
struct ShaderInput {
    float2 pos : POSITION;
    float2 tex : TEXCOORD0;
};

struct ShaderOutput {
    float4 pos : SV_POSITION;
    float2 tex : TEXCOORD0;
};

ShaderOutput main(ShaderInput input) {
    ShaderOutput output;
    output.pos = float4(input.pos, 0.0, 1.0);
    output.tex = input.tex;
    return output;
}
