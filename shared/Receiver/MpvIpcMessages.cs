using System;
using System.Collections.Generic;
using System.Globalization;
using System.Text;
using HdrCast.Common;

namespace HdrCast.Receiver
{
    public enum MpvIpcMessageKind
    {
        Event = 0,
        Reply = 1,
        Unknown = 2,
    }

    /// <summary>One line of mpv's JSON IPC protocol (DOCS/man/ipc.rst), parsed tolerantly.</summary>
    public sealed class MpvIpcMessage
    {
        public MpvIpcMessageKind Kind { get; set; } = MpvIpcMessageKind.Unknown;
        /// <summary>Event name ("property-change", "file-loaded", "end-file", "shutdown", ...).</summary>
        public string? Event { get; set; }
        /// <summary>Property name for property-change events.</summary>
        public string? Name { get; set; }
        /// <summary>Observer id for property-change events.</summary>
        public long? Id { get; set; }
        /// <summary>Property value / reply payload; null when mpv reports the property as unavailable.</summary>
        public object? Data { get; set; }
        public long? RequestId { get; set; }
        /// <summary>Reply status ("success" or an error text).</summary>
        public string? Error { get; set; }
        /// <summary>end-file reason ("eof", "stop", "quit", "error", "redirect", "unknown").</summary>
        public string? Reason { get; set; }
        public Dictionary<string, object?> Raw { get; set; } = new Dictionary<string, object?>(StringComparer.Ordinal);

        public bool IsPropertyChange => Kind == MpvIpcMessageKind.Event && Event == "property-change";
        public bool IsSuccess => Kind == MpvIpcMessageKind.Reply && (Error == null || Error == "success");

        /// <summary>Returns null for blank or non-JSON lines (mpv may interleave nothing else, but stay safe).</summary>
        public static MpvIpcMessage? TryParseLine(string? line)
        {
            if (string.IsNullOrWhiteSpace(line)) return null;
            if (!MiniJson.TryParse(line!, out object? root)) return null;
            var obj = MiniJson.AsObject(root);
            if (obj == null) return null;

            var m = new MpvIpcMessage { Raw = obj };
            if (obj.TryGetValue("event", out object? ev) && ev is string evName)
            {
                m.Kind = MpvIpcMessageKind.Event;
                m.Event = evName;
                m.Name = MiniJson.GetString(obj, "name");
                m.Id = MiniJson.AsLong(MiniJson.Get(obj, "id"));
                m.Reason = MiniJson.GetString(obj, "reason");
                obj.TryGetValue("data", out object? data);
                m.Data = data;
            }
            else if (obj.ContainsKey("error") || obj.ContainsKey("request_id"))
            {
                m.Kind = MpvIpcMessageKind.Reply;
                m.Error = MiniJson.GetString(obj, "error");
                m.RequestId = MiniJson.AsLong(MiniJson.Get(obj, "request_id"));
                obj.TryGetValue("data", out object? data);
                m.Data = data;
            }
            return m;
        }
    }

    /// <summary>Builds the JSON command lines we send to mpv.</summary>
    public static class MpvIpcCommands
    {
        public static string Command(long? requestId, params object?[] command)
        {
            var d = new Dictionary<string, object?>(StringComparer.Ordinal) { { "command", command } };
            if (requestId.HasValue) d["request_id"] = requestId.Value;
            return MiniJson.Serialize(d);
        }

        public static string ObserveProperty(long id, string name) => Command(null, "observe_property", id, name);
        public static string GetProperty(string name, long requestId) => Command(requestId, "get_property", name);
        public static string SetProperty(string name, object? value, long? requestId = null) => Command(requestId, "set_property", name, value);
        public static string CycleProperty(string name, long? requestId = null) => Command(requestId, "cycle", name);
        public static string Quit(long? requestId = null) => Command(requestId, "quit");
    }

    /// <summary>Properties the receiver observes (ids are echoed back in property-change events).</summary>
    public static class MpvObservedProperties
    {
        public const string DemuxerCacheDuration = "demuxer-cache-duration";
        public const string PausedForCache = "paused-for-cache";
        public const string FrameDropCount = "frame-drop-count";
        public const string DecoderFrameDropCount = "decoder-frame-drop-count";
        public const string EstimatedVfFps = "estimated-vf-fps";
        public const string HwdecCurrent = "hwdec-current";
        public const string VideoParams = "video-params";
        public const string VideoTargetParams = "video-target-params";
        public const string VideoBitrate = "video-bitrate";
        public const string CacheBufferingState = "cache-buffering-state";
        public const string Speed = "speed";
        public const string Fullscreen = "fullscreen";

        public static readonly KeyValuePair<long, string>[] All =
        {
            new KeyValuePair<long, string>(1, DemuxerCacheDuration),
            new KeyValuePair<long, string>(2, PausedForCache),
            new KeyValuePair<long, string>(3, FrameDropCount),
            new KeyValuePair<long, string>(4, DecoderFrameDropCount),
            new KeyValuePair<long, string>(5, EstimatedVfFps),
            new KeyValuePair<long, string>(6, HwdecCurrent),
            new KeyValuePair<long, string>(7, VideoParams),
            new KeyValuePair<long, string>(8, VideoTargetParams),
            new KeyValuePair<long, string>(9, VideoBitrate),
            new KeyValuePair<long, string>(10, CacheBufferingState),
            new KeyValuePair<long, string>(11, Speed),
            new KeyValuePair<long, string>(12, Fullscreen),
        };

        public static IEnumerable<string> ObserveCommands()
        {
            foreach (var kv in All) yield return MpvIpcCommands.ObserveProperty(kv.Key, kv.Value);
        }
    }

    /// <summary>Subset of mpv's video-params / video-target-params node we display.</summary>
    public sealed class MpvVideoParams
    {
        public string? PixelFormat { get; set; }
        public long? Width { get; set; }
        public long? Height { get; set; }
        public string? ColorMatrix { get; set; }
        public string? Primaries { get; set; }
        public string? Gamma { get; set; }
        public string? ColorLevels { get; set; }
        public double? MaxLuma { get; set; }
        public double? MinLuma { get; set; }
        public double? SigPeak { get; set; }

        public static MpvVideoParams? FromNode(object? node)
        {
            var d = MiniJson.AsObject(node);
            if (d == null) return null;
            return new MpvVideoParams
            {
                PixelFormat = MiniJson.GetString(d, "pixelformat"),
                Width = MiniJson.AsLong(MiniJson.Get(d, "w")),
                Height = MiniJson.AsLong(MiniJson.Get(d, "h")),
                ColorMatrix = MiniJson.GetString(d, "colormatrix"),
                Primaries = MiniJson.GetString(d, "primaries"),
                Gamma = MiniJson.GetString(d, "gamma"),
                ColorLevels = MiniJson.GetString(d, "colorlevels"),
                MaxLuma = MiniJson.AsDouble(MiniJson.Get(d, "max-luma")),
                MinLuma = MiniJson.AsDouble(MiniJson.Get(d, "min-luma")),
                SigPeak = MiniJson.AsDouble(MiniJson.Get(d, "sig-peak")),
            };
        }

        /// <summary>True when the transfer is PQ or HLG (HDR content / HDR output).</summary>
        public bool IsHdr => Gamma == "pq" || Gamma == "hlg";

        /// <summary>"bt.2020 / pq / bt.2020-ncl / p010 3840x2160 / peak 1000 nit"</summary>
        public string Describe()
        {
            var parts = new List<string>();
            if (Primaries != null) parts.Add(Primaries);
            if (Gamma != null) parts.Add(Gamma);
            if (ColorMatrix != null) parts.Add(ColorMatrix);
            if (PixelFormat != null) parts.Add(PixelFormat);
            if (Width.HasValue && Height.HasValue) parts.Add(Width.Value + "x" + Height.Value);
            if (MaxLuma.HasValue) parts.Add("peak " + MaxLuma.Value.ToString("0", CultureInfo.InvariantCulture) + " nit");
            else if (SigPeak.HasValue) parts.Add("sig-peak " + SigPeak.Value.ToString("0.##", CultureInfo.InvariantCulture));
            return parts.Count == 0 ? "-" : string.Join(" / ", parts.ToArray());
        }

        public override string ToString() => Describe();
    }

    /// <summary>Aggregated live state built from property-change events.</summary>
    public sealed class MpvStatusModel
    {
        public double? CacheSeconds { get; set; }
        public bool PausedForCache { get; set; }
        public long? BufferingPercent { get; set; }
        public long FrameDrops { get; set; }
        public long DecoderFrameDrops { get; set; }
        public double? Fps { get; set; }
        public string? HwDec { get; set; }
        public MpvVideoParams? Source { get; set; }
        public MpvVideoParams? Target { get; set; }
        /// <summary>Bits per second as reported by mpv's video-bitrate.</summary>
        public double? VideoBitrateBps { get; set; }
        public double Speed { get; set; } = 1.0;
        public bool Fullscreen { get; set; }
        public DateTime? LastUpdateUtc { get; set; }

        public void Reset()
        {
            CacheSeconds = null;
            PausedForCache = false;
            BufferingPercent = null;
            FrameDrops = 0;
            DecoderFrameDrops = 0;
            Fps = null;
            HwDec = null;
            Source = null;
            Target = null;
            VideoBitrateBps = null;
            Speed = 1.0;
            Fullscreen = false;
            LastUpdateUtc = null;
        }

        /// <summary>Applies a property-change message; returns false for non-property or unknown messages.</summary>
        public bool Apply(MpvIpcMessage msg)
        {
            if (msg == null || !msg.IsPropertyChange || msg.Name == null) return false;
            return Apply(msg.Name, msg.Data);
        }

        public bool Apply(string name, object? data)
        {
            switch (name)
            {
                case MpvObservedProperties.DemuxerCacheDuration: CacheSeconds = MiniJson.AsDouble(data); break;
                case MpvObservedProperties.PausedForCache: PausedForCache = MiniJson.AsBool(data) ?? false; break;
                case MpvObservedProperties.FrameDropCount: FrameDrops = MiniJson.AsLong(data) ?? 0; break;
                case MpvObservedProperties.DecoderFrameDropCount: DecoderFrameDrops = MiniJson.AsLong(data) ?? 0; break;
                case MpvObservedProperties.EstimatedVfFps: Fps = MiniJson.AsDouble(data); break;
                case MpvObservedProperties.HwdecCurrent: HwDec = data as string; break;
                case MpvObservedProperties.VideoParams: Source = MpvVideoParams.FromNode(data); break;
                case MpvObservedProperties.VideoTargetParams: Target = MpvVideoParams.FromNode(data); break;
                case MpvObservedProperties.VideoBitrate: VideoBitrateBps = MiniJson.AsDouble(data); break;
                case MpvObservedProperties.CacheBufferingState: BufferingPercent = MiniJson.AsLong(data); break;
                case MpvObservedProperties.Speed: Speed = MiniJson.AsDouble(data) ?? 1.0; break;
                case MpvObservedProperties.Fullscreen: Fullscreen = MiniJson.AsBool(data) ?? false; break;
                default: return false;
            }
            LastUpdateUtc = DateTime.UtcNow;
            return true;
        }

        public string DescribeBitrate()
        {
            if (!VideoBitrateBps.HasValue) return "-";
            return (VideoBitrateBps.Value / 1_000_000.0).ToString("0.0", CultureInfo.InvariantCulture) + " Mbps";
        }

        public string DescribeCache()
        {
            var sb = new StringBuilder();
            sb.Append(CacheSeconds.HasValue ? CacheSeconds.Value.ToString("0.00", CultureInfo.InvariantCulture) + " s" : "-");
            if (PausedForCache)
            {
                sb.Append("  (buffering");
                if (BufferingPercent.HasValue) sb.Append(' ').Append(BufferingPercent.Value).Append('%');
                sb.Append(')');
            }
            if (Math.Abs(Speed - 1.0) > 0.001) sb.Append("  speed ").Append(Speed.ToString("0.00", CultureInfo.InvariantCulture)).Append('x');
            return sb.ToString();
        }
    }
}
