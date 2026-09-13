using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Runtime.Serialization;
using System.Runtime.Serialization.Json;
using System.Text;

namespace HdrCast.Capture
{
    /// <summary>
    /// One DXGI output as reported by <c>HdrCastSender.exe list-outputs --json</c> (design.md §2.1).
    /// Serialised with DataContractJsonSerializer so the GUI can parse it without extra packages.
    /// </summary>
    [DataContract]
    public sealed class OutputInfo
    {
        /// <summary>Global index (EnumAdapters1 → EnumOutputs order); what <c>capture --output</c> takes.</summary>
        [DataMember(Name = "index", Order = 0)] public int Index { get; set; }
        [DataMember(Name = "adapterIndex", Order = 1)] public int AdapterIndex { get; set; }
        [DataMember(Name = "outputIndexOnAdapter", Order = 2)] public int OutputIndexOnAdapter { get; set; }
        [DataMember(Name = "adapter", Order = 3)] public string Adapter { get; set; } = "";
        /// <summary>GDI device name, e.g. \\.\DISPLAY1.</summary>
        [DataMember(Name = "deviceName", Order = 4)] public string DeviceName { get; set; } = "";
        /// <summary>Monitor friendly name from the CCD API (may be empty).</summary>
        [DataMember(Name = "friendlyName", Order = 5)] public string FriendlyName { get; set; } = "";
        [DataMember(Name = "width", Order = 6)] public int Width { get; set; }
        [DataMember(Name = "height", Order = 7)] public int Height { get; set; }
        [DataMember(Name = "x", Order = 8)] public int X { get; set; }
        [DataMember(Name = "y", Order = 9)] public int Y { get; set; }
        /// <summary>DXGI_MODE_ROTATION: 0 unspecified, 1 identity, 2 = 90°, 3 = 180°, 4 = 270°.</summary>
        [DataMember(Name = "rotation", Order = 10)] public int Rotation { get; set; }
        [DataMember(Name = "refreshRate", Order = 11)] public double RefreshRate { get; set; }
        [DataMember(Name = "attachedToDesktop", Order = 12)] public bool AttachedToDesktop { get; set; }
        /// <summary>DXGI_COLOR_SPACE_TYPE numeric value (12 = RGB_FULL_G2084_NONE_P2020 = HDR on).</summary>
        [DataMember(Name = "colorSpace", Order = 13)] public int ColorSpace { get; set; }
        [DataMember(Name = "hdr", Order = 14)] public bool Hdr { get; set; }
        [DataMember(Name = "bitsPerColor", Order = 15)] public int BitsPerColor { get; set; }
        [DataMember(Name = "minLuminance", Order = 16)] public float MinLuminance { get; set; }
        [DataMember(Name = "maxLuminance", Order = 17)] public float MaxLuminance { get; set; }
        [DataMember(Name = "maxFullFrameLuminance", Order = 18)] public float MaxFullFrameLuminance { get; set; }

        /// <summary>DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.</summary>
        public const int ColorSpaceHdr10 = 12;

        public string RotationName
        {
            get
            {
                switch (Rotation)
                {
                    case 1: return "0";
                    case 2: return "90";
                    case 3: return "180";
                    case 4: return "270";
                    default: return "?";
                }
            }
        }

        /// <summary>True when the desktop image is rotated by 90° or 270° (the duplicated texture is W/H swapped).</summary>
        public bool IsRotated90 => Rotation == 2 || Rotation == 4;

        /// <summary>One-line label for combo boxes: "0: Dell AW3225QF (\\.\DISPLAY1) 3840x2160@240 HDR".</summary>
        public string Label
        {
            get
            {
                var sb = new StringBuilder();
                sb.Append(Index).Append(": ");
                sb.Append(FriendlyName.Length > 0 ? FriendlyName : "Display");
                sb.Append(" (").Append(DeviceName).Append(") ");
                sb.Append(Width).Append('x').Append(Height);
                if (RefreshRate > 0) sb.Append('@').Append(RefreshRate.ToString("0.#", CultureInfo.InvariantCulture));
                sb.Append(Hdr ? " HDR" : " SDR");
                if (Hdr && MaxLuminance > 0) sb.Append(' ').Append(MaxLuminance.ToString("0", CultureInfo.InvariantCulture)).Append("nit");
                return sb.ToString();
            }
        }

        public static string ToJson(IList<OutputInfo> outputs)
        {
            var ser = new DataContractJsonSerializer(typeof(List<OutputInfo>));
            using (var ms = new MemoryStream())
            {
                ser.WriteObject(ms, new List<OutputInfo>(outputs));
                return Encoding.UTF8.GetString(ms.ToArray());
            }
        }

        public static List<OutputInfo> FromJson(string json)
        {
            var ser = new DataContractJsonSerializer(typeof(List<OutputInfo>));
            using (var ms = new MemoryStream(Encoding.UTF8.GetBytes(json)))
            {
                return (List<OutputInfo>?)ser.ReadObject(ms) ?? new List<OutputInfo>();
            }
        }

        /// <summary>Human-readable table for <c>list-outputs</c> without --json.</summary>
        public static string ToTable(IList<OutputInfo> outputs)
        {
            var sb = new StringBuilder();
            sb.AppendLine("idx  device         name                      size        pos          hz     hdr  colorspace  bits  maxnit  maxffl  minnit  adapter");
            foreach (var o in outputs)
            {
                sb.Append(o.Index.ToString(CultureInfo.InvariantCulture).PadRight(5));
                sb.Append(Pad(o.DeviceName, 15));
                sb.Append(Pad(o.FriendlyName, 26));
                sb.Append(Pad(o.Width + "x" + o.Height + (o.IsRotated90 ? " r" + o.RotationName : ""), 12));
                sb.Append(Pad(o.X + "," + o.Y, 13));
                sb.Append(Pad(o.RefreshRate.ToString("0.##", CultureInfo.InvariantCulture), 7));
                sb.Append(Pad(o.Hdr ? "on" : "off", 5));
                sb.Append(Pad(o.ColorSpace.ToString(CultureInfo.InvariantCulture), 12));
                sb.Append(Pad(o.BitsPerColor.ToString(CultureInfo.InvariantCulture), 6));
                sb.Append(Pad(o.MaxLuminance.ToString("0.#", CultureInfo.InvariantCulture), 8));
                sb.Append(Pad(o.MaxFullFrameLuminance.ToString("0.#", CultureInfo.InvariantCulture), 8));
                sb.Append(Pad(o.MinLuminance.ToString("0.####", CultureInfo.InvariantCulture), 8));
                sb.Append(o.Adapter);
                sb.AppendLine();
            }
            return sb.ToString();
        }

        static string Pad(string s, int width)
        {
            if (s.Length >= width) return s.Substring(0, width - 1) + " ";
            return s.PadRight(width);
        }
    }
}
