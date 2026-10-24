
// HdrCast desktop -> P010 / NV12 conversion (design.md section 2.3). Generated from ConvertShaderSource.cs.
Texture2D<float4> srcTex     : register(t0);   // duplicated desktop: R16G16B16A16_FLOAT scRGB linear or B8G8R8A8_UNORM sRGB
Texture2D<float4> cursorTex  : register(t1);   // BGRA8 straight-alpha cursor; a == 0 && r > 0.5 means 'invert desktop'
SamplerState linearSampler   : register(s0);
RWByteAddressBuffer dst      : register(u0);   // full raw frame: Y plane then interleaved UV plane
// Zero copy (design.md section 2.8): the planes of the encoder's P010 / NV12 texture instead of the raw frame.
RWTexture2D<uint>  dstLuma   : register(u1);   // Y plane: R16_UINT view (p010, value << 6) or R8_UINT (nv12)
RWTexture2D<uint2> dstChroma : register(u2);   // interleaved UV plane, half size: R16G16_UINT or R8G8_UINT

cbuffer Params : register(b0)
{
    uint  srcWidth;   uint srcHeight;    // duplicated texture size (unrotated framebuffer)
    uint  deskWidth;  uint deskHeight;   // desktop size as seen by the user (rotated)
    uint  dstWidth;   uint dstHeight;    // output frame size (even)
    uint  rotation;                      // 0 identity, 1 = 90, 2 = 180, 3 = 270 (DXGI_MODE_ROTATION - 1)
    uint  flags;                         // FLAG_* bits
    float sdrWhite;                      // nits represented by scRGB 1.0 (default 80)
    int   cursorX;    int cursorY;       // cursor top-left (hotspot already subtracted), desktop pixels
    uint  cursorW;    uint cursorH;
    float pad0; float pad1; float pad2;
};

static const uint FLAG_CURSOR = 1;
static const uint FLAG_SRC_LINEAR = 2;
static const uint FLAG_EXACT = 4;

// Constants match OBS color.effect / zscale(npl=80).
static const float3x3 M709to2020 = {
    0.6274039, 0.3292830, 0.0433131,
    0.0690973, 0.9195404, 0.0113623,
    0.0163914, 0.0880133, 0.8955953 };

float pq_oetf(float x)   // x in units of 10000 nit
{
    const float m1 = 0.1593017578125, m2 = 78.84375, c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    float p = pow(max(x, 0.0), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

float3 srgb_to_linear(float3 c)
{
    return (c <= 0.04045) ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

float3 linear_to_srgb(float3 c)
{
    c = saturate(c);
    return (c <= 0.0031308) ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055;
}

// Desktop-space normalized coordinate -> source texture coordinate (undo the display rotation).
float2 desk_to_src(float2 d)
{
    if (rotation == 1) return float2(d.y, 1.0 - d.x);
    if (rotation == 2) return float2(1.0 - d.x, 1.0 - d.y);
    if (rotation == 3) return float2(1.0 - d.y, d.x);
    return d;
}

// Raw source texel for output pixel (x, y).
float4 fetch_src(uint x, uint y)
{
    if (flags & FLAG_EXACT) return srcTex.Load(int3(x, y, 0));
    float2 d = (float2(x, y) + 0.5) / float2(dstWidth, dstHeight);
    return srcTex.SampleLevel(linearSampler, desk_to_src(d), 0);
}

// Cursor texel covering output pixel (x, y); alpha 0 when outside the cursor rectangle.
float4 fetch_cursor(uint x, uint y)
{
    if (!(flags & FLAG_CURSOR)) return float4(0, 0, 0, 0);
    float2 dp = (float2(x, y) + 0.5) * float2(deskWidth, deskHeight) / float2(dstWidth, dstHeight);
    float2 cu = (dp - float2(cursorX, cursorY)) / float2(cursorW, cursorH);
    if (any(cu < 0.0) || any(cu >= 1.0)) return float4(0, 0, 0, 0);
    return cursorTex.SampleLevel(linearSampler, cu, 0);
}

// Pixel in linear scRGB (1.0 = sdrWhite nit) with the cursor composited - the HDR (P010) working space.
float3 pixel_linear(uint x, uint y)
{
    float4 s = fetch_src(x, y);
    float3 c = (flags & FLAG_SRC_LINEAR) ? s.rgb : srgb_to_linear(s.rgb);   // SDR desktop forced to p010: decode sRGB
    float4 k = fetch_cursor(x, y);
    if (k.a <= 0.0)
    {
        if (k.r > 0.5) c = 1.0 - saturate(c);   // monochrome/masked XOR cursor: invert (SDR-range approximation)
        return c;
    }
    float3 kl = srgb_to_linear(k.rgb) * (80.0 / sdrWhite);   // cursor white = SDR white (80 nit)
    return lerp(c, kl, k.a);
}

// Pixel in sRGB-encoded [0,1] with the cursor composited - the SDR (NV12) working space.
float3 pixel_srgb(uint x, uint y)
{
    float4 s = fetch_src(x, y);
    // HDR desktop forced to nv12 (low-priority path): treat scRGB as sRGB after clamping to [0,1],
    // i.e. everything above SDR white is clipped and the linear value is gamma-encoded.
    float3 c = (flags & FLAG_SRC_LINEAR) ? linear_to_srgb(s.rgb) : s.rgb;
    float4 k = fetch_cursor(x, y);
    if (k.a <= 0.0)
    {
        if (k.r > 0.5) c = 1.0 - saturate(c);
        return c;
    }
    return lerp(c, k.rgb, k.a);
}

float3 encode_pq2020(float3 lin)
{
    float3 lin2020 = max(mul(M709to2020, lin), 0.0);   // negative = outside BT.2020 -> clip
    return saturate(float3(pq_oetf(lin2020.r * sdrWhite / 10000.0),
                           pq_oetf(lin2020.g * sdrWhite / 10000.0),
                           pq_oetf(lin2020.b * sdrWhite / 10000.0)));
}

uint luma10(float3 E)
{
    float Y = 0.2627 * E.r + 0.6780 * E.g + 0.0593 * E.b;
    return (uint)round(64.0 + 876.0 * Y);
}

// The 2x2 block at (x, y) in HDR10: four 10-bit lumas (x,y / x+1,y / x,y+1 / x+1,y+1) and one Cb/Cr pair.
void p010_block(uint x, uint y, out uint4 luma, out uint2 chroma)
{
    float3 E0 = encode_pq2020(pixel_linear(x,     y));
    float3 E1 = encode_pq2020(pixel_linear(x + 1, y));
    float3 E2 = encode_pq2020(pixel_linear(x,     y + 1));
    float3 E3 = encode_pq2020(pixel_linear(x + 1, y + 1));
    luma = uint4(luma10(E0), luma10(E1), luma10(E2), luma10(E3));
    float3 Ea = (E0 + E1 + E2 + E3) * 0.25;             // chroma from the average of the 4 PQ-encoded pixels
    float Ya = 0.2627 * Ea.r + 0.6780 * Ea.g + 0.0593 * Ea.b;
    float Cb = (Ea.b - Ya) / 1.8814;
    float Cr = (Ea.r - Ya) / 1.4746;
    chroma = uint2((uint)round(512.0 + 896.0 * Cb), (uint)round(512.0 + 896.0 * Cr));
}

[numthreads(8, 8, 1)]
void main_p010(uint3 id : SV_DispatchThreadID)
{
    uint x = id.x * 2, y = id.y * 2;
    if (x >= dstWidth || y >= dstHeight) return;
    uint4 l; uint2 c;
    p010_block(x, y, l, c);
    uint rowBytes = dstWidth * 2;
    dst.Store(y * rowBytes + x * 2,       (l.x << 6) | ((l.y << 6) << 16));   // p010le: 10 bits in the high bits
    dst.Store((y + 1) * rowBytes + x * 2, (l.z << 6) | ((l.w << 6) << 16));
    uint uvBase = dstWidth * dstHeight * 2;
    dst.Store(uvBase + (y >> 1) * rowBytes + x * 2, (c.x << 6) | ((c.y << 6) << 16));
}

// Zero copy: the same block, written into the planes of a P010 texture.
[numthreads(8, 8, 1)]
void main_p010_planes(uint3 id : SV_DispatchThreadID)
{
    uint x = id.x * 2, y = id.y * 2;
    if (x >= dstWidth || y >= dstHeight) return;
    uint4 l; uint2 c;
    p010_block(x, y, l, c);
    dstLuma[uint2(x, y)]         = l.x << 6;
    dstLuma[uint2(x + 1, y)]     = l.y << 6;
    dstLuma[uint2(x, y + 1)]     = l.z << 6;
    dstLuma[uint2(x + 1, y + 1)] = l.w << 6;
    dstChroma[id.xy] = c << 6;
}

uint luma8(float3 c)
{
    float Y = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    return (uint)round(16.0 + 219.0 * Y);
}

// 8-bit Cb/Cr of the average c of a 2x2 block (sRGB-encoded, BT.709).
uint2 chroma8(float3 c)
{
    float Y = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    return uint2((uint)round(128.0 + 224.0 * (c.b - Y) / 1.8556), (uint)round(128.0 + 224.0 * (c.r - Y) / 1.5748));
}

[numthreads(64, 1, 1)]
void main_nv12(uint3 id : SV_DispatchThreadID)
{
    uint t = id.y * (64u * 1024u) + id.x;              // linear thread id (2-D dispatch of 1024 groups per row)
    uint ySlots  = (dstWidth * dstHeight) >> 2;        // uints in the Y plane
    // Round up: with W and H both even but not multiples of 4 (1366x766, say) the UV plane ends on a half
    // uint, and truncating would leave the last chroma pair of the frame unwritten.
    uint uvSlots = ((dstWidth * dstHeight) + 7u) >> 3;
    if (t < ySlots)
    {
        uint i0 = t * 4;
        uint v = 0;
        [unroll] for (uint j = 0; j < 4; j++)
        {
            uint i = i0 + j;
            v |= luma8(pixel_srgb(i % dstWidth, i / dstWidth)) << (8 * j);
        }
        dst.Store(t * 4, v);
    }
    else if (t < ySlots + uvSlots)
    {
        uint m0 = (t - ySlots) * 2;
        uint cw = dstWidth >> 1;
        uint chromaPairs = (dstWidth * dstHeight) >> 2;    // (W/2) * (H/2)
        uint v = 0;
        [unroll] for (uint k = 0; k < 2; k++)
        {
            uint m = m0 + k;
            if (m >= chromaPairs) break;                   // last, partially used uint of the plane
            uint cx = (m % cw) * 2, cy = (m / cw) * 2;
            uint2 cbcr = chroma8((pixel_srgb(cx, cy) + pixel_srgb(cx + 1, cy) + pixel_srgb(cx, cy + 1) + pixel_srgb(cx + 1, cy + 1)) * 0.25);
            v |= (cbcr.x << (16 * k)) | (cbcr.y << (16 * k + 8));
        }
        dst.Store(dstWidth * dstHeight + (t - ySlots) * 4, v);
    }
}

// Zero copy: one thread per 2x2 block, as main_p010 (a texture has no 4-byte store alignment to work around).
[numthreads(8, 8, 1)]
void main_nv12_planes(uint3 id : SV_DispatchThreadID)
{
    uint x = id.x * 2, y = id.y * 2;
    if (x >= dstWidth || y >= dstHeight) return;
    float3 c0 = pixel_srgb(x, y), c1 = pixel_srgb(x + 1, y), c2 = pixel_srgb(x, y + 1), c3 = pixel_srgb(x + 1, y + 1);
    dstLuma[uint2(x, y)]         = luma8(c0);
    dstLuma[uint2(x + 1, y)]     = luma8(c1);
    dstLuma[uint2(x, y + 1)]     = luma8(c2);
    dstLuma[uint2(x + 1, y + 1)] = luma8(c3);
    dstChroma[id.xy] = chroma8((c0 + c1 + c2 + c3) * 0.25);
}
