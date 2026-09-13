using System;
using System.Collections.Generic;
using HdrCast.Capture;
using SharpDX.DXGI;

namespace HdrCast.Sender.Capture
{
    /// <summary>
    /// Enumerates DXGI outputs in the exact order the capture engine indexes them:
    /// EnumAdapters1 → EnumOutputs, flattened. That index is what <c>capture --output</c> takes.
    /// </summary>
    internal static class OutputEnumerator
    {
        /// <summary>DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 — the desktop is in HDR mode.</summary>
        public const int ColorSpaceHdr10 = OutputInfo.ColorSpaceHdr10;

        public static List<OutputInfo> Enumerate()
        {
            var result = new List<OutputInfo>();
            Dictionary<string, DisplayConfig.MonitorEntry> ccd = DisplayConfig.QueryActiveMonitors();

            using (var factory = new Factory1())
            {
                int adapterCount = factory.GetAdapterCount1();
                int index = 0;
                for (int ai = 0; ai < adapterCount; ai++)
                {
                    Adapter1 adapter;
                    try { adapter = factory.GetAdapter1(ai); }
                    catch (Exception) { continue; }
                    using (adapter)
                    {
                        string adapterName;
                        try { adapterName = (adapter.Description1.Description ?? "").Trim('\0').Trim(); }
                        catch (Exception) { adapterName = "adapter " + ai; }

                        int outputCount;
                        try { outputCount = adapter.GetOutputCount(); }
                        catch (Exception) { outputCount = 0; }

                        for (int oi = 0; oi < outputCount; oi++)
                        {
                            Output output;
                            try { output = adapter.GetOutput(oi); }
                            catch (Exception) { continue; }
                            using (output)
                            {
                                OutputInfo info = Describe(output, adapterName, ai, oi, index, ccd);
                                result.Add(info);
                                index++;
                            }
                        }
                    }
                }
            }
            return result;
        }

        static OutputInfo Describe(Output output, string adapterName, int adapterIndex, int outputIndexOnAdapter,
                                   int globalIndex, Dictionary<string, DisplayConfig.MonitorEntry> ccd)
        {
            OutputDescription d = output.Description;
            string device = (d.DeviceName ?? "").Trim('\0');
            var info = new OutputInfo
            {
                Index = globalIndex,
                AdapterIndex = adapterIndex,
                OutputIndexOnAdapter = outputIndexOnAdapter,
                Adapter = adapterName,
                DeviceName = device,
                Width = d.DesktopBounds.Right - d.DesktopBounds.Left,
                Height = d.DesktopBounds.Bottom - d.DesktopBounds.Top,
                X = d.DesktopBounds.Left,
                Y = d.DesktopBounds.Top,
                Rotation = (int)d.Rotation,
                AttachedToDesktop = d.IsAttachedToDesktop,
            };

            DisplayConfig.MonitorEntry? entry;
            if (ccd.TryGetValue(device, out entry) && entry != null)
            {
                info.FriendlyName = entry.FriendlyName;
                info.RefreshRate = entry.RefreshRate;
            }

            // Output6 (Windows 10 1703+) carries the colour space and luminance of the current desktop mode.
            try
            {
                using (Output6 o6 = output.QueryInterface<Output6>())
                {
                    OutputDescription1 d1 = o6.Description1;
                    info.ColorSpace = (int)d1.ColorSpace;
                    info.Hdr = info.ColorSpace == ColorSpaceHdr10;
                    info.BitsPerColor = d1.BitsPerColor;
                    info.MinLuminance = d1.MinLuminance;
                    info.MaxLuminance = d1.MaxLuminance;
                    info.MaxFullFrameLuminance = d1.MaxFullFrameLuminance;
                }
            }
            catch (Exception)
            {
                info.ColorSpace = -1;   // IDXGIOutput6 unavailable: treat the desktop as SDR
            }
            return info;
        }

        /// <summary>Finds the output for a saved configuration: by index, falling back to the remembered names.</summary>
        public static OutputInfo? Resolve(List<OutputInfo> outputs, int index, string deviceName, string friendlyName)
        {
            if (outputs == null || outputs.Count == 0) return null;
            foreach (OutputInfo o in outputs)
            {
                if (o.Index == index &&
                    (string.IsNullOrEmpty(deviceName) || string.Equals(o.DeviceName, deviceName, StringComparison.OrdinalIgnoreCase)))
                    return o;
            }
            if (!string.IsNullOrEmpty(deviceName))
                foreach (OutputInfo o in outputs)
                    if (string.Equals(o.DeviceName, deviceName, StringComparison.OrdinalIgnoreCase)) return o;
            if (!string.IsNullOrEmpty(friendlyName))
                foreach (OutputInfo o in outputs)
                    if (string.Equals(o.FriendlyName, friendlyName, StringComparison.OrdinalIgnoreCase)) return o;
            foreach (OutputInfo o in outputs)
                if (o.Index == index) return o;
            return outputs[0];
        }
    }
}
