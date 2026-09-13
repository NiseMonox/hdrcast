using System;
using System.Globalization;
using HdrCast.Capture;
using SharpDX;
using SharpDX.Direct3D;
using SharpDX.Direct3D11;
using SharpDX.DXGI;
using Device11 = SharpDX.Direct3D11.Device;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// <c>capture --selftest</c>: runs the conversion shader on a synthetic scRGB gradient and compares the
    /// GPU result with the C# reference implementation (<see cref="ColorMath"/>). This verifies the HDR maths
    /// on the user's own GPU without touching the desktop, so a colour problem can be located quickly.
    /// Reference values (ST.2084 at sdr-white 80 nit, limited-range 10 bit): scRGB 1.0 → Y10 490,
    /// 2.5 → 571, 125 → 940 (clipped at the limited-range maximum). ffmpeg's zscale with npl=80 lands within
    /// 3 code values of these; the GPU must match the C# port exactly, which is what this test checks.
    /// </summary>
    internal static class SelfTest
    {
        static readonly float[] TestValues = { 0.0f, 0.5f, 1.0f, 2.5f, 12.5f, 125.0f };

        public static int Run(float sdrWhite)
        {
            int width = 16 * TestValues.Length;   // one 16 px column per test value
            int height = 16;

            Device11? device = null;
            Texture2D? source = null;
            GpuConverter? converter = null;
            try
            {
                device = CreateDevice(out string driver);
                Console.Out.WriteLine("device: " + driver);

                source = CreateSourceTexture(device, width, height);
                converter = new GpuConverter(device, CaptureFormat.P010, width, height);
                converter.UpdateSource(source);

                var frame = new byte[converter.FrameBytes];
                converter.Convert(new ShaderParams
                {
                    SrcWidth = (uint)width,
                    SrcHeight = (uint)height,
                    DeskWidth = (uint)width,
                    DeskHeight = (uint)height,
                    DstWidth = (uint)width,
                    DstHeight = (uint)height,
                    Rotation = 0,
                    Flags = ConvertShaderSource.FlagSourceLinear | ConvertShaderSource.FlagExact,
                    SdrWhite = sdrWhite,
                    CursorX = 0,
                    CursorY = 0,
                    CursorW = 1,
                    CursorH = 1,
                }, frame);

                Console.Out.WriteLine("scRGB -> HDR10 (PQ, BT.2020, limited range 10 bit), sdr-white = " +
                                      sdrWhite.ToString("0.#", CultureInfo.InvariantCulture) + " nit");
                Console.Out.WriteLine("  input      GPU Y10   reference   nits      status");
                int failures = 0;
                for (int i = 0; i < TestValues.Length; i++)
                {
                    int x = i * 16 + 8, y = 8;
                    int offset = (y * width + x) * 2;
                    ushort sample = (ushort)(frame[offset] | (frame[offset + 1] << 8));
                    int gpu = ColorMath.FromP010Sample(sample);
                    int reference = ColorMath.ScRgbGreyToY10(TestValues[i], sdrWhite);
                    double nits = ColorMath.PqEotfNits(Math.Max(0, (gpu - 64) / 876.0));
                    bool ok = Math.Abs(gpu - reference) <= 2;
                    if (!ok) failures++;
                    Console.Out.WriteLine(string.Format(CultureInfo.InvariantCulture,
                        "  {0,-10} {1,7}   {2,9}   {3,8:0.#}  {4}",
                        TestValues[i].ToString("0.###", CultureInfo.InvariantCulture), gpu, reference, nits,
                        ok ? "ok" : "MISMATCH"));
                }

                // The UV plane must be neutral for grey input (Cb = Cr = 512).
                long uvOffset = ColorMath.UvPlaneOffset(CaptureFormat.P010, width, height);
                int uvIndex = (int)uvOffset + ((height / 2 - 1) * width * 2) + 16;
                int cb = ColorMath.FromP010Sample((ushort)(frame[uvIndex] | (frame[uvIndex + 1] << 8)));
                int cr = ColorMath.FromP010Sample((ushort)(frame[uvIndex + 2] | (frame[uvIndex + 3] << 8)));
                bool uvOk = Math.Abs(cb - 512) <= 2 && Math.Abs(cr - 512) <= 2;
                Console.Out.WriteLine("  chroma of grey: Cb=" + cb + " Cr=" + cr + (uvOk ? " ok" : " MISMATCH (expected 512/512)"));
                if (!uvOk) failures++;

                Console.Out.WriteLine(failures == 0
                    ? "selftest: PASS — the GPU shader matches the reference implementation"
                    : "selftest: FAIL — " + failures + " mismatch(es)");
                return failures == 0 ? ExitCodes.Ok : ExitCodes.DeviceError;
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine("selftest: " + ex.Message);
                return ExitCodes.DeviceError;
            }
            finally
            {
                converter?.Dispose();
                source?.Dispose();
                device?.Dispose();
            }
        }

        static Device11 CreateDevice(out string description)
        {
            try
            {
                using (var factory = new Factory1())
                {
                    if (factory.GetAdapterCount1() > 0)
                    {
                        using (Adapter1 adapter = factory.GetAdapter1(0))
                        {
                            description = (adapter.Description1.Description ?? "").Trim('\0').Trim();
                            return new Device11(adapter, DeviceCreationFlags.BgraSupport,
                                                new[] { FeatureLevel.Level_11_1, FeatureLevel.Level_11_0 });
                        }
                    }
                }
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine("selftest: hardware device unavailable (" + ex.Message + "), falling back to WARP");
            }
            description = "WARP (software)";
            return new Device11(DriverType.Warp, DeviceCreationFlags.BgraSupport);
        }

        /// <summary>Builds an R16G16B16A16_FLOAT texture with one constant scRGB column per test value.</summary>
        static Texture2D CreateSourceTexture(Device11 device, int width, int height)
        {
            var pixels = new ushort[width * height * 4];
            for (int y = 0; y < height; y++)
            {
                for (int x = 0; x < width; x++)
                {
                    float v = TestValues[Math.Min(TestValues.Length - 1, x / 16)];
                    int i = (y * width + x) * 4;
                    pixels[i + 0] = FloatToHalf(v);
                    pixels[i + 1] = FloatToHalf(v);
                    pixels[i + 2] = FloatToHalf(v);
                    pixels[i + 3] = FloatToHalf(1.0f);
                }
            }

            var desc = new Texture2DDescription
            {
                Width = width,
                Height = height,
                MipLevels = 1,
                ArraySize = 1,
                Format = Format.R16G16B16A16_Float,
                SampleDescription = new SampleDescription(1, 0),
                Usage = ResourceUsage.Default,
                BindFlags = BindFlags.ShaderResource,
                CpuAccessFlags = CpuAccessFlags.None,
                OptionFlags = ResourceOptionFlags.None,
            };
            using (DataStream stream = DataStream.Create(pixels, true, false))
            {
                return new Texture2D(device, desc, new DataRectangle(stream.DataPointer, width * 8));
            }
        }

        /// <summary>IEEE 754 binary32 → binary16 (round toward zero; good enough for a test pattern).</summary>
        internal static ushort FloatToHalf(float value)
        {
            byte[] bytes = BitConverter.GetBytes(value);
            uint bits = BitConverter.ToUInt32(bytes, 0);
            uint sign = (bits >> 16) & 0x8000;
            int exponent = (int)((bits >> 23) & 0xFF) - 127 + 15;
            uint mantissa = bits & 0x7FFFFF;

            if (exponent >= 0x1F) return (ushort)(sign | 0x7C00);                   // overflow / inf / nan
            if (exponent <= 0)
            {
                if (exponent < -10) return (ushort)sign;                            // underflow to zero
                mantissa |= 0x800000;
                uint sub = mantissa >> (1 - exponent + 13);
                return (ushort)(sign | sub);
            }
            return (ushort)(sign | ((uint)exponent << 10) | (mantissa >> 13));
        }
    }
}
