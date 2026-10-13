using System;
using System.Runtime.InteropServices;
using HdrCast.Capture;
using SharpDX;
using SharpDX.D3DCompiler;
using SharpDX.Direct3D;
using SharpDX.Direct3D11;
using SharpDX.DXGI;
using Buffer11 = SharpDX.Direct3D11.Buffer;
using Device11 = SharpDX.Direct3D11.Device;
using MapFlags11 = SharpDX.Direct3D11.MapFlags;

namespace HdrCast.Sender.Capture
{
    /// <summary>Constant buffer of the conversion shader — must match "cbuffer Params" in ConvertShaderSource (64 bytes).</summary>
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    internal struct ShaderParams
    {
        public uint SrcWidth;
        public uint SrcHeight;
        public uint DeskWidth;
        public uint DeskHeight;
        public uint DstWidth;
        public uint DstHeight;
        public uint Rotation;
        public uint Flags;
        public float SdrWhite;
        public int CursorX;
        public int CursorY;
        public uint CursorW;
        public uint CursorH;
        public float Pad0;
        public float Pad1;
        public float Pad2;

        public const int SizeInBytes = 64;
    }

    /// <summary>
    /// GPU side of the capture engine: compiles the conversion compute shader, runs it over the duplicated
    /// desktop texture and reads the finished P010 / NV12 frame back to system memory (design.md §2.3).
    /// </summary>
    internal sealed class GpuConverter : IDisposable
    {
        readonly Device11 _device;
        readonly DeviceContext _context;
        readonly CaptureFormat _format;
        readonly int _dstWidth;
        readonly int _dstHeight;
        readonly int _frameBytes;

        ComputeShader? _shader;
        Buffer11? _outBuffer;
        UnorderedAccessView? _outUav;
        Buffer11? _staging;
        Buffer11? _constants;
        SamplerState? _sampler;

        Texture2D? _srcCopy;
        ShaderResourceView? _srcSrv;
        Texture2D? _cursorTex;
        ShaderResourceView? _cursorSrv;
        int _cursorW, _cursorH;

        public GpuConverter(Device11 device, CaptureFormat format, int dstWidth, int dstHeight)
        {
            if (format == CaptureFormat.Auto) throw new ArgumentException("format must be resolved", nameof(format));
            _device = device;
            _context = device.ImmediateContext;
            _format = format;
            _dstWidth = dstWidth;
            _dstHeight = dstHeight;
            long bytes = ColorMath.FrameBytes(format, dstWidth, dstHeight);
            if (bytes > int.MaxValue) throw new ArgumentException("frame too large");
            _frameBytes = (int)bytes;
            Build();
        }

        public int FrameBytes => _frameBytes;
        public int DstWidth => _dstWidth;
        public int DstHeight => _dstHeight;

        void Build()
        {
            // The constant buffer layout is duplicated in HLSL; a mismatch would silently corrupt every frame.
            int paramsSize = Marshal.SizeOf(typeof(ShaderParams));
            if (paramsSize != ShaderParams.SizeInBytes)
                throw new InvalidOperationException("ShaderParams is " + paramsSize + " bytes, expected " +
                                                    ShaderParams.SizeInBytes + " (must match cbuffer Params)");

            string entry = _format == CaptureFormat.P010 ? ConvertShaderSource.EntryP010 : ConvertShaderSource.EntryNv12;
            // bytes, not the string: see ConvertShaderSource.SourceBytes
            using (CompilationResult result = ShaderBytecode.Compile(
                       ConvertShaderSource.SourceBytes(), entry, ConvertShaderSource.Profile,
                       ShaderFlags.OptimizationLevel3, EffectFlags.None, "hdrcast_convert.hlsl"))
            {
                if (result.HasErrors || result.Bytecode == null)
                    throw new InvalidOperationException("conversion shader failed to compile: " + result.Message);
                _shader = new ComputeShader(_device, result.Bytecode);
            }

            // Raw (ByteAddress) UAV buffer holding the complete frame, plus a staging copy for CPU readback.
            _outBuffer = new Buffer11(_device, new BufferDescription
            {
                SizeInBytes = Align16(_frameBytes),
                Usage = ResourceUsage.Default,
                BindFlags = BindFlags.UnorderedAccess | BindFlags.ShaderResource,
                CpuAccessFlags = CpuAccessFlags.None,
                OptionFlags = ResourceOptionFlags.BufferAllowRawViews,
                StructureByteStride = 0,
            });

            var uavDesc = new UnorderedAccessViewDescription
            {
                Format = Format.R32_Typeless,
                Dimension = UnorderedAccessViewDimension.Buffer,
                Buffer = new UnorderedAccessViewDescription.BufferResource
                {
                    FirstElement = 0,
                    ElementCount = Align16(_frameBytes) / 4,
                    Flags = UnorderedAccessViewBufferFlags.Raw,
                },
            };
            _outUav = new UnorderedAccessView(_device, _outBuffer, uavDesc);

            _staging = new Buffer11(_device, new BufferDescription
            {
                SizeInBytes = Align16(_frameBytes),
                Usage = ResourceUsage.Staging,
                BindFlags = BindFlags.None,
                CpuAccessFlags = CpuAccessFlags.Read,
                OptionFlags = ResourceOptionFlags.None,
                StructureByteStride = 0,
            });

            _constants = new Buffer11(_device, new BufferDescription
            {
                SizeInBytes = ShaderParams.SizeInBytes,
                Usage = ResourceUsage.Default,
                BindFlags = BindFlags.ConstantBuffer,
                CpuAccessFlags = CpuAccessFlags.None,
                OptionFlags = ResourceOptionFlags.None,
                StructureByteStride = 0,
            });

            var samplerDesc = SamplerStateDescription.Default();
            samplerDesc.Filter = Filter.MinMagMipLinear;
            samplerDesc.AddressU = TextureAddressMode.Clamp;
            samplerDesc.AddressV = TextureAddressMode.Clamp;
            samplerDesc.AddressW = TextureAddressMode.Clamp;
            _sampler = new SamplerState(_device, samplerDesc);
        }

        static int Align16(int v) => (v + 15) & ~15;

        /// <summary>Copies the duplicated desktop texture into our own shader-readable texture.</summary>
        public void UpdateSource(Texture2D desktop)
        {
            Texture2DDescription d = desktop.Description;
            if (_srcCopy == null || _srcCopy.Description.Width != d.Width || _srcCopy.Description.Height != d.Height ||
                _srcCopy.Description.Format != d.Format)
            {
                Utilities.Dispose(ref _srcSrv);
                Utilities.Dispose(ref _srcCopy);
                _srcCopy = new Texture2D(_device, new Texture2DDescription
                {
                    Width = d.Width,
                    Height = d.Height,
                    MipLevels = 1,
                    ArraySize = 1,
                    Format = d.Format,
                    SampleDescription = new SampleDescription(1, 0),
                    Usage = ResourceUsage.Default,
                    BindFlags = BindFlags.ShaderResource,
                    CpuAccessFlags = CpuAccessFlags.None,
                    OptionFlags = ResourceOptionFlags.None,
                });
                _srcSrv = new ShaderResourceView(_device, _srcCopy);
            }
            _context.CopyResource(desktop, _srcCopy);
        }

        public bool HasSource => _srcCopy != null;

        /// <summary>Uploads a decoded cursor image (BGRA, straight alpha; a=0 &amp;&amp; r=1 means "invert desktop").</summary>
        public void SetCursor(byte[] bgra, int width, int height)
        {
            if (width <= 0 || height <= 0 || bgra == null) return;
            if (_cursorTex == null || _cursorW != width || _cursorH != height)
            {
                Utilities.Dispose(ref _cursorSrv);
                Utilities.Dispose(ref _cursorTex);
                _cursorTex = new Texture2D(_device, new Texture2DDescription
                {
                    Width = width,
                    Height = height,
                    MipLevels = 1,
                    ArraySize = 1,
                    Format = Format.B8G8R8A8_UNorm,
                    SampleDescription = new SampleDescription(1, 0),
                    Usage = ResourceUsage.Default,
                    BindFlags = BindFlags.ShaderResource,
                    CpuAccessFlags = CpuAccessFlags.None,
                    OptionFlags = ResourceOptionFlags.None,
                });
                _cursorSrv = new ShaderResourceView(_device, _cursorTex);
                _cursorW = width;
                _cursorH = height;
            }
            GCHandle handle = GCHandle.Alloc(bgra, GCHandleType.Pinned);
            try
            {
                _context.UpdateSubresource(new DataBox(handle.AddrOfPinnedObject(), width * 4, width * 4 * height),
                                           _cursorTex, 0);
            }
            finally { handle.Free(); }
        }

        /// <summary>Runs the shader and copies the finished frame into <paramref name="destination"/>.</summary>
        public void Convert(ShaderParams parameters, byte[] destination)
        {
            if (_srcCopy == null || _srcSrv == null) throw new InvalidOperationException("no source texture");
            if (destination.Length < _frameBytes) throw new ArgumentException("destination buffer too small", nameof(destination));
            if (_cursorSrv == null) parameters.Flags &= ~(uint)ConvertShaderSource.FlagCursorVisible;

            _context.UpdateSubresource(ref parameters, _constants);

            _context.ComputeShader.Set(_shader);
            _context.ComputeShader.SetConstantBuffer(0, _constants);
            _context.ComputeShader.SetShaderResource(0, _srcSrv);
            _context.ComputeShader.SetShaderResource(1, _cursorSrv);
            _context.ComputeShader.SetSampler(0, _sampler);
            _context.ComputeShader.SetUnorderedAccessView(0, _outUav);

            if (_format == CaptureFormat.P010)
            {
                int blocksX = (_dstWidth + 1) / 2, blocksY = (_dstHeight + 1) / 2;
                int g = ConvertShaderSource.P010GroupSize;
                _context.Dispatch((blocksX + g - 1) / g, (blocksY + g - 1) / g, 1);
            }
            else
            {
                // main_nv12 is a 1-D kernel folded into a 2-D dispatch of Nv12GroupsX groups per row.
                // Same rounding as the shader: the UV plane can end on a half uint.
                long pixels = (long)_dstWidth * _dstHeight;
                long slots = pixels / 4 + (pixels + 7) / 8;
                long groups = (slots + ConvertShaderSource.Nv12GroupSize - 1) / ConvertShaderSource.Nv12GroupSize;
                int gx = ConvertShaderSource.Nv12GroupsX;
                int gy = (int)((groups + gx - 1) / gx);
                _context.Dispatch(gx, Math.Max(1, gy), 1);
            }

            // Unbind so the resources can be copied / re-bound next frame.
            _context.ComputeShader.SetUnorderedAccessView(0, null);
            _context.ComputeShader.SetShaderResource(0, null);
            _context.ComputeShader.SetShaderResource(1, null);

            _context.CopyResource(_outBuffer, _staging);
            DataBox box = _context.MapSubresource(_staging, 0, MapMode.Read, MapFlags11.None);
            try
            {
                Marshal.Copy(box.DataPointer, destination, 0, _frameBytes);
            }
            finally
            {
                _context.UnmapSubresource(_staging, 0);
            }
        }

        public void Dispose()
        {
            Utilities.Dispose(ref _cursorSrv);
            Utilities.Dispose(ref _cursorTex);
            Utilities.Dispose(ref _srcSrv);
            Utilities.Dispose(ref _srcCopy);
            Utilities.Dispose(ref _sampler);
            Utilities.Dispose(ref _constants);
            Utilities.Dispose(ref _staging);
            Utilities.Dispose(ref _outUav);
            Utilities.Dispose(ref _outBuffer);
            Utilities.Dispose(ref _shader);
        }
    }
}
