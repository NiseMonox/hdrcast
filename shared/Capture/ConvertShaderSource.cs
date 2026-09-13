namespace HdrCast.Capture
{
    /// <summary>
    /// HLSL source of the desktop → P010 / NV12 conversion compute shader (design.md §2.3), compiled at runtime
    /// with d3dcompiler_47 (cs_5_0). Two entry points share the helpers:
    ///   main_p010 — one thread per 2x2 block (8x8 groups): 4 luma + 1 averaged chroma pair, 4-byte stores.
    ///   main_nv12 — one thread per 4 Y bytes / per 2 chroma pairs (linear addressing): RWByteAddressBuffer
    ///               stores are 4-byte aligned, and an NV12 row is W bytes, so 2x2 blocks cannot be written
    ///               directly when W % 4 == 2 (e.g. 1366-wide displays). Colour maths is cheap for SDR, so the
    ///               2x recompute of the chroma path does not matter.
    /// Constant buffer layout must match <c>ShaderParams</c> in the sender (64 bytes).
    /// </summary>
    public static class ConvertShaderSource
    {
        public const int FlagCursorVisible = 1;
        public const int FlagSourceLinear = 2;   // source texture is FP16 scRGB (HDR desktop); else BGRA8 sRGB
        public const int FlagExact = 4;          // no scaling / rotation: use Load() instead of the sampler
        public const int FlagSourceIsHdrForcedSdr = 8;   // informational only (logging)

        public const string EntryP010 = "main_p010";
        public const string EntryNv12 = "main_nv12";
        public const string Profile = "cs_5_0";

        /// <summary>Threads per group in X and Y for main_p010 (each thread = one 2x2 block).</summary>
        public const int P010GroupSize = 8;
        /// <summary>Threads per group for main_nv12 (1-D, folded into a 2-D dispatch of Nv12GroupsX columns).</summary>
        public const int Nv12GroupSize = 64;
        public const int Nv12GroupsX = 1024;

        public const string Source = @"
// HdrCast desktop -> P010 / NV12 conversion (design.md §2.3). Generated from ConvertShaderSource.cs.
Texture2D<float4> srcTex     : register(t0);   // duplicated desktop: R16G16B16A16_FLOAT scRGB linear or B8G8R8A8_UNORM sRGB
Texture2D<float4> cursorTex  : register(t1);   // BGRA8 straight-alpha cursor; a == 0 && r > 0.5 means 'invert desktop'
SamplerState linearSampler   : register(s0);
RWByteAddressBuffer dst      : register(u0);   // full raw frame: Y plane then interleaved UV plane

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

// Pixel in linear scRGB (1.0 = sdrWhite nit) with the cursor composited — the HDR (P010) working space.
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

// Pixel in sRGB-encoded [0,1] with the cursor composited — the SDR (NV12) working space.
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

[numthreads(8, 8, 1)]
void main_p010(uint3 id : SV_DispatchThreadID)
{
    uint x = id.x * 2, y = id.y * 2;
    if (x >= dstWidth || y >= dstHeight) return;
    float3 E0 = encode_pq2020(pixel_linear(x,     y));
    float3 E1 = encode_pq2020(pixel_linear(x + 1, y));
    float3 E2 = encode_pq2020(pixel_linear(x,     y + 1));
    float3 E3 = encode_pq2020(pixel_linear(x + 1, y + 1));
    uint y0 = luma10(E0), y1 = luma10(E1), y2 = luma10(E2), y3 = luma10(E3);
    float3 Ea = (E0 + E1 + E2 + E3) * 0.25;             // chroma from the average of the 4 PQ-encoded pixels
    float Ya = 0.2627 * Ea.r + 0.6780 * Ea.g + 0.0593 * Ea.b;
    float Cb = (Ea.b - Ya) / 1.8814;
    float Cr = (Ea.r - Ya) / 1.4746;
    uint cb10 = (uint)round(512.0 + 896.0 * Cb);
    uint cr10 = (uint)round(512.0 + 896.0 * Cr);
    uint rowBytes = dstWidth * 2;
    dst.Store(y * rowBytes + x * 2,       (y0 << 6) | ((y1 << 6) << 16));   // p010le: 10 bits in the high bits
    dst.Store((y + 1) * rowBytes + x * 2, (y2 << 6) | ((y3 << 6) << 16));
    uint uvBase = dstWidth * dstHeight * 2;
    dst.Store(uvBase + (y >> 1) * rowBytes + x * 2, (cb10 << 6) | ((cr10 << 6) << 16));
}

uint luma8(float3 c)
{
    float Y = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
    return (uint)round(16.0 + 219.0 * Y);
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
            float3 c = (pixel_srgb(cx, cy) + pixel_srgb(cx + 1, cy) + pixel_srgb(cx, cy + 1) + pixel_srgb(cx + 1, cy + 1)) * 0.25;
            float Y = 0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b;
            uint cb8 = (uint)round(128.0 + 224.0 * (c.b - Y) / 1.8556);
            uint cr8 = (uint)round(128.0 + 224.0 * (c.r - Y) / 1.5748);
            v |= (cb8 << (16 * k)) | (cr8 << (16 * k + 8));
        }
        dst.Store(dstWidth * dstHeight + (t - ySlots) * 4, v);
    }
}
";
    }
}
