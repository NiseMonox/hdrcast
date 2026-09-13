using System;
using System.Runtime.InteropServices;
using HdrCast.Capture;
using SharpDX;
using SharpDX.Direct3D;
using SharpDX.Direct3D11;
using SharpDX.DXGI;
using Device11 = SharpDX.Direct3D11.Device;
using DxgiResource = SharpDX.DXGI.Resource;

namespace HdrCast.Sender.Capture
{
    /// <summary>Outcome of one <see cref="DesktopDuplicator.TryAcquire"/> call.</summary>
    internal enum AcquireResult
    {
        /// <summary>A new desktop image was copied into the converter.</summary>
        NewFrame,
        /// <summary>No new frame within the timeout — the desktop did not change.</summary>
        Timeout,
        /// <summary>Only the mouse pointer moved / changed shape.</summary>
        PointerOnly,
        /// <summary>The duplication was invalidated (mode change, full-screen app, session switch): recreate it.</summary>
        AccessLost,
    }

    /// <summary>
    /// DXGI Desktop Duplication for one output: creates the D3D11 device on the owning adapter, requests an
    /// FP16 (scRGB, HDR desktop) or BGRA8 (SDR desktop) duplication and feeds frames to the <see cref="GpuConverter"/>.
    /// </summary>
    internal sealed class DesktopDuplicator : IDisposable
    {
        // DXGI HRESULTs (compared numerically so the code does not depend on SharpDX enum naming).
        const int DXGI_ERROR_WAIT_TIMEOUT = unchecked((int)0x887A0027);
        const int DXGI_ERROR_ACCESS_LOST = unchecked((int)0x887A0026);
        const int DXGI_ERROR_INVALID_CALL = unchecked((int)0x887A0001);
        const int DXGI_ERROR_UNSUPPORTED = unchecked((int)0x887A0004);
        const int DXGI_ERROR_SESSION_DISCONNECTED = unchecked((int)0x887A0028);
        const int DXGI_ERROR_NOT_CURRENTLY_AVAILABLE = unchecked((int)0x887A0022);
        const int E_ACCESSDENIED = unchecked((int)0x80070005);

        const int POINTER_SHAPE_MONOCHROME = 1;
        const int POINTER_SHAPE_COLOR = 2;
        const int POINTER_SHAPE_MASKED_COLOR = 4;

        readonly int _outputIndex;
        readonly bool _wantHdr;
        Device11? _device;
        Output5? _output5;
        Output? _output;
        OutputDuplication? _duplication;
        byte[] _shapeBuffer = new byte[0];

        public DesktopDuplicator(int outputIndex, bool wantHdr)
        {
            _outputIndex = outputIndex;
            _wantHdr = wantHdr;
        }

        public Device11 Device => _device ?? throw new InvalidOperationException("not initialised");
        /// <summary>Size of the duplicated framebuffer (not rotated).</summary>
        public int SourceWidth { get; private set; }
        public int SourceHeight { get; private set; }
        /// <summary>Desktop size as the user sees it (rotation applied).</summary>
        public int DesktopWidth { get; private set; }
        public int DesktopHeight { get; private set; }
        /// <summary>DXGI_MODE_ROTATION - 1: 0 identity, 1 = 90°, 2 = 180°, 3 = 270°.</summary>
        public int Rotation { get; private set; }
        /// <summary>True when the duplication delivers R16G16B16A16_FLOAT (scRGB) frames.</summary>
        public bool SourceIsFloat { get; private set; }
        public OutputInfo Info { get; private set; } = new OutputInfo();

        public bool CursorVisible { get; private set; }
        public int CursorX { get; private set; }
        public int CursorY { get; private set; }
        public int CursorWidth { get; private set; }
        public int CursorHeight { get; private set; }
        /// <summary>Set when a new cursor bitmap was decoded; the caller uploads it and clears the flag.</summary>
        public byte[]? PendingCursorBgra { get; private set; }

        /// <summary>Creates the device and the duplication. Throws with a readable message on failure.</summary>
        public void Initialize()
        {
            Dispose();
            using (var factory = new Factory1())
            {
                int index = 0;
                for (int ai = 0; ai < factory.GetAdapterCount1(); ai++)
                {
                    using (Adapter1 adapter = factory.GetAdapter1(ai))
                    {
                        int outputs;
                        try { outputs = adapter.GetOutputCount(); }
                        catch (Exception) { continue; }
                        for (int oi = 0; oi < outputs; oi++)
                        {
                            if (index++ != _outputIndex) continue;
                            Output output = adapter.GetOutput(oi);
                            try
                            {
                                // The D3D11 device must live on the adapter that owns the output.
                                _device = new Device11(adapter, DeviceCreationFlags.BgraSupport,
                                                       new[] { FeatureLevel.Level_11_1, FeatureLevel.Level_11_0 });
                                _output = output;
                                SetupFromOutput(output, adapter, ai, oi);
                                return;
                            }
                            catch
                            {
                                output.Dispose();
                                throw;
                            }
                        }
                    }
                }
            }
            throw new InvalidOperationException("DXGI output " + _outputIndex + " does not exist (run list-outputs)");
        }

        void SetupFromOutput(Output output, Adapter1 adapter, int adapterIndex, int outputIndexOnAdapter)
        {
            OutputDescription d = output.Description;
            int w = d.DesktopBounds.Right - d.DesktopBounds.Left;
            int h = d.DesktopBounds.Bottom - d.DesktopBounds.Top;
            DesktopWidth = w;
            DesktopHeight = h;
            Rotation = Math.Max(0, (int)d.Rotation - 1);
            // A 90/270 rotated desktop is duplicated in the framebuffer's (unrotated) orientation.
            bool swapped = Rotation == 1 || Rotation == 3;
            SourceWidth = swapped ? h : w;
            SourceHeight = swapped ? w : h;

            Info = new OutputInfo
            {
                Index = _outputIndex,
                AdapterIndex = adapterIndex,
                OutputIndexOnAdapter = outputIndexOnAdapter,
                Adapter = (adapter.Description1.Description ?? "").Trim('\0').Trim(),
                DeviceName = (d.DeviceName ?? "").Trim('\0'),
                Width = w,
                Height = h,
                X = d.DesktopBounds.Left,
                Y = d.DesktopBounds.Top,
                Rotation = (int)d.Rotation,
                AttachedToDesktop = d.IsAttachedToDesktop,
                ColorSpace = -1,
            };
            try
            {
                using (Output6 o6 = output.QueryInterface<Output6>())
                {
                    OutputDescription1 d1 = o6.Description1;
                    Info.ColorSpace = (int)d1.ColorSpace;
                    Info.Hdr = Info.ColorSpace == OutputInfo.ColorSpaceHdr10;
                    Info.BitsPerColor = d1.BitsPerColor;
                    Info.MinLuminance = d1.MinLuminance;
                    Info.MaxLuminance = d1.MaxLuminance;
                    Info.MaxFullFrameLuminance = d1.MaxFullFrameLuminance;
                }
            }
            catch (Exception) { }

            _output5 = output.QueryInterface<Output5>();
            CreateDuplication();
        }

        void CreateDuplication()
        {
            if (_output5 == null || _device == null) throw new InvalidOperationException("not initialised");
            Utilities.Dispose(ref _duplication);

            // Ask for FP16 first on an HDR desktop: that is the only format carrying values above SDR white.
            Format[] formats = _wantHdr
                ? new[] { Format.R16G16B16A16_Float, Format.B8G8R8A8_UNorm }
                : new[] { Format.B8G8R8A8_UNorm };
            try
            {
                _duplication = _output5.DuplicateOutput1(_device, 0, formats.Length, formats);
            }
            catch (SharpDXException ex)
            {
                if (ex.ResultCode.Code == E_ACCESSDENIED)
                    throw new InvalidOperationException(
                        "Desktop Duplication was denied (another capture session, or a protected full-screen app). " +
                        "HRESULT 0x" + ex.ResultCode.Code.ToString("X8"), ex);
                if (ex.ResultCode.Code == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE)
                    throw new InvalidOperationException(
                        "Too many desktop duplications are already active on this GPU. HRESULT 0x" +
                        ex.ResultCode.Code.ToString("X8"), ex);
                if (ex.ResultCode.Code == DXGI_ERROR_UNSUPPORTED || ex.ResultCode.Code == DXGI_ERROR_INVALID_CALL)
                    throw new InvalidOperationException(
                        "This display cannot be duplicated in the requested format. HRESULT 0x" +
                        ex.ResultCode.Code.ToString("X8"), ex);
                throw;
            }

            OutputDuplicateDescription desc = _duplication.Description;
            SourceIsFloat = desc.ModeDescription.Format == Format.R16G16B16A16_Float;
            if (desc.ModeDescription.Width > 0 && desc.ModeDescription.Height > 0)
            {
                SourceWidth = desc.ModeDescription.Width;
                SourceHeight = desc.ModeDescription.Height;
            }
        }

        /// <summary>Recreates the duplication after DXGI_ERROR_ACCESS_LOST (device stays alive).</summary>
        public void Recreate() => CreateDuplication();

        /// <summary>
        /// Tries to get one frame. On <see cref="AcquireResult.NewFrame"/> the image was copied into
        /// <paramref name="converter"/>; pointer state is updated on every call that returns a frame or pointer update.
        /// </summary>
        public AcquireResult TryAcquire(int timeoutMs, GpuConverter converter)
        {
            if (_duplication == null) return AcquireResult.AccessLost;

            OutputDuplicateFrameInformation frameInfo;
            DxgiResource? resource = null;
            Result result = _duplication.TryAcquireNextFrame(timeoutMs, out frameInfo, out resource);

            if (result.Code == DXGI_ERROR_WAIT_TIMEOUT) return AcquireResult.Timeout;
            if (result.Code == DXGI_ERROR_ACCESS_LOST || result.Code == DXGI_ERROR_SESSION_DISCONNECTED)
                return AcquireResult.AccessLost;
            if (result.Failure)
            {
                Utilities.Dispose(ref resource);
                throw new SharpDXException(result);
            }

            try
            {
                UpdatePointer(ref frameInfo);
                bool newImage = frameInfo.LastPresentTime != 0 && frameInfo.AccumulatedFrames > 0;
                if (newImage && resource != null)
                {
                    using (Texture2D texture = resource.QueryInterface<Texture2D>())
                    {
                        converter.UpdateSource(texture);
                    }
                }
                return newImage ? AcquireResult.NewFrame
                     : (frameInfo.LastMouseUpdateTime != 0 ? AcquireResult.PointerOnly : AcquireResult.Timeout);
            }
            finally
            {
                Utilities.Dispose(ref resource);
                try { _duplication.ReleaseFrame(); }
                catch (SharpDXException) { /* already released / access lost: handled on the next acquire */ }
            }
        }

        void UpdatePointer(ref OutputDuplicateFrameInformation frameInfo)
        {
            if (frameInfo.LastMouseUpdateTime != 0)
            {
                CursorVisible = frameInfo.PointerPosition.Visible;
                CursorX = frameInfo.PointerPosition.Position.X;
                CursorY = frameInfo.PointerPosition.Position.Y;
            }
            if (frameInfo.PointerShapeBufferSize <= 0 || _duplication == null) return;

            if (_shapeBuffer.Length < frameInfo.PointerShapeBufferSize)
                _shapeBuffer = new byte[frameInfo.PointerShapeBufferSize];

            GCHandle handle = GCHandle.Alloc(_shapeBuffer, GCHandleType.Pinned);
            try
            {
                int required;
                OutputDuplicatePointerShapeInformation shapeInfo;
                _duplication.GetFramePointerShape(_shapeBuffer.Length, handle.AddrOfPinnedObject(), out required, out shapeInfo);
                byte[]? bgra = DecodeCursor(_shapeBuffer, shapeInfo, out int w, out int h);
                if (bgra != null)
                {
                    PendingCursorBgra = bgra;
                    CursorWidth = w;
                    CursorHeight = h;
                }
            }
            catch (SharpDXException)
            {
                // A shape we cannot read is not fatal; keep the previous cursor image.
            }
            finally { handle.Free(); }
        }

        /// <summary>
        /// Converts a DXGI pointer shape into straight-alpha BGRA for the shader.
        /// Convention: alpha 0 with red 255 means "invert the desktop pixel" (XOR cursors).
        /// </summary>
        internal static byte[]? DecodeCursor(byte[] data, OutputDuplicatePointerShapeInformation info, out int width, out int height)
        {
            width = info.Width;
            height = info.Type == POINTER_SHAPE_MONOCHROME ? info.Height / 2 : info.Height;
            if (width <= 0 || height <= 0 || info.Pitch <= 0) return null;

            var bgra = new byte[width * height * 4];
            switch (info.Type)
            {
                case POINTER_SHAPE_COLOR:
                {
                    for (int y = 0; y < height; y++)
                    {
                        int src = y * info.Pitch;
                        int dst = y * width * 4;
                        if (src + width * 4 > data.Length) break;
                        Array.Copy(data, src, bgra, dst, width * 4);
                    }
                    break;
                }
                case POINTER_SHAPE_MASKED_COLOR:
                {
                    // alpha 0x00: opaque colour. alpha 0xFF: XOR with the desktop.
                    for (int y = 0; y < height; y++)
                    {
                        int src = y * info.Pitch;
                        int dst = y * width * 4;
                        if (src + width * 4 > data.Length) break;
                        for (int x = 0; x < width; x++)
                        {
                            byte b = data[src + x * 4 + 0], g = data[src + x * 4 + 1];
                            byte r = data[src + x * 4 + 2], a = data[src + x * 4 + 3];
                            if (a == 0)
                            {
                                bgra[dst + x * 4 + 0] = b; bgra[dst + x * 4 + 1] = g;
                                bgra[dst + x * 4 + 2] = r; bgra[dst + x * 4 + 3] = 255;
                            }
                            else
                            {
                                bool invert = (r | g | b) != 0;
                                bgra[dst + x * 4 + 0] = 0; bgra[dst + x * 4 + 1] = 0;
                                bgra[dst + x * 4 + 2] = invert ? (byte)255 : (byte)0;
                                bgra[dst + x * 4 + 3] = 0;
                            }
                        }
                    }
                    break;
                }
                case POINTER_SHAPE_MONOCHROME:
                {
                    // Top half = AND mask, bottom half = XOR mask, 1 bit per pixel.
                    for (int y = 0; y < height; y++)
                    {
                        int andRow = y * info.Pitch;
                        int xorRow = (y + height) * info.Pitch;
                        int dst = y * width * 4;
                        if (xorRow + (width + 7) / 8 > data.Length) break;
                        for (int x = 0; x < width; x++)
                        {
                            int mask = 0x80 >> (x & 7);
                            bool andBit = (data[andRow + (x >> 3)] & mask) != 0;
                            bool xorBit = (data[xorRow + (x >> 3)] & mask) != 0;
                            byte b, g, r, a;
                            if (!andBit)
                            {
                                byte v = xorBit ? (byte)255 : (byte)0;      // opaque white / black
                                b = v; g = v; r = v; a = 255;
                            }
                            else if (xorBit)
                            {
                                b = 0; g = 0; r = 255; a = 0;               // invert
                            }
                            else
                            {
                                b = 0; g = 0; r = 0; a = 0;                 // transparent
                            }
                            bgra[dst + x * 4 + 0] = b; bgra[dst + x * 4 + 1] = g;
                            bgra[dst + x * 4 + 2] = r; bgra[dst + x * 4 + 3] = a;
                        }
                    }
                    break;
                }
                default:
                    return null;
            }
            return bgra;
        }

        public void ClearPendingCursor() => PendingCursorBgra = null;

        public void Dispose()
        {
            Utilities.Dispose(ref _duplication);
            Utilities.Dispose(ref _output5);
            Utilities.Dispose(ref _output);
            Utilities.Dispose(ref _device);
        }
    }
}
