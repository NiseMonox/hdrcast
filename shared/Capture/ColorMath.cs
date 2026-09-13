using System;

namespace HdrCast.Capture
{
    /// <summary>
    /// C# port of the compute-shader colour maths (design.md §2.3). It is the reference the unit tests check
    /// and what <c>capture --selftest</c> compares the GPU output against. Keep it identical to
    /// <see cref="ConvertShaderSource"/>.
    /// </summary>
    public static class ColorMath
    {
        /// <summary>BT.709 → BT.2020 primaries (row-major; each row sums to 1 so white is preserved).</summary>
        public static readonly double[,] M709To2020 =
        {
            { 0.6274039, 0.3292830, 0.0433131 },
            { 0.0690973, 0.9195404, 0.0113623 },
            { 0.0163914, 0.0880133, 0.8955953 },
        };

        // BT.2020 non-constant-luminance luma coefficients
        public const double Kr2020 = 0.2627, Kg2020 = 0.6780, Kb2020 = 0.0593;
        // BT.709 luma coefficients
        public const double Kr709 = 0.2126, Kg709 = 0.7152, Kb709 = 0.0722;

        /// <summary>SMPTE ST 2084 inverse EOTF. Input in units of 10000 nit (1.0 = 10000 nit); output in [0,1].</summary>
        public static double PqOetf(double x)
        {
            const double m1 = 0.1593017578125, m2 = 78.84375, c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
            double p = Math.Pow(Math.Max(x, 0.0), m1);
            return Math.Pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
        }

        /// <summary>Inverse of <see cref="PqOetf"/>: PQ signal [0,1] → luminance in nit.</summary>
        public static double PqEotfNits(double e)
        {
            const double m1 = 0.1593017578125, m2 = 78.84375, c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
            e = Math.Min(Math.Max(e, 0.0), 1.0);
            double p = Math.Pow(e, 1.0 / m2);
            double num = Math.Max(p - c1, 0.0);
            double den = c2 - c3 * p;
            return Math.Pow(num / den, 1.0 / m1) * 10000.0;
        }

        public static double SrgbToLinear(double c)
        {
            return c <= 0.04045 ? c / 12.92 : Math.Pow((c + 0.055) / 1.055, 2.4);
        }

        public static double LinearToSrgb(double c)
        {
            c = Saturate(c);
            return c <= 0.0031308 ? c * 12.92 : 1.055 * Math.Pow(c, 1.0 / 2.4) - 0.055;
        }

        public static double Saturate(double v) => v < 0 ? 0 : (v > 1 ? 1 : v);

        /// <summary>Rounds like HLSL round() for non-negative values (half away from zero).</summary>
        public static int RoundToInt(double v) => (int)Math.Round(v, MidpointRounding.AwayFromZero);

        /// <summary>
        /// scRGB linear (BT.709 primaries, 1.0 = sdrWhite nit) → PQ-encoded BT.2020 RGB in [0,1]
        /// (the "E" vector of design.md §2.3; negative out-of-gamut values are clipped).
        /// </summary>
        public static void ScRgbToPq2020(double r, double g, double b, double sdrWhite, out double er, out double eg, out double eb)
        {
            double lr = Math.Max(M709To2020[0, 0] * r + M709To2020[0, 1] * g + M709To2020[0, 2] * b, 0.0);
            double lg = Math.Max(M709To2020[1, 0] * r + M709To2020[1, 1] * g + M709To2020[1, 2] * b, 0.0);
            double lb = Math.Max(M709To2020[2, 0] * r + M709To2020[2, 1] * g + M709To2020[2, 2] * b, 0.0);
            er = Saturate(PqOetf(lr * sdrWhite / 10000.0));
            eg = Saturate(PqOetf(lg * sdrWhite / 10000.0));
            eb = Saturate(PqOetf(lb * sdrWhite / 10000.0));
        }

        /// <summary>Full HDR10 pixel conversion: scRGB → 10-bit limited-range Y'CbCr (NCL, BT.2020) as stored in P010 before the &lt;&lt; 6 shift.</summary>
        public static void ScRgbToYuv10(double r, double g, double b, double sdrWhite, out int y10, out int cb10, out int cr10)
        {
            ScRgbToPq2020(r, g, b, sdrWhite, out double er, out double eg, out double eb);
            double y = Kr2020 * er + Kg2020 * eg + Kb2020 * eb;
            double cb = (eb - y) / 1.8814;
            double cr = (er - y) / 1.4746;
            y10 = RoundToInt(64.0 + 876.0 * y);
            cb10 = RoundToInt(512.0 + 896.0 * cb);
            cr10 = RoundToInt(512.0 + 896.0 * cr);
        }

        /// <summary>Luma-only helper (grey values): scRGB value → Y10.</summary>
        public static int ScRgbGreyToY10(double v, double sdrWhite = 80.0)
        {
            ScRgbToYuv10(v, v, v, sdrWhite, out int y10, out _, out _);
            return y10;
        }

        /// <summary>The 16-bit sample as stored in a p010le frame for a 10-bit code (value &lt;&lt; 6).</summary>
        public static ushort ToP010Sample(int code10) => (ushort)((code10 & 0x3FF) << 6);
        public static int FromP010Sample(ushort sample) => sample >> 6;

        /// <summary>SDR pixel conversion: sRGB-encoded BGRA (values in [0,1], used directly as BT.709 gamma) → 8-bit limited-range Y'CbCr for NV12.</summary>
        public static void SrgbToYuv8(double r, double g, double b, out int y8, out int cb8, out int cr8)
        {
            r = Saturate(r); g = Saturate(g); b = Saturate(b);
            double y = Kr709 * r + Kg709 * g + Kb709 * b;
            double cb = (b - y) / 1.8556;
            double cr = (r - y) / 1.5748;
            y8 = RoundToInt(16.0 + 219.0 * y);
            cb8 = RoundToInt(128.0 + 224.0 * cb);
            cr8 = RoundToInt(128.0 + 224.0 * cr);
        }

        /// <summary>Byte size of one raw frame for the given output format (W and H must be even).</summary>
        public static long FrameBytes(CaptureFormat format, int width, int height)
        {
            if (format == CaptureFormat.P010) return (long)width * height * 2 + (long)width * (height / 2) * 2;
            if (format == CaptureFormat.Nv12) return (long)width * height + (long)width * (height / 2);
            throw new ArgumentException("format must be resolved to p010 or nv12", nameof(format));
        }

        /// <summary>Byte offset of the interleaved UV plane inside a raw frame.</summary>
        public static long UvPlaneOffset(CaptureFormat format, int width, int height)
        {
            return format == CaptureFormat.P010 ? (long)width * height * 2 : (long)width * height;
        }

        /// <summary>Fills a frame buffer with "black" (Y = 64/16, Cb = Cr = 512/128) so the encoder gets a valid picture before the first capture.</summary>
        public static void FillBlack(byte[] frame, CaptureFormat format, int width, int height)
        {
            long uv = UvPlaneOffset(format, width, height);
            long total = FrameBytes(format, width, height);
            if (frame.Length < total) throw new ArgumentException("frame buffer too small");
            if (format == CaptureFormat.P010)
            {
                ushort y = ToP010Sample(64), c = ToP010Sample(512);
                for (long i = 0; i < uv; i += 2) { frame[i] = (byte)y; frame[i + 1] = (byte)(y >> 8); }
                for (long i = uv; i < total; i += 2) { frame[i] = (byte)c; frame[i + 1] = (byte)(c >> 8); }
            }
            else
            {
                for (long i = 0; i < uv; i++) frame[i] = 16;
                for (long i = uv; i < total; i++) frame[i] = 128;
            }
        }
    }
}
