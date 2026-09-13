using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// Windows CCD (Connecting and Configuring Displays) queries: maps a GDI device name (\\.\DISPLAY1) to the
    /// monitor's friendly name and refresh rate. Structures ported from the project's virtual-display/DisplayApi.cs.
    /// </summary>
    internal static class DisplayConfig
    {
        const uint QDC_ONLY_ACTIVE_PATHS = 0x2;
        const uint DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME = 1;
        const uint DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME = 2;
        const int ERROR_SUCCESS = 0;
        const int ERROR_INSUFFICIENT_BUFFER = 122;

        [StructLayout(LayoutKind.Sequential)]
        struct LUID { public uint LowPart; public int HighPart; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_RATIONAL { public uint Numerator; public uint Denominator; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_PATH_SOURCE_INFO { public LUID adapterId; public uint id; public uint modeInfoIdx; public uint statusFlags; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_PATH_TARGET_INFO
        {
            public LUID adapterId;
            public uint id;
            public uint modeInfoIdx;
            public uint outputTechnology;
            public uint rotation;
            public uint scaling;
            public DISPLAYCONFIG_RATIONAL refreshRate;
            public uint scanLineOrdering;
            [MarshalAs(UnmanagedType.Bool)] public bool targetAvailable;
            public uint statusFlags;
        }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_PATH_INFO
        {
            public DISPLAYCONFIG_PATH_SOURCE_INFO sourceInfo;
            public DISPLAYCONFIG_PATH_TARGET_INFO targetInfo;
            public uint flags;
        }

        [StructLayout(LayoutKind.Sequential)]
        struct POINTL { public int x; public int y; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_2DREGION { public uint cx; public uint cy; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_VIDEO_SIGNAL_INFO
        {
            public ulong pixelRate;
            public DISPLAYCONFIG_RATIONAL hSyncFreq;
            public DISPLAYCONFIG_RATIONAL vSyncFreq;
            public DISPLAYCONFIG_2DREGION activeSize;
            public DISPLAYCONFIG_2DREGION totalSize;
            public uint videoStandard;
            public uint scanLineOrdering;
        }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_TARGET_MODE { public DISPLAYCONFIG_VIDEO_SIGNAL_INFO targetVideoSignalInfo; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_SOURCE_MODE { public uint width; public uint height; public uint pixelFormat; public POINTL position; }

        [StructLayout(LayoutKind.Sequential)]
        struct RECTL { public int left; public int top; public int right; public int bottom; }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_DESKTOP_IMAGE_INFO { public POINTL PathSourceSize; public RECTL DesktopImageRegion; public RECTL DesktopImageClip; }

        // infoType(4) + id(4) + adapterId(8), then a 48-byte union.
        [StructLayout(LayoutKind.Explicit)]
        struct DISPLAYCONFIG_MODE_INFO
        {
            [FieldOffset(0)] public uint infoType;
            [FieldOffset(4)] public uint id;
            [FieldOffset(8)] public LUID adapterId;
            [FieldOffset(16)] public DISPLAYCONFIG_TARGET_MODE targetMode;
            [FieldOffset(16)] public DISPLAYCONFIG_SOURCE_MODE sourceMode;
            [FieldOffset(16)] public DISPLAYCONFIG_DESKTOP_IMAGE_INFO desktopImageInfo;
        }

        [StructLayout(LayoutKind.Sequential)]
        struct DISPLAYCONFIG_DEVICE_INFO_HEADER { public uint type; public uint size; public LUID adapterId; public uint id; }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        struct DISPLAYCONFIG_TARGET_DEVICE_NAME
        {
            public DISPLAYCONFIG_DEVICE_INFO_HEADER header;
            public uint flags;
            public uint outputTechnology;
            public ushort edidManufactureId;
            public ushort edidProductCodeId;
            public uint connectorInstance;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string monitorFriendlyDeviceName;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string monitorDevicePath;
        }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        struct DISPLAYCONFIG_SOURCE_DEVICE_NAME
        {
            public DISPLAYCONFIG_DEVICE_INFO_HEADER header;
            [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string viewGdiDeviceName;
        }

        [DllImport("user32.dll")]
        static extern int GetDisplayConfigBufferSizes(uint flags, out uint numPathArrayElements, out uint numModeInfoArrayElements);

        [DllImport("user32.dll")]
        static extern int QueryDisplayConfig(uint flags,
            ref uint numPathArrayElements, [In, Out] DISPLAYCONFIG_PATH_INFO[] pathArray,
            ref uint numModeInfoArrayElements, [In, Out] DISPLAYCONFIG_MODE_INFO[] modeInfoArray,
            IntPtr currentTopologyId);

        [DllImport("user32.dll")]
        static extern int DisplayConfigGetDeviceInfo(ref DISPLAYCONFIG_TARGET_DEVICE_NAME requestPacket);

        [DllImport("user32.dll")]
        static extern int DisplayConfigGetDeviceInfo(ref DISPLAYCONFIG_SOURCE_DEVICE_NAME requestPacket);

        /// <summary>What CCD knows about one active display path.</summary>
        internal sealed class MonitorEntry
        {
            public string GdiDeviceName = "";
            public string FriendlyName = "";
            public double RefreshRate;
        }

        /// <summary>
        /// Returns the active display paths keyed by GDI device name. Never throws: on any failure the caller
        /// simply gets fewer (or no) friendly names, which only affects labels.
        /// </summary>
        public static Dictionary<string, MonitorEntry> QueryActiveMonitors()
        {
            var map = new Dictionary<string, MonitorEntry>(StringComparer.OrdinalIgnoreCase);
            try
            {
                DISPLAYCONFIG_PATH_INFO[] paths;
                DISPLAYCONFIG_MODE_INFO[] modes;
                if (!TryQuery(out paths, out modes)) return map;

                foreach (DISPLAYCONFIG_PATH_INFO path in paths)
                {
                    var src = new DISPLAYCONFIG_SOURCE_DEVICE_NAME();
                    src.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
                    src.header.size = (uint)Marshal.SizeOf(typeof(DISPLAYCONFIG_SOURCE_DEVICE_NAME));
                    src.header.adapterId = path.sourceInfo.adapterId;
                    src.header.id = path.sourceInfo.id;
                    if (DisplayConfigGetDeviceInfo(ref src) != ERROR_SUCCESS) continue;
                    string gdi = src.viewGdiDeviceName ?? "";
                    if (gdi.Length == 0) continue;

                    var entry = new MonitorEntry { GdiDeviceName = gdi };

                    var tgt = new DISPLAYCONFIG_TARGET_DEVICE_NAME();
                    tgt.header.type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
                    tgt.header.size = (uint)Marshal.SizeOf(typeof(DISPLAYCONFIG_TARGET_DEVICE_NAME));
                    tgt.header.adapterId = path.targetInfo.adapterId;
                    tgt.header.id = path.targetInfo.id;
                    if (DisplayConfigGetDeviceInfo(ref tgt) == ERROR_SUCCESS)
                        entry.FriendlyName = (tgt.monitorFriendlyDeviceName ?? "").Trim();

                    if (path.targetInfo.refreshRate.Denominator != 0)
                        entry.RefreshRate = (double)path.targetInfo.refreshRate.Numerator / path.targetInfo.refreshRate.Denominator;

                    map[gdi] = entry;
                }
            }
            catch (Exception)
            {
                // DllNotFound / EntryPointNotFound on a non-Windows host: labels stay empty.
            }
            return map;
        }

        static bool TryQuery(out DISPLAYCONFIG_PATH_INFO[] paths, out DISPLAYCONFIG_MODE_INFO[] modes)
        {
            paths = new DISPLAYCONFIG_PATH_INFO[0];
            modes = new DISPLAYCONFIG_MODE_INFO[0];
            // The display set can change between the size query and the data query; retry a few times.
            for (int attempt = 0; attempt < 5; attempt++)
            {
                uint nPaths, nModes;
                if (GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, out nPaths, out nModes) != ERROR_SUCCESS) return false;
                paths = new DISPLAYCONFIG_PATH_INFO[nPaths];
                modes = new DISPLAYCONFIG_MODE_INFO[nModes];
                int err = QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, ref nPaths, paths, ref nModes, modes, IntPtr.Zero);
                if (err == ERROR_INSUFFICIENT_BUFFER) continue;
                if (err != ERROR_SUCCESS) return false;
                if (nPaths < paths.Length) Array.Resize(ref paths, (int)nPaths);
                if (nModes < modes.Length) Array.Resize(ref modes, (int)nModes);
                return true;
            }
            return false;
        }
    }
}
